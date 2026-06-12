//! Prefetching module for QMDB.
//!
//! This module provides prefetching functionality to optimize data access by:
//! - Asynchronously loading data before it's needed
//! - Managing data transfer between threads
//! - Coordinating prefetch operations across shards
//! - Providing platform-specific optimizations
//!
//! The module uses different implementations based on the target platform:
//! - Linux: Uses direct I/O prefetching (dioprefetcher)
//! - Other platforms: Uses universal prefetching (uniprefetcher)

use cfg_if::cfg_if;

cfg_if! {
    if #[cfg(all(target_os = "linux", not(feature = "disable_dio")))] {
        pub mod dioprefetcher;
        pub use dioprefetcher::{JobManager, Prefetcher};

        /// Number of concurrent jobs for normal operation.
        /// Balances throughput and resource usage.
        const JOB_COUNT: usize = 40000;

        /// Number of concurrent jobs during warmup phase.
        /// Higher to accelerate initial data loading.
        const WARMUP_JOB_COUNT: usize = 160000;

        /// Number of sub-jobs to reserve.
        /// Ensures capacity for dependent operations.
        const SUB_JOB_RESERVE_COUNT: usize = 50;
    } else {
        pub mod uniprefetcher;
        pub use uniprefetcher::{JobManager, Prefetcher};

        /// Stub module so that `use crate::prefetcher::dioprefetcher::PruneCmd`
        /// compiles unchanged in flusher and lib when `disable_dio` is active or
        /// on non-Linux targets.  The channel is created and the cmd is sent by
        /// the flusher, but the uniprefetcher simply ignores it.
        pub mod dioprefetcher {
            #[derive(Debug, Clone, Copy)]
            pub struct PruneCmd {
                pub shard_id: usize,
                pub seg_id: usize,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::entryfile::{entrybuffer, EntryCache, EntryFile};
    use crate::indexer::Indexer;
    use crate::tasks::{BlockPairTaskHub, TasksManager};
    use crate::test_helper::SimpleTask;
    #[cfg(feature = "tee_cipher")]
    use aead::{Key, KeyInit};
    #[cfg(feature = "tee_cipher")]
    use aes_gcm::Aes256Gcm;
    use crossbeam::channel::bounded;
    use parking_lot::RwLock;
    use qmdb_common::def::{self, SHARD_COUNT};
    use qmdb_common::entryfile::entry::Entry;
    use qmdb_common::entryfile::readbuf::ReadBuf;
    use qmdb_common::indexer::indexer_trait::IndexerTrait;
    use qmdb_common::utils::byte0_to_shard_id;
    use qmdb_common::utils::changeset::ChangeSet;
    use std::sync::Arc;
    use threadpool::ThreadPool;

    use super::*;

    #[test]
    fn test_run_task() {
        let temp_dir = tempfile::Builder::new()
            .prefix("prefetcher_ut_cache_hit")
            .tempdir()
            .unwrap();
        #[cfg(not(feature = "tee_cipher"))]
        let cipher = None;
        #[cfg(feature = "tee_cipher")]
        let cipher = {
            let key = Key::<Aes256Gcm>::from_slice(&[0; 32]);
            let cipher = Aes256Gcm::new(&key);
            Some(cipher)
        };

        let entry_file = Arc::new(EntryFile::new(
            8 * 1024,
            128 * 1024,
            temp_dir.path().to_str().unwrap().to_string(),
            cipher,
        ));
        let (mut entry_buffer_writer, _reader) = entrybuffer::new(0, 1024);
        let entry_cache = Arc::new(EntryCache::new());
        let indexer_arc = Arc::new(Indexer::new(1024));
        let task_hub = Arc::new(BlockPairTaskHub::<SimpleTask>::new());
        let pool = ThreadPool::new(1);

        let mut cs1 = ChangeSet::new();
        let key = "key".as_bytes();
        let kh = qmdb_common::entryfile::entry::entry_key_hash(key);
        let shard_id = byte0_to_shard_id(kh[0]) as u8;
        cs1.add_op(def::OP_DELETE, &kh, key.as_ref(), &[1, 2, 3], Option::None);
        cs1.sort();
        let task0 = SimpleTask::new(vec![ChangeSet::new()]);
        let task1 = SimpleTask::new(vec![cs1]);
        let tasks = vec![
            RwLock::new(Option::Some(task0)),
            RwLock::new(Option::Some(task1)),
        ];

        let entry = Entry {
            key,
            value: "val".as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        };
        let pos = entry_buffer_writer.append(&entry, &[]);
        indexer_arc.add_kv(&kh[..10], pos, 0);

        for _i in 0..2000 {
            let _ = entry_buffer_writer.append(&entry, &[]);
        }

        task_hub.start_block(
            0x123,
            Arc::new(TasksManager::new(tasks)),
            entry_cache.clone(),
        );

        let mut job_man = JobManager::new(0, 0);
        let mut done_receivers = Vec::with_capacity(SHARD_COUNT);
        for _ in 0..SHARD_COUNT {
            let (done_sender, done_receiver) = bounded(1);
            job_man.add_shard(
                entry_buffer_writer.entry_buffer.clone(),
                entry_file.clone(),
                done_sender,
            );
            done_receivers.push(done_receiver);
        }

        let (_prune_tx, prune_rx) = bounded(1);
        let prefetcher = Prefetcher::new(
            task_hub,
            entry_cache.clone(),
            indexer_arc,
            Arc::new(pool),
            job_man,
            prune_rx,
        );

        prefetcher.run_task(0x123000001);

        std::thread::sleep(std::time::Duration::from_secs(1));
        assert!(done_receivers[0].try_recv().is_ok());
        assert!(done_receivers[0].try_recv().is_err());
        assert!(done_receivers[shard_id as usize].try_recv().is_ok());
        assert!(done_receivers[shard_id as usize].try_recv().is_err());

        let mut buf = ReadBuf::new();
        entry_cache.lookup(shard_id as usize, 0, &mut buf);

        #[cfg(not(feature = "tee_cipher"))]
        assert_eq!(64, buf.len());
        #[cfg(feature = "tee_cipher")]
        assert_eq!(80, buf.len());
    }
}
