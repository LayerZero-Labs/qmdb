use std::convert::TryFrom;
use std::sync::Arc;
use std::time::Instant;

use bench::COUNTERS;
use clap::Parser;
use log::info;
use parking_lot::RwLock;
use qmdb::metadb::MetaInfo;
use qmdb::tasks::TasksManager;
use qmdb::test_helper::SimpleTask;
use qmdb::utils::lfsr::GaloisLfsr;
use qmdb::{config::Config, AdsWrap, ADS};
use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE};
use qmdb_common::entryfile::entry::entry_key_hash;
use qmdb_common::task::taskid::join_task_id;
use qmdb_common::utils::changeset::ChangeSet;

const READ_SALT: u64 = 0x5245_4144; // "READ"
const UPDATE_SALT: u64 = 0x5550_4454; // "UPDT"
const DELETE_SALT: u64 = 0x4445_4C45; // "DELE"
const CREATE_SALT: u64 = 0x4352_4541; // "CREA"
const LFSR_BITS: usize = 63;
const LFSR_MASK: u64 = (1u64 << LFSR_BITS) - 1;

/// Simple CRUD benchmark on top of an existing QMDB initialized by `init-db`.
///
/// The benchmark performs four phases on a contiguous range of account IDs:
/// 1. Read N accounts (by integer ID, in order).
/// 2. Update the same N accounts (rewrite their values).
/// 3. Delete the same N accounts.
/// 4. Create N new accounts (with new integer IDs).
#[derive(Parser, Debug)]
struct CrudCli {
    /// Path to the QMDB data directory (the same directory passed to `Config::from_dir`).
    #[arg(long, default_value = "/tmp/QMDB_bench_sendeth")]
    db_dir: String,

    /// Starting account id (must correspond to an existing account created by init-db).
    #[arg(long, default_value_t = 1)]
    start_account_id: u64,

    /// Target number of operations per block/phase. Will be rounded down to the nearest value that
    /// fits an integer number of tasks (`changesets_per_task * ops_per_changeset`).
    #[arg(long, default_value_t = 100_000)]
    ops_per_block: u64,

    /// Number of changesets per task.
    #[arg(long, default_value_t = 2, alias = "changesets-per-txn")]
    changesets_per_task: u64,

    /// Number of operations per changeset.
    #[arg(long, default_value_t = 1024)]
    ops_per_changeset: u64,

    /// Number of CRUD block rounds to run (each round runs READ, UPDATE, DELETE, CREATE).
    #[arg(long, default_value_t = 1)]
    blocks_per_phase: u64,

    /// Number of rounds. Each round performs all four phases sequentially.
    #[arg(long, default_value_t = 1)]
    rounds: u64,

    /// Total number of accounts that were created by init-db (maximum existing account id, inclusive).
    /// We assume init-db used consecutive integer ids starting from 1 up to this value.
    /// The CRUD benchmark will:
    ///   - READ  : [start_account_id, start_account_id + ops_per_block)
    ///   - UPDATE: [start_account_id + ops_per_block, start_account_id + 2*ops_per_block)
    ///   - DELETE: [start_account_id + 2*ops_per_block, start_account_id + 3*ops_per_block)
    ///   - CREATE: [init_entry_count + 1, init_entry_count + ops_per_block]
    #[arg(long, default_value_t = 1_000_000)]
    init_entry_count: u64,
}

