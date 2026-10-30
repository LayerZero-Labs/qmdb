use crate::entryfile::{EntryBuffer, EntryCache, EntryFile};
use crate::indexer::Indexer;
use crate::tasks::TaskHub;
use atomptr::{AtomPtr, Ref};
use crossbeam::channel::{Receiver, Sender};
use log::debug;
use qmdb_common::def::SHARD_COUNT;
use qmdb_common::entryfile::readbuf::ReadBuf;
use qmdb_common::indexer::indexer_trait::IndexerTrait;
use qmdb_common::task::taskid::split_task_id;
use qmdb_common::utils::byte0_to_shard_id;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use threadpool::ThreadPool;

/// Manages jobs for the universal prefetcher implementation.
///
/// This implementation is used for platforms that don't support io_uring.
/// It provides basic job management functionality for prefetching entries
/// from buffers and files.
pub struct JobManager {
    /// Buffers containing recent updates
    update_buffers: Vec<Arc<EntryBuffer>>,
    /// Files containing persisted entries
    entry_files: Vec<Arc<EntryFile>>,
    /// Channels for signaling task completion
    done_chans: Vec<Sender<i64>>,
}

impl Default for JobManager {
    /// Creates a new JobManager with default settings.
    fn default() -> Self {
        Self::new(0, 0)
    }
}

impl JobManager {
    /// Creates a new JobManager.
    ///
    /// # Arguments
    /// * `_` - Unused uring count parameter (for API compatibility)
    /// * `_` - Unused uring size parameter (for API compatibility)
    pub fn new(_: usize, _: u32) -> Self {
        JobManager {
            update_buffers: Vec::with_capacity(SHARD_COUNT),
            entry_files: Vec::with_capacity(SHARD_COUNT),
            done_chans: Vec::with_capacity(SHARD_COUNT),
        }
    }

    /// Adds a shard to the JobManager.
    ///
    /// # Arguments
    /// * `update_buffer` - Buffer containing recent updates for the shard
    /// * `entry_file` - File containing persisted entries for the shard
    /// * `done_chan` - Channel for signaling task completion for the shard
    pub fn add_shard(
        &mut self,
        update_buffer: Arc<EntryBuffer>,
        entry_file: Arc<EntryFile>,
        done_chan: Sender<i64>,
    ) {
        self.update_buffers.push(update_buffer);
        self.entry_files.push(entry_file);
        self.done_chans.push(done_chan);
    }
}

/// Prefetcher implementation for platforms without io_uring support.
///
/// This prefetcher uses a thread pool to parallelize entry fetching
/// operations. It coordinates with the task hub to prefetch entries
/// needed for task execution.
pub struct Prefetcher {
    /// Task hub for coordinating task execution
    task_hub: Arc<dyn TaskHub>,
    /// Buffers containing recent updates
    update_buffers: Vec<Arc<EntryBuffer>>,
    /// Files containing persisted entries
    entry_files: Vec<Arc<EntryFile>>,
    /// Cache for storing fetched entries
    cache: AtomPtr<Arc<EntryCache>>,
    /// Indexer for looking up entry positions
    indexer: Arc<Indexer>,
    /// Channels for signaling task completion
    done_chans: Vec<Sender<i64>>,
    /// Thread pool for parallel fetching
    tpool: Arc<ThreadPool>,
    cur_height: AtomicI64,
}

/// Fetches an entry and stores it in the cache.
///
/// # Arguments
/// * `update_buffer` - Buffer containing recent updates
/// * `entry_file` - File containing persisted entries
/// * `cache` - Cache to store the fetched entry
/// * `shard_id` - ID of the shard containing the entry
/// * `file_pos` - Position of the entry in the file
fn fetch_entry_to_cache(
    update_buffer: &Arc<EntryBuffer>,
    entry_file: &Arc<EntryFile>,
    cache: &Arc<EntryCache>,
    shard_id: usize,
    file_pos: i64,
) {
    // try to insert a locked entry_pos
    let cache_hit = cache.contains(shard_id, file_pos);
    if cache_hit {
        return; // no need to fetch
    }
    // in 'get_entry_bz_at', 'curr_buf' is None because we cannot read it
    let mut buf = ReadBuf::new();
    let in_disk = update_buffer.get_entry_bz(file_pos, &mut buf);
    if !buf.is_empty() {
        cache.insert(shard_id, file_pos, &buf.as_entry_bz());
        return;
    }

    if !in_disk {
        return;
    }

    entry_file.read_entry(file_pos, &mut buf);
    if !buf.is_empty() {
        cache.insert(shard_id, file_pos, &buf.as_entry_bz());
    }
}

