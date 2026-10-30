#![cfg(feature = "use_reth")]

// General imports
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use parking_lot::RwLock;
use rayon::prelude::*;
use std::time::Duration;

// Reth imports
use alloy_primitives::{Address, B256, U256, keccak256};
use reth_db::tables;
use reth_db_api::cursor::{DbCursorRO, DbDupCursorRO};
use reth_db_api::transaction::DbTx;
use reth_node_types::NodeTypesWithDBAdapter;
use reth_primitives::Account;
use reth_provider::{ProviderFactory, StateChangeWriter, TrieWriter};
use reth_provider::providers::{StaticFileProvider, ConsistentDbView};
use reth_provider::test_utils::MockNodeTypes;
use reth_trie::TrieInput;
use reth_trie::{HashedPostState, HashedStorage};
use reth_trie::updates::TrieUpdates;
use reth_trie_parallel::parallel_root::ParallelStateRoot;

use reth_db::mdbx::MaxReadTransactionDuration;
use reth_db::mdbx::DatabaseArguments;

// QMDB imports
use qmdb_common::task::task::Task;
use qmdb::test_helper::SimpleTask;
use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_WRITE, OP_READ};

type BenchNode = NodeTypesWithDBAdapter<MockNodeTypes, Arc<reth_db::DatabaseEnv>>;

/// In-memory pool that accumulates state across blocks before writing to DB.
///
/// BATCH_SIZE=1 means every block triggers a DB write. Larger values amortize
/// write overhead but increase memory usage and change commit semantics.
///
/// Note on trie caching: `trie_updates` are kept in memory across blocks within a
/// batch. On the N-th block of the batch they are passed to `TrieInput::nodes` so
/// `InMemoryTrieCursorFactory` can serve trie node reads from memory instead of DB.
/// With BATCH_SIZE=1 this cache is always empty (reset every block), so every block
/// reads all trie nodes from DB. Increasing BATCH_SIZE improves cache hit rates for
/// workloads with locality.
#[derive(Default)]
pub struct BatchPool {
    pub hashed_state: HashedPostState,
    pub trie_updates: TrieUpdates,
    pub block_count: usize,
}

/// True when running ETH replay mode (real Ethereum traces).
/// False for synthetic benchmark (all keys are uniform 52-byte, treated as accounts).
static IS_ETH_MODE: AtomicBool = AtomicBool::new(false);

lazy_static::lazy_static! {
    static ref RETH_FACTORY: RwLock<Option<ProviderFactory<BenchNode>>> = RwLock::new(None);
    static ref BATCH_POOL: RwLock<BatchPool> = RwLock::new(BatchPool::default());
}

/// Number of blocks to accumulate before flushing to DB.
/// =1 → commit every block (simplest, no in-memory trie caching benefit).
/// >1 → amortises MDBX write-transaction overhead and enables trie node caching.
const BATCH_SIZE: usize = 1;

/// Parse a big-endian U256 from a variable-length byte slice (up to 32 bytes).
fn parse_u256(value: &[u8]) -> U256 {
    if value.is_empty() {
        return U256::ZERO;
    }
    let len = value.len().min(32);
    let mut padded = [0u8; 32];
    padded[32 - len..].copy_from_slice(&value[..len]);
    U256::from_be_bytes(padded)
}

pub fn init(db_dir: &str, is_eth_mode: bool) {
    IS_ETH_MODE.store(is_eth_mode, Ordering::Relaxed);
    let path = Path::new(db_dir);

    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("Failed to create RETH dir");

    let db_args = DatabaseArguments::default().with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Set(Duration::from_secs(36000))));
    let db = Arc::new(reth_db::init_db(path, db_args).expect("Failed to init MDBX"));

    let static_file_path = path.join("static_files");
    std::fs::create_dir_all(&static_file_path).unwrap();
    let static_file_provider =
        StaticFileProvider::read_write(static_file_path).expect("Failed to init StaticFileProvider");

    let factory = ProviderFactory::<BenchNode>::new(db, Default::default(), static_file_provider);
    *RETH_FACTORY.write() = Some(factory);

    let db_path = Path::new(db_dir).join("mdbx.dat");
    println!(
        "[INIT] Reth adapter ready. db={:?} is_eth_mode={} BATCH_SIZE={}",
        db_path, is_eth_mode, BATCH_SIZE
    );
}