fn main() {
    //env_logger::init();
    let args = CrudCli::parse();

    info!("CRUD benchmark config: {:?}", args);

    // Open existing QMDB directory.
    let mut config = Config::from_dir(&args.db_dir);

    let mut ads: AdsWrap<SimpleTask> = AdsWrap::new(&config);

    // Determine the next block height after the current chain tip.
    let metadb = ads.get_metadb();
    let current_height = metadb.read().get_curr_height();
    let mut next_height = current_height + 1;
    info!("Current height in QMDB: {current_height}, starting benchmark at height {next_height}");

    let cs_per_task = args.changesets_per_task;
    let ops_per_cs = args.ops_per_changeset;
    if cs_per_task == 0 {
        panic!("changesets_per_task must be greater than zero");
    }
    if ops_per_cs == 0 {
        panic!("ops_per_changeset must be greater than zero");
    }
    let ops_per_task = cs_per_task
        .checked_mul(ops_per_cs)
        .expect("changesets_per_task * ops_per_changeset overflowed; please lower one of them");
    if ops_per_task == 0 {
        panic!("invalid workload configuration: ops_per_task resolved to zero");
    }
    let tasks_per_block = args.ops_per_block / ops_per_task;
    if tasks_per_block == 0 {
        panic!(
            "ops_per_block={} is too small for {} changesets per task * {} ops per changeset; \
             increase --ops-per-block or reduce the other parameters",
            args.ops_per_block, cs_per_task, ops_per_cs
        );
    }
    let n = tasks_per_block * ops_per_task;
    if n != args.ops_per_block {
        info!(
            "Rounded ops_per_block down from {} to {} to fit an integer number of tasks \
             ({} tasks/block × {} changesets/task × {} ops/changeset)",
            args.ops_per_block, n, tasks_per_block, cs_per_task, ops_per_cs
        );
    }
    info!(
        "Workload layout: {} tasks/block × {} changesets/task × {} ops/changeset = {} ops",
        tasks_per_block, cs_per_task, ops_per_cs, n
    );
    let start_id = args.start_account_id;
    let init_entry_count = args.init_entry_count;
    let blocks_per_phase = args.blocks_per_phase;
    if blocks_per_phase == 0 {
        panic!("blocks_per_phase must be greater than zero");
    }
    let rounds = args.rounds;
    if rounds == 0 {
        panic!("rounds must be greater than zero");
    }
    let start_id = args.start_account_id;
    let init_entry_count = args.init_entry_count;
    let three_n = n
        .checked_mul(3)
        .expect("n is too large when multiplied by 3");
    let per_block_existing_span = three_n;
    let per_block_create_span = n;
    let round_stride_existing = per_block_existing_span
        .checked_mul(blocks_per_phase)
        .expect("per_block_existing_span * blocks_per_phase overflowed");
    let round_stride_create = per_block_create_span
        .checked_mul(blocks_per_phase)
        .expect("per_block_create_span * blocks_per_phase overflowed");
    let total_existing_span = round_stride_existing
        .checked_mul(rounds)
        .expect("round_stride_existing * rounds overflowed");
    let max_id_used_for_existing = start_id
        .checked_add(
            total_existing_span
                .checked_sub(1)
                .expect("total_existing_span must be > 0"),
        )
        .expect("start_account_id + total_existing_span overflowed u64");

    if max_id_used_for_existing > init_entry_count {
        panic!(
            "Configuration invalid: read/update/delete ranges exceed init-db account range. \
             Need start_account_id + 3*ops_per_block*blocks_per_phase*rounds - 1 <= init_entry_count \
             (got start_account_id={} ops_per_block={} blocks_per_phase={} rounds={} init_entry_count={})",
            start_id, n, blocks_per_phase, rounds, init_entry_count
        );
    }

    let create_start = init_entry_count + 1;
    let seed_base = start_id ^ init_entry_count ^ n ^ blocks_per_phase ^ rounds;

    // Use a fixed payload size for updates/creates; QMDB treats values as opaque bytes.
    let default_value: Vec<u8> = vec![0u8; 32];
    let double_n = n.checked_mul(2).expect("2 * ops_per_block overflowed");

    for round_idx in 0..rounds {
        let round_number = round_idx + 1;
        let round_seed = mix_round_seed(seed_base, round_idx);
        let round_existing_offset = round_stride_existing
            .checked_mul(round_idx)
            .expect("round existing offset overflowed");
        let read_base = start_id
            .checked_add(round_existing_offset)
            .expect("read base overflowed");
        let update_base = read_base.checked_add(n).expect("update base overflowed");
        let delete_base = read_base
            .checked_add(double_n)
            .expect("delete base overflowed");
        let round_create_offset = round_stride_create
            .checked_mul(round_idx)
            .expect("round create offset overflowed");
        let create_base = create_start
            .checked_add(round_create_offset)
            .expect("create base overflowed");

        run_op_sequence(
            "[Phase 1] Reads",
            "reads",
            OP_READ,
            READ_SALT,
            n,
            blocks_per_phase,
            per_block_existing_span,
            &mut ads,
            cs_per_task,
            ops_per_cs,
            read_base,
            round_seed,
            None,
            &mut next_height,
            round_number,
            rounds,
        );

        run_op_sequence(
            "[Phase 2] Updates",
            "writes",
            OP_WRITE,
            UPDATE_SALT,
            n,
            blocks_per_phase,
            per_block_existing_span,
            &mut ads,
            cs_per_task,
            ops_per_cs,
            update_base,
            round_seed,
            Some(&default_value),
            &mut next_height,
            round_number,
            rounds,
        );

        run_op_sequence(
            "[Phase 3] Deletes",
            "deletes",
            OP_DELETE,
            DELETE_SALT,
            n,
            blocks_per_phase,
            per_block_existing_span,
            &mut ads,
            cs_per_task,
            ops_per_cs,
            delete_base,
            round_seed,
            None,
            &mut next_height,
            round_number,
            rounds,
        );

        run_op_sequence(
            "[Phase 4] Creates",
            "creates",
            OP_CREATE,
            CREATE_SALT,
            n,
            blocks_per_phase,
            per_block_create_span,
            &mut ads,
            cs_per_task,
            ops_per_cs,
            create_base,
            round_seed,
            Some(&default_value),
            &mut next_height,
            round_number,
            rounds,
        );
    }

    // Print some internal cache counters for reference.
    COUNTERS.print();
}