impl Prefetcher {
    /// Creates a new Prefetcher.
    ///
    /// # Arguments
    /// * `task_hub` - Task hub for coordinating task execution
    /// * `cache` - Cache for storing fetched entries
    /// * `indexer` - Indexer for looking up entry positions
    /// * `tpool` - Thread pool for parallel fetching
    /// * `job_man` - Job manager for managing prefetch operations
    pub fn new(
        task_hub: Arc<dyn TaskHub>,
        cache: Arc<EntryCache>,
        indexer: Arc<Indexer>,
        tpool: Arc<ThreadPool>,
        job_man: JobManager,
        // Accepted for API parity with dioprefetcher::Prefetcher::new; ignored
        // because uniprefetcher does not use io_uring and needs no prune signals.
        _prune_receiver: crossbeam::channel::Receiver<crate::prefetcher::dioprefetcher::PruneCmd>,
    ) -> Self {
        Self {
            task_hub,
            cache: AtomPtr::new(cache),
            indexer,
            update_buffers: job_man.update_buffers,
            entry_files: job_man.entry_files,
            done_chans: job_man.done_chans,
            tpool,
            cur_height: AtomicI64::new(-1),
        }
    }

    /// Starts the prefetcher threads.
    ///
    /// # Arguments
    /// * `task_receiver` - Channel for receiving task IDs to process
    pub fn start_threads(self: Arc<Self>, task_receiver: Receiver<i64>) {
        std::thread::spawn(move || loop {
            match task_receiver.recv() {
                Ok(task_id) => {
                    self.run_task(task_id);
                }
                Err(_) => {
                    debug!("task receiver in prefetcher exit!");
                    return;
                }
            }
        });
    }

    pub fn run_task(&self, task_id: i64) {
        let height = split_task_id(task_id).0;
        let cur_height = self.cur_height.fetch_max(height, Ordering::SeqCst);
        if cur_height != height {
            assert!(
                cur_height == -1 || cur_height + 1 == height,
                "cur_height={} height={}",
                cur_height,
                height
            );
            let cache = self.task_hub.get_entry_cache(height);
            let old = self.cache.swap(cache);
            drop(old);
        }
        let task_hub = self.task_hub.clone();
        let mut thread_counts = Vec::with_capacity(SHARD_COUNT);
        for shard_id in 0..SHARD_COUNT {
            let mut thread_count = 0usize;
            // first we need to know the count of threads that will be spawned
            for change_set in &*task_hub.get_change_sets(task_id) {
                thread_count += change_set.op_count_in_shard(shard_id);
            }
            // if there are no OP_*, prefetcher needs to do nothing on this shard,
            // so we can signal completion immediately.
            if thread_count == 0 {
                self.done_chans[shard_id].send(task_id).unwrap();
            }
            thread_counts.push(AtomicUsize::new(thread_count));
        }
        // spawn a thread for each OP_*
        let thread_counts = Arc::new(thread_counts);
        let change_sets = task_hub.get_change_sets(task_id).clone();
        for i in 0..change_sets.len() {
            let update_buffers = self.update_buffers.clone();
            let entry_files = self.entry_files.clone();
            let arc: Ref<Arc<EntryCache>> = self.cache.get_ref();
            let cache = Arc::clone(&arc);
            let thread_counts = thread_counts.clone();
            let done_chans = self.done_chans.clone();
            let indexer = self.indexer.clone();
            let change_sets = change_sets.clone();
            self.tpool.execute(move || {
                change_sets[i].for_each_op(|op, _, kh: &[u8; 32], _k, _v, _, _r| {
                    let shard_id = byte0_to_shard_id(kh[0]);
                    let pos_list = indexer.for_each(height, op, &kh[..]);
                    for pos in pos_list.enumerate() {
                        let ubuf = &update_buffers[shard_id];
                        let ef = &entry_files[shard_id];
                        fetch_entry_to_cache(ubuf, ef, &cache, shard_id, pos);
                    }
                    // the last finished thread for this shard sends task_id to done_chan
                    if thread_counts[shard_id].fetch_sub(1, Ordering::SeqCst) == 1 {
                        done_chans[shard_id].send(task_id).unwrap();
                    }
                });
            });
        }
    }