/// Core block-processing function.
///
/// Execution is split into three phases to minimise lock contention:
///
///   Phase 1 – Parallel hashed-state computation (no lock).
///     Each rayon thread opens its own read-only provider and computes the
///     `HashedPostState` contribution for its tasks.
///
///   Phase 2 – Pool accumulation (brief write lock).
///     Merge the per-thread states into the pool. Extract everything needed for
///     the trie computation (clones / take), then release the lock immediately.
///
///   Phase 3 – Trie root computation (no lock).
///     `ParallelStateRoot` spawns storage-root tasks via rayon and walks the
///     account trie. This is the heaviest operation and runs lock-free.
///
///   Phase 4 – Write-back (brief write lock, only every BATCH_SIZE blocks).
///     Extend accumulated trie updates, then flush hashed state + trie nodes to
///     MDBX in a single write transaction and reset the pool.
///
/// Design notes on `ParallelStateRoot` vs `StateRoot`:
///   `ParallelStateRoot` is always used because the ETH replay mode touches both
///   account and storage tries.  For the synthetic (account-only) workload the
///   storage-root subtasks resolve immediately (empty storage → EMPTY_ROOT_HASH),
///   adding some rayon-dispatch overhead but keeping the code path uniform.
pub fn execute_batch(_block_height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    let factory_guard = RETH_FACTORY.read();
    let factory = factory_guard.as_ref().unwrap();

    // Phase 1: Parallel per-thread HashedPostState computation
    let local_hashed_state = task_list
        .into_par_iter()
        .fold(
            || {
                let provider = factory.provider().expect("Failed to get RO provider");
                (HashedPostState::default(), provider)
            },
            |mut acc, task_lock| {
                let (ref mut local_state, ref provider) = acc;
                if let Some(task) = task_lock.read().as_ref() {
                    let tx = provider.tx_ref();
                    let mut acc_cursor =
                        tx.cursor_read::<tables::HashedAccounts>().expect("cursor_read HashedAccounts");
                    let mut stg_cursor =
                        tx.cursor_dup_read::<tables::HashedStorages>().expect("cursor_dup_read HashedStorages");

                    let eth_mode = IS_ETH_MODE.load(Ordering::Relaxed);
                    for cset in task.get_change_sets().iter() {
                        cset.for_each_op(|op_type, _, _, key, value, _, _| {
                            if eth_mode {
                                // ETH replay: key structure determines op type.
                                // 52-byte key = 20-byte address + 32-byte slot → storage op.
                                // 20-byte key = address only → account op.
                                if key.len() >= 52 {
                                    let address = Address::from_slice(&key[0..20]);
                                    let slot = B256::from_slice(&key[20..52]);
                                    let hashed_address = keccak256(address);
                                    let hashed_slot = keccak256(slot);

                                    match op_type {
                                        OP_WRITE | OP_CREATE => {
                                            local_state
                                                .storages
                                                .entry(hashed_address)
                                                .or_insert_with(|| HashedStorage::new(false))
                                                .storage
                                                .insert(hashed_slot, parse_u256(value));
                                        }
                                        OP_DELETE => {
                                            local_state
                                                .storages
                                                .entry(hashed_address)
                                                .or_insert_with(|| HashedStorage::new(false))
                                                .storage
                                                .insert(hashed_slot, U256::ZERO);
                                        }
                                        OP_READ => {
                                            let _ = stg_cursor
                                                .seek_by_key_subkey(hashed_address, hashed_slot)
                                                .unwrap_or(None);
                                        }
                                        _ => {}
                                    }
                                } else if key.len() >= 20 {
                                    let address = Address::from_slice(&key[0..20]);
                                    let hashed_address = keccak256(address);

                                    match op_type {
                                        OP_WRITE | OP_CREATE => {
                                            let account = Account {
                                                nonce: 1,
                                                balance: parse_u256(value),
                                                bytecode_hash: None,
                                            };
                                            local_state.accounts.insert(hashed_address, Some(account));
                                        }
                                        OP_DELETE => {
                                            local_state.accounts.insert(hashed_address, None);
                                        }
                                        OP_READ => {
                                            let _ = acc_cursor
                                                .seek_exact(hashed_address)
                                                .unwrap_or(None);
                                        }
                                        _ => {}
                                    }
                                }
                            } else {
                                // Synthetic benchmark: treat each 52-byte key as an account
                                // operation, using the first 20 bytes as the account address.
                                let address = Address::from_slice(&key[0..20]);
                                let hashed_address = keccak256(address);

                                match op_type {
                                    OP_WRITE | OP_CREATE => {
                                        let account = Account {
                                            nonce: 1,
                                            balance: parse_u256(value),
                                            bytecode_hash: None,
                                        };
                                        local_state.accounts.insert(hashed_address, Some(account));
                                    }
                                    OP_DELETE => {
                                        local_state.accounts.insert(hashed_address, None);
                                    }
                                    OP_READ => {
                                        let _ = acc_cursor
                                            .seek_exact(hashed_address)
                                            .unwrap_or(None);
                                    }
                                    _ => {}
                                }
                            }
                        });
                    }
                }
                acc
            },
        )
        .map(|(state, _provider)| state)
        .reduce(
            || HashedPostState::default(),
            |mut a, b| {
                a.extend(b);
                a
            },
        );

    // Phase 2: Accumulate into pool, extract trie-input snapshot
    // (write lock held only for this brief section)
    let (trie_input, should_write) = {
        let mut pool = BATCH_POOL.write();
        pool.hashed_state.extend(local_hashed_state);
        pool.block_count += 1;
        let should_write = pool.block_count >= BATCH_SIZE;

        // Build prefix sets from the full accumulated state.
        // prefix_sets tells the trie walker which paths changed and must be re-traversed.
        // Keys absent from the prefix set reuse cached intermediate nodes from `nodes`.
        let prefix_sets = pool.hashed_state.construct_prefix_sets();

        // Clone hashed_state and trie_updates for the TrieInput.
        // For BATCH_SIZE=1 `trie_updates` is always empty here (reset every block),
        // so there is no in-memory trie node cache.  Larger BATCH_SIZE values
        // accumulate nodes across blocks and reduce DB reads.
        let trie_input = TrieInput::new(
            pool.trie_updates.clone(),
            pool.hashed_state.clone(),
            prefix_sets,
        );

        (trie_input, should_write)
    }; // Pool write lock released here — Phase 3 runs without holding it.

    // Phase 3: Heavy trie root computation (lock-free)
    // `ConsistentDbView` pins a consistent DB snapshot for the duration of
    // the parallel storage-root tasks and the account-trie walk.
    let view = ConsistentDbView::new(factory.clone(), None);
    let (_, current_trie_updates) = ParallelStateRoot::new(view, trie_input)
        .incremental_root_with_updates()
        .expect("Failed to calculate parallel state root with updates");

    // Phase 4: Extend trie updates; flush to DB when batch full
    // (write lock held only for this brief section)
    {
        let mut pool = BATCH_POOL.write();
        pool.trie_updates.extend(current_trie_updates);

        if should_write {
            // Compute sorted representation only when we actually need to write.
            // `into_sorted()` separates destroyed accounts and zero-valued storage
            // slots for efficient cursor-based insertion/deletion in MDBX.
            let sorted_state = pool.hashed_state.clone().into_sorted();

            let provider = factory.provider_rw().unwrap();
            provider.write_hashed_state(&sorted_state).expect("Failed to write hashed state");
            provider.write_trie_updates(&pool.trie_updates).expect("Failed to write trie updates");
            provider.commit().unwrap();

            // Reset pool for the next batch.
            pool.hashed_state = HashedPostState::default();
            pool.trie_updates = TrieUpdates::default();
            pool.block_count = 0;
        }
    }
}