/// Run a single CRUD phase as a pipelined QMDB block, using tasks and changesets
/// similar to `run_single_hover_block` in `speed.rs`.
///
/// - `op`: one of OP_READ / OP_WRITE / OP_DELETE / OP_CREATE.
/// - `account_ids`: exact list of account ids to touch (order will be followed as-provided).
/// - `changesets_per_task`: number of changesets per task.
/// - `ops_per_changeset`: number of operations per changeset.
/// - `value`: payload to use for CREATE / WRITE operations (ignored for READ / DELETE).
fn run_phase(
    ads: &mut AdsWrap<SimpleTask>,
    height: i64,
    op: u8,
    account_ids: &[u64],
    changesets_per_task: u64,
    ops_per_changeset: u64,
    value: Option<&[u8]>,
    label: &str,
) {
    let count = account_ids.len() as u64;
    if count == 0 {
        return;
    }

    // Estimate capacity: total_ops / (cs_per_task * ops_per_cs)
    let tasks_estimate = (count + (changesets_per_task * ops_per_changeset) - 1)
        / (changesets_per_task * ops_per_changeset);
    let mut task_list: Vec<RwLock<Option<SimpleTask>>> =
        Vec::with_capacity(tasks_estimate as usize);

    let mut cur_task_csets = Vec::new();
    let mut cur_cset = ChangeSet::new();
    let mut ops_in_cur_cset = 0;

    for &acc_id in account_ids {
        // Convert acc_id to a 20-byte key (big-endian, zero-padded)
        let mut key = [0u8; 20];
        key[12..20].copy_from_slice(&acc_id.to_be_bytes());
        let kh = entry_key_hash(&key);

        match op {
            OP_CREATE | OP_WRITE => {
                let v = value.expect("value must be provided for CREATE/WRITE");
                cs_add_op(&mut cur_cset, op, &kh, &key, v);
            }
            OP_DELETE | OP_READ => {
                cs_add_op(&mut cur_cset, op, &kh, &key, &[]);
            }
            _ => panic!("unsupported op code in CRUD benchmark: {}", op),
        }

        ops_in_cur_cset += 1;
        if ops_in_cur_cset >= ops_per_changeset {
            cur_cset.sort();
            cur_task_csets.push(cur_cset);
            cur_cset = ChangeSet::new();
            ops_in_cur_cset = 0;

            if cur_task_csets.len() as u64 >= changesets_per_task {
                let task = SimpleTask::new(cur_task_csets);
                task_list.push(RwLock::new(Some(task)));
                cur_task_csets = Vec::new();
            }
        }
    }

    // Handle remaining operations in the current changeset
    if ops_in_cur_cset > 0 {
        cur_cset.sort();
        cur_task_csets.push(cur_cset);
    }

    // Handle remaining changesets in the current task
    if !cur_task_csets.is_empty() {
        let task = SimpleTask::new(cur_task_csets);
        task_list.push(RwLock::new(Some(task)));
    }

    let task_count = task_list.len();
    let tasks_manager = Arc::new(TasksManager::new(task_list));

    let (started, _) = ads.start_block(height, tasks_manager);
    if !started {
        panic!("failed to start block at height {}", height);
    }

    let shared_ads = ads.get_shared();
    shared_ads.insert_extra_data(height, label.to_owned());
    for idx in 0..task_count {
        let task_id = join_task_id(height, idx, idx == task_count - 1);
        shared_ads.add_task(task_id);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_op_sequence(
    label_prefix: &str,
    action_name: &str,
    op: u8,
    salt: u64,
    ops_per_block: u64,
    blocks_per_phase: u64,
    block_stride: u64,
    ads: &mut AdsWrap<SimpleTask>,
    changesets_per_task: u64,
    ops_per_changeset: u64,
    base_start: u64,
    seed_base: u64,
    value: Option<&[u8]>,
    next_height: &mut i64,
    round_number: u64,
    total_rounds: u64,
) {
    let sequence_start = Instant::now();
    let mut last_height: Option<i64> = None;
    let total_ops = ops_per_block
        .checked_mul(blocks_per_phase)
        .expect("ops_per_block * blocks_per_phase overflowed");
    for block_idx in 0..blocks_per_phase {
        let block_number = block_idx + 1;
        let block_offset = block_stride
            .checked_mul(block_idx)
            .expect("block_stride * block_idx overflowed");
        let start_block = base_start
            .checked_add(block_offset)
            .expect("block start overflowed");
        let per_block_seed = mix_block_seed(seed_base ^ salt, block_idx);
        let shuffle_seed = derive_lfsr_seed(per_block_seed, salt);
        let account_ids = shuffled_id_range(start_block, ops_per_block, shuffle_seed);
        let label = format!(
            "{label_prefix} [Round {}/{}] (block {}/{})",
            round_number, total_rounds, block_number, blocks_per_phase
        );
        println!(
            "{}: submitting block {}/{} for round {}/{} ({} accounts)",
            label_prefix, block_number, blocks_per_phase, round_number, total_rounds, ops_per_block
        );
        run_phase(
            ads,
            *next_height,
            op,
            &account_ids,
            changesets_per_task,
            ops_per_changeset,
            value,
            label.as_str(),
        );
        println!(
            "{}: block {}/{} submitted at height {}",
            label_prefix, block_number, blocks_per_phase, *next_height
        );
        last_height = Some(*next_height);
        *next_height += 1;
    }
    if let Some(height) = last_height {
        flush_until_height(ads, height);
    }
    let elapsed = sequence_start.elapsed();
    println!(
        "{} [Round {}/{}]: completed {} blocks ({} ops) in {:.3}s (~{:.2} {}/s)",
        label_prefix,
        round_number,
        total_rounds,
        blocks_per_phase,
        total_ops,
        elapsed.as_secs_f64(),
        total_ops as f64 / elapsed.as_secs_f64().max(1e-9),
        action_name,
    );
}

fn shuffled_id_range(start: u64, count: u64, seed: u64) -> Vec<u64> {
    if count == 0 {
        return Vec::new();
    }
    let len = usize::try_from(count).expect("count does not fit in usize");
    let mut ids = Vec::with_capacity(len);
    for offset in 0..count {
        ids.push(start + offset);
    }
    shuffle_with_lfsr(&mut ids, seed);
    ids
}

fn shuffle_with_lfsr(ids: &mut [u64], seed: u64) {
    if ids.len() <= 1 {
        return;
    }
    let mut lfsr = GaloisLfsr::new(seed, LFSR_BITS);
    for i in (1..ids.len()).rev() {
        let rand = lfsr.next();
        let j = (rand % ((i + 1) as u64)) as usize;
        ids.swap(i, j);
    }
}

fn derive_lfsr_seed(base: u64, salt: u64) -> u64 {
    let mut seed = base.wrapping_mul(0x9E37_79B1_85EB_CA87).wrapping_add(salt) & LFSR_MASK;
    if seed == 0 {
        seed = 1;
    }
    seed
}

fn mix_block_seed(base: u64, block_idx: u64) -> u64 {
    base.wrapping_add(block_idx.wrapping_mul(0xA076_1D64_78BD_642F)) & LFSR_MASK
}

fn mix_round_seed(base: u64, round_idx: u64) -> u64 {
    base.wrapping_add(round_idx.wrapping_mul(0xE703_7ED1_A0B4_579B)) & LFSR_MASK
}

fn cs_add_op(cs: &mut ChangeSet, op: u8, kh: &[u8; 32], key: &[u8], value: &[u8]) {
    cs.add_op(op, kh, key, value, None);
}

fn flush_until_height(ads: &mut AdsWrap<SimpleTask>, target_height: i64) {
    loop {
        let metas: Vec<Arc<MetaInfo>> = ads.flush();
        if metas.iter().any(|m| m.curr_height == target_height) {
            break;
        }
    }
}