    /// Fetches entries from previous tasks into the provided cache.
    ///
    /// This variant is used when the main pipeline bypasses the asynchronous
    /// prefetcher (e.g., in environments without io_uring or when explicit
    /// prefetch timing is desired). The caller supplies the target
    /// `entry_cache`, which is used as the destination for prefetched data.
    /// Only entries related to `OP_CREATE` and `OP_DELETE` are prefetched,
    /// mirroring the behavior of the linux `dioprefetcher::fetch_prev_entries`,
    /// since those operations may need to read adjacent (previous) entries to
    /// maintain linked lists in the entry file. No completion notifications
    /// are sent on `done_chans`; callers are expected to fall back to direct
    /// reads if a requested entry is not yet present in the cache.
    pub fn fetch_prev_entries(&self, task_id: i64, entry_cache: Arc<EntryCache>) {
        let height = split_task_id(task_id).0;
        let change_sets = self.task_hub.get_change_sets(task_id).clone();

        // Only prefetch when there is at least one create/delete operation.
        let need_job = change_sets.iter().any(|cs| cs.has_create_del());
        if !need_job {
            return;
        }

        let update_buffers = self.update_buffers.clone();
        let entry_files = self.entry_files.clone();
        let indexer = self.indexer.clone();

        // Process each ChangeSet in the thread pool, prefetching only "adjacent"
        // (previous) entries for keys that are being created or deleted.
        for i in 0..change_sets.len() {
            let ubufs = update_buffers.clone();
            let efs = entry_files.clone();
            let cache = entry_cache.clone();
            let idx = indexer.clone();
            let change_sets = change_sets.clone();

            self.tpool.execute(move || {
                change_sets[i].for_each_op(|op, _, kh: &[u8; 32], _k, _v, _, _r| {
                    if op == qmdb_common::def::OP_CREATE || op == qmdb_common::def::OP_DELETE {
                        let shard_id = byte0_to_shard_id(kh[0]);
                        let pos_list = idx.for_each_adjacent_value(height, &kh[..]);
                        for pos in pos_list.enumerate() {
                            let ubuf = &ubufs[shard_id];
                            let ef = &efs[shard_id];
                            fetch_entry_to_cache(ubuf, ef, &cache, shard_id, pos);
                        }
                    }
                });
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::entryfile::{entrybuffer, EntryCache, EntryFile, EntryFileWriter};
    #[cfg(feature = "tee_cipher")]
    use aead::{Key, KeyInit};
    #[cfg(feature = "tee_cipher")]
    use aes_gcm::Aes256Gcm;
    use qmdb_common::entryfile::entry::{Entry, EntryBz};
    use std::sync::Arc;

    use super::*;

    /// Tests that fetch_entry_to_cache handles cache hits correctly.
    #[test]
    fn test_fetch_entry_to_cache_cache_hit() {
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

        let (entry_buffer_writer, _reader) = entrybuffer::new(0, 1024);
        let entry_buffer = entry_buffer_writer.entry_buffer;
        let entry_file = Arc::new(EntryFile::new(
            8 * 1024,
            128 * 1024,
            temp_dir.path().to_str().unwrap().to_string(),
            cipher,
        ));
        let entry_cache = Arc::new(EntryCache::new());

        // Create a properly formatted entry
        let entry = Entry {
            key: "key".as_bytes(),
            value: "value".as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        };
        let deactived_sn_list = [];
        let mut buf = [0u8; 128];
        let entry_len = entry.dump(&mut buf, &deactived_sn_list);

        entry_cache.insert(
            7,
            123,
            &EntryBz {
                bz: &buf[..entry_len],
            },
        );

        fetch_entry_to_cache(&entry_buffer, &entry_file, &entry_cache, 7, 123);
    }

    /// Tests that fetch_entry_to_cache can fetch entries from the buffer.
    #[test]
    fn test_fetch_entry_to_cache_in_buffer() {
        let temp_dir = tempfile::Builder::new()
            .prefix("test_fetch_entry_to_cache_in_buffer")
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

        let entry = Entry {
            key: "key".as_bytes(),
            value: "value".as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        };
        let deactived_sn_list = [];

        for _i in 0..2000 {
            entry_buffer_writer.append(&entry, &deactived_sn_list);
        }

        fetch_entry_to_cache(
            &entry_buffer_writer.entry_buffer,
            &entry_file,
            &entry_cache.clone(),
            7,
            0,
        );

        let mut buf = ReadBuf::new();
        entry_cache.lookup(7, 0, &mut buf);

        #[cfg(not(feature = "tee_cipher"))]
        assert_eq!(64, buf.len());
        #[cfg(feature = "tee_cipher")]
        assert_eq!(80, buf.len());
    }

    #[test]
    fn test_fetch_entry_to_cache_from_file() {
        let temp_dir = tempfile::Builder::new()
            .prefix("test_fetch_entry_to_cache_from_file")
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
            cipher.clone(),
        ));
        let mut entry_file_w = EntryFileWriter::new(entry_file.clone(), 8 * 1024);
        let (entry_buffer_writer, _reader) = entrybuffer::new(1, 1024);
        let entry_buffer = entry_buffer_writer.entry_buffer;
        let entry_cache = Arc::new(EntryCache::new());

        let entry = Entry {
            key: "key".as_bytes(),
            value: "value".as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        };
        let deactived_sn_list = [];

        let mut entry_bz_buf: [u8; 1000] = [0; 1000];
        let entry_bz_size = entry.dump(&mut entry_bz_buf[..], &deactived_sn_list);
        let entry_bz = EntryBz {
            bz: &entry_bz_buf[..entry_bz_size],
        };

        entry_file_w.append(&entry_bz).unwrap();
        entry_file_w.flush().unwrap();

        let shard_id = byte0_to_shard_id(entry_bz.key()[0]);
        fetch_entry_to_cache(
            &entry_buffer,
            &entry_file,
            &entry_cache.clone(),
            shard_id,
            0,
        );

        let mut buf = ReadBuf::new();
        entry_cache.lookup(shard_id, 0, &mut buf);

        #[cfg(not(feature = "tee_cipher"))]
        assert_eq!(64, buf.len());
        #[cfg(feature = "tee_cipher")]
        assert_eq!(80, buf.len());
    }
}