pub fn create_kv(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    execute_batch(height, task_list);
}
pub fn update_kv(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    execute_batch(height, task_list);
}
pub fn delete_kv(height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    execute_batch(height, task_list);
}

/// Read-only benchmark path.
/// Reads are served directly from `HashedAccounts` / `HashedStorages` tables,
/// which store keccak-hashed keys and are the canonical source of truth for
/// state root computation.
pub fn read_kv(task_list: Vec<parking_lot::RwLock<Option<SimpleTask>>>) {
    let guard = RETH_FACTORY.read();
    let factory = guard.as_ref().unwrap();

    // Split into chunks so each rayon worker reuses the same cursors across
    // multiple tasks, amortising cursor-open overhead.
    let chunk_size = 2000;

    task_list.par_chunks(chunk_size).for_each(|chunk| {
        let provider = factory.provider().expect("Failed to get read provider");
        let tx = provider.tx_ref();

        let mut acc_cursor = tx.cursor_read::<tables::HashedAccounts>().expect("cursor");
        let mut stg_cursor = tx.cursor_dup_read::<tables::HashedStorages>().expect("cursor");

        let eth_mode = IS_ETH_MODE.load(Ordering::Relaxed);
        for task_lock in chunk {
            if let Some(task) = task_lock.read().as_ref() {
                for cset in task.get_change_sets().iter() {
                    cset.for_each_op(|op_type, _, _, key, _, _, _| {
                        if op_type == OP_READ {
                            if eth_mode {
                                if key.len() >= 52 {
                                    let address = Address::from_slice(&key[0..20]);
                                    let slot = B256::from_slice(&key[20..52]);
                                    let hashed_address = keccak256(address);
                                    let hashed_slot = keccak256(slot);
                                    let _ = stg_cursor
                                        .seek_by_key_subkey(hashed_address, hashed_slot)
                                        .unwrap_or(None);
                                } else if key.len() >= 20 {
                                    let address = Address::from_slice(&key[0..20]);
                                    let hashed_address = keccak256(address);
                                    let _ = acc_cursor.seek_exact(hashed_address).unwrap_or(None);
                                }
                            } else {
                                let address = Address::from_slice(&key[0..20]);
                                let hashed_address = keccak256(address);
                                let _ = acc_cursor.seek_exact(hashed_address).unwrap_or(None);
                            }
                        }
                    });
                }
            }
        }
    });
}

