//! Direct I/O prefetcher implementation for Linux systems.
//!
//! This module provides a high-performance prefetching implementation that:
//! - Uses io_uring for asynchronous I/O operations
//! - Manages direct I/O buffers for optimal performance
//! - Coordinates prefetch operations across multiple shards
//! - Handles both task-based and warmup-based prefetching

use crate::entryfile::{EntryBuffer, EntryCache, EntryFile};
use crate::indexer::Indexer;
use crate::tasks::TaskHub;
use atomptr::{AtomPtr, Ref};
use crossbeam::channel::{bounded, unbounded, Receiver, Sender};
use hpfile::AlignedBuf512;
use io_uring::{cqueue, opcode, squeue, types, IoUring};
use log::debug;
use qmdb_common::def::{DEFAULT_ENTRY_SIZE, OP_CREATE, OP_DELETE, SHARD_COUNT};
use qmdb_common::entryfile::entry::EntryBz;
use qmdb_common::entryfile::readbuf::ReadBuf;
use qmdb_common::indexer::indexer_trait::IndexerTrait;
use qmdb_common::task::taskid::{is_first_task_in_block, split_task_id};
use qmdb_common::utils::byte0_to_shard_id;
use smallvec::SmallVec;
use std::collections::{HashMap, HashSet, VecDeque};
use std::iter::repeat_with;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::usize;
use threadpool::ThreadPool;
use {std::fs::File, std::os::unix::io::AsRawFd};

const SC: Ordering = Ordering::SeqCst;
pub const BLK_SIZE: i64 = 4096;

/// Minimum number of CQEs to wait for in `submit_and_wait`.
const MIN_WAIT: usize = 1;
/// Maximum number of CQEs to wait for in `submit_and_wait`.
/// Larger values reduce syscall frequency but increase tail latency.
const MAX_WAIT: usize = 32;

/// Maximum registered files: 16 shards × 256 segments = 4096
const MAX_REGISTERED_FILES: u32 = 16 * 256;

/// Number of SSD reads submitted to io_uring so far. The benchmark polls it to time a
/// Read phase until the prefetcher has issued its last read.
pub static DIO_READ_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn dio_read_count() -> u64 {
    DIO_READ_COUNT.load(Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy)]
pub struct PruneCmd {
    pub shard_id: usize,
    pub seg_id: usize,
}

struct RegisteredFiles {
    key_to_slot: HashMap<u64, u16>,
    next_slot: u16,
    pending: Vec<(u64, RawFd)>,
    pending_keys: HashSet<u64>,
    // [新增] 真正实现槽位复用，修复性能退化
    free_slots: Vec<u16>, 
}

impl RegisteredFiles {
    fn new() -> Self {
        Self {
            key_to_slot: HashMap::with_capacity(256),
            next_slot: 0,
            pending: Vec::new(),
            pending_keys: HashSet::with_capacity(256),
            // [新增] 初始化资源池
            free_slots: Vec::with_capacity(256), 
        }
    }

    fn lookup_or_mark_pending(&mut self, key: u64, fd: RawFd) -> Option<u16> {
        if let Some(&slot) = self.key_to_slot.get(&key) {
            return Some(slot);
        }
        // [修改] 检查池子里有没有，或者还能不能开辟新槽
        if self.free_slots.is_empty() && self.next_slot as u32 >= MAX_REGISTERED_FILES {
            return None;
        }
        if self.pending_keys.insert(key) {
            self.pending.push((key, fd));
        }
        None
    }

    /// [重写] 极速异步驱逐 (0 Syscall overhead!)
    pub fn evict_sync<S: squeue::EntryMarker, C: cqueue::EntryMarker>(
        &mut self,
        _ring: &IoUring<S, C>, // 甚至不需要真的去调用 ring 了
        key: u64,
    ) {
        if let Some(slot) = self.key_to_slot.remove(&key) {
            self.pending_keys.remove(&key);
            
            // 魔法在这里：不再调用 register_files_update(&[-1])
            // 只是把 slot 回收。等下次写入新 FD 时，内核会自动覆盖并回收旧文件！
            self.free_slots.push(slot); 
        }
    }

    fn flush_pending<S: squeue::EntryMarker, C: cqueue::EntryMarker>(
        &mut self,
        ring: &IoUring<S, C>,
    ) {
        let mut leftovers: Vec<(u64, RawFd)> = Vec::new();
        for (key, fd) in self.pending.drain(..) {
            
            // [新增] 优先复用被驱逐的旧槽位！
            let slot = if let Some(s) = self.free_slots.pop() {
                s
            } else if (self.next_slot as u32) < MAX_REGISTERED_FILES {
                let s = self.next_slot;
                self.next_slot += 1;
                s
            } else {
                leftovers.push((key, fd));
                continue;
            };

            // 这里的 update 会一石二鸟：
            // 1. 如果是新 slot，正常注册。
            // 2. 如果是复用的旧 slot，内核会自动 drop 掉上一个文件，安全释放磁盘！
            if ring
                .submitter()
                .register_files_update(slot as u32, &[fd])
                .is_ok()
            {
                self.key_to_slot.insert(key, slot);
            } else {
                self.free_slots.push(slot); // 失败了把槽位还回去
                leftovers.push((key, fd));
            }
        }
        
        self.pending = leftovers;
        self.pending_keys.clear();
        for (key, _) in self.pending.iter() {
            self.pending_keys.insert(*key);
        }
    }
}

/// Main prefetcher component that manages prefetching operations.
///
/// Coordinates:
/// - Task-based prefetching
/// - Warmup prefetching
/// - Cache management
/// - Job scheduling
pub struct Prefetcher {
    /// Task hub for accessing task information
    task_hub: Arc<dyn TaskHub>,
    /// Entry cache for storing prefetched data
    cache: AtomPtr<Arc<EntryCache>>,
    /// Indexer for looking up entry positions
    indexer: Arc<Indexer>,
    /// Job manager for coordinating prefetch operations
    job_man: Arc<JobManager>,
    /// Thread pool for executing tasks
    tpool: Arc<ThreadPool>,
    /// Channel for receiving task jobs
    bck_receiver: Receiver<Box<TaskJob>>,
    /// Channels for receiving warmup jobs
    warmup_bck_receivers: Vec<Receiver<Box<WarmupJob>>>,
}

/// Manages prefetch jobs and I/O operations across shards.
///
/// Handles:
/// - Buffer management
/// - I/O scheduling
/// - Job coordination
/// - Completion notification
pub struct JobManager {
    /// Entry buffers for each shard
    update_buffers: Vec<Arc<EntryBuffer>>,
    /// Entry files for each shard
    entry_files: Vec<Arc<EntryFile>>,
    /// Channels for signaling job completion.
    ///
    /// Completion semantics are intentionally **weak** here:
    /// - A notification means that all prefetch *sub-jobs* for the given task have been
    ///   scheduled and submitted to `io_uring`.
    /// - It does **not** guarantee that every underlying I/O operation has already completed.
    ///
    /// This trades strong synchronization guarantees for lower CPU and bookkeeping overhead.
    /// Downstream components must be prepared to fall back to direct reads when a prefetched
    /// entry is not yet available in the cache.
    done_chans: Vec<Sender<i64>>,
    /// Channel for sending task jobs
    bck_sender: Sender<Box<TaskJob>>,
    /// Channels for sending warmup jobs
    warmup_bck_senders: Vec<Sender<Box<WarmupJob>>>,
    /// Channels for sending sub-jobs to io_uring instances
    sub_id_senders: Vec<Sender<Arc<Job>>>,
    /// Number of io_uring instances
    uring_count: usize,
    /// Size of each io_uring instance
    uring_size: u32,
    /// 【added】
    pub prune_receiver: Option<Receiver<PruneCmd>>,
}

/// Per-io_uring-worker tracking for a job.
///
/// We keep exactly one `Arc<Job>` alive per job (per worker) to avoid per-I/O refcount
/// contention on the `Arc` header cacheline.
struct JobSlot {
    job: Arc<Job>,
    /// pending + in-flight for this job within this io_uring worker
    remaining: usize,
}

impl Prefetcher {
    /// Creates a new Prefetcher instance.
    ///
    /// # Arguments
    /// * `task_hub` - Task hub for accessing task information
    /// * `cache` - Entry cache for storing prefetched data
    /// * `indexer` - Indexer for looking up entry positions
    /// * `tpool` - Thread pool for executing tasks
    /// * `job_man` - Job manager for coordinating prefetch operations
    ///
    /// # Returns
    /// A new Prefetcher instance
    pub fn new(
        task_hub: Arc<dyn TaskHub>,
        cache: Arc<EntryCache>,
        indexer: Arc<Indexer>,
        tpool: Arc<ThreadPool>,
        mut job_man: JobManager,
        prune_receiver: Receiver<PruneCmd>,
    ) -> Self {
        job_man.prune_receiver = Some(prune_receiver);

        let uring_count = job_man.uring_count;
        let (job_man, bck_receiver, warmup_bck_receivers) = job_man.start_threads(uring_count);
        Self {
            task_hub,
            cache: AtomPtr::new(cache),
            indexer,
            job_man,
            tpool,
            bck_receiver,
            warmup_bck_receivers,
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
            };
        });
    }

    /// Runs a warmup job for prefetching.
    ///
    /// # Arguments
    /// * `file_pos` - Position in the entry file
    /// * `shard_id` - ID of the shard
    /// * `cache` - Cache to store the fetched data
    pub fn run_warmup(&self, file_pos: i64, shard_id: usize, cache: Arc<EntryCache>) {
        let count = self.warmup_bck_receivers.len();
        let uring_id = SubJob::uring_id(file_pos, shard_id, count);
        let mut job = self.warmup_bck_receivers[uring_id].recv().unwrap();
        job.cache = Some(cache);
        job.add(0, shard_id, file_pos);
        let job_man = self.job_man.clone();
        job_man.add_job(Job::Warmup(job, SmallVec::new()));
    }

    /// Runs a task job for prefetching.
    ///
    /// # Arguments
    /// * `task_id` - ID of the task to process
    pub fn run_task(&self, task_id: i64) {
        let height = split_task_id(task_id).0;
        if is_first_task_in_block(task_id) {
            let cache = self.task_hub.get_entry_cache(height);
            let old = self.cache.swap(cache);
            drop(old);
        }
        let change_sets = self.task_hub.get_change_sets(task_id).clone();
        let mut job = self.bck_receiver.recv().unwrap();
        job.need_send_to_updater = true;
        job.task_id = task_id;
        let arc: Ref<Arc<EntryCache>> = self.cache.get_ref();
        job.cache = Some(Arc::clone(&arc));
        let indexer = self.indexer.clone();
        let change_sets = change_sets.clone();
        let job_man = self.job_man.clone();
        self.tpool.execute(move || {
            let mut sub_id = 0u32;
            for change_set in change_sets.iter() {
                change_set.for_each_op(|op, _, kh: &[u8; 32], _k, _v, _, _r| {
                    let shard_id = byte0_to_shard_id(kh[0]);

                    let pos_list = if op == OP_CREATE || op == OP_DELETE {
                        indexer.for_each_adjacent_value(height, &kh[..])
                    } else {
                        indexer.for_each(height, op, &kh[..])
                    };

                    pos_list.enumerate().for_each(|pos| {
                        job.add(sub_id, shard_id, pos);
                        sub_id += 1;
                    });
                });
            }
            job_man.add_job(Job::Task(job, SmallVec::new()));

            // Soft completion notification:
            // - Signals that all prefetch sub-jobs for this task have been *submitted* to
            //   the underlying io_uring instances.
            // - Does NOT guarantee that every I/O has finished when the receiver observes
            //   this task_id on `done_chans`.
            //
            // This weaker semantic is a deliberate performance/complexity trade-off:
            // tracking per-shard outstanding sub-jobs with atomics would provide stronger
            // guarantees, but costs more CPU and coordination. Consumers are expected to
            // tolerate occasionally missing prefetched entries and fall back to direct
            // reads when necessary.
            for shard_id in 0..SHARD_COUNT {
                job_man.done_chans[shard_id].send(task_id).unwrap();
            }
        });
    }

    /// Fetches entries from previous tasks.
    ///
    /// # Arguments
    /// * `task_id` - ID of the task to process
    pub fn fetch_prev_entries(&self, task_id: i64, cache: Arc<EntryCache>) {
        let height = split_task_id(task_id).0;
        let change_sets = self.task_hub.get_change_sets(task_id).clone();
        let need_job = change_sets.iter().any(|x| x.has_create_del());
        if !need_job {
            return;
        }

        let mut job = self.bck_receiver.recv().unwrap();
        job.task_id = task_id;
        job.cache = Some(cache);
        job.need_send_to_updater = false;
        let mut sub_id = 0u32;
        for change_set in change_sets.iter() {
            change_set.for_each_op(|op, _, kh: &[u8; 32], _k, _v, _, _r| {
                if op == OP_CREATE || op == OP_DELETE {
                    let shard_id = byte0_to_shard_id(kh[0]);
                    let v = self.indexer.for_each_adjacent_value(height, &kh[..]);
                    v.enumerate().for_each(|pos| {
                        job.add(sub_id, shard_id, pos);
                        sub_id += 1;
                    });
                }
            });
        }
        self.job_man.add_job(Job::Task(job, SmallVec::new()));
    }
}

/// Represents a job to be processed by the prefetcher.
///
/// Can be either:
/// - A task job for processing multiple entries
/// - A warmup job for prefetching specific entries
enum Job {
    /// Task-based job for processing multiple entries
    Task(Box<TaskJob>, SmallVec<[u32; 32]>),
    /// Warmup job for prefetching specific entries
    Warmup(Box<WarmupJob>, SmallVec<[u32; 32]>),
}

/// A job for processing multiple entries as part of a task.
pub struct TaskJob {
    /// Cache for storing prefetched entries
    cache: Option<Arc<EntryCache>>,
    /// ID of the task being processed
    task_id: i64,
    /// List of sub-jobs to process
    sub_jobs: Vec<SubJob>,
    /// Total number of sub-jobs
    sub_job_count: usize,
    need_send_to_updater: bool,
}

/// A job for prefetching specific entries during warmup.
pub struct WarmupJob {
    /// Cache for storing prefetched entries
    cache: Option<Arc<EntryCache>>,
    /// Single sub-job to process
    sub_jobs: [SubJob; 1],
}

impl Default for TaskJob {
    /// Creates a new TaskJob with default settings.
    ///
    /// Initializes:
    /// - Empty read counts for each shard
    /// - No cache
    /// - Minimum task ID
    /// - Empty sub-jobs list
    fn default() -> Self {
        Self {
            cache: None,
            task_id: i64::MIN,
            sub_jobs: Vec::new(),
            sub_job_count: 0,
            need_send_to_updater: true,
        }
    }
}

#[inline]
fn mix64(mut x: u64) -> u64 {
    // splitmix64 finalizer: fast, good avalanche, stable across runs.
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

impl Job {
    /// Gets a reference to a specific sub-job.
    ///
    /// # Arguments
    /// * `idx` - Index of the sub-job
    ///
    /// # Returns
    /// Reference to the requested sub-job
    fn get_sub_job(&self, idx: usize) -> &SubJob {
        match self {
            Self::Task(job, _) => &job.sub_jobs[idx],
            Self::Warmup(job, _) => &job.sub_jobs[idx],
        }
    }

    /// Gets the total number of sub-jobs.
    ///
    /// # Returns
    /// Number of sub-jobs in this job
    fn get_sub_job_count(&self) -> usize {
        match self {
            Self::Task(job, _) => job.sub_job_count,
            Self::Warmup(_job, _) => 1,
        }
    }

    #[inline]
    fn disk_sub_ids(&self) -> &[u32] {
        match self {
            Self::Task(_, ids) => ids.as_slice(),
            Self::Warmup(_, ids) => ids.as_slice(),
        }
    }

    #[inline]
    fn disk_sub_ids_mut(&mut self) -> &mut SmallVec<[u32; 32]> {
        match self {
            Self::Task(_, ids) => ids,
            Self::Warmup(_, ids) => ids,
        }
    }

    /// Routing key used to pin a whole Job onto a single io_uring worker.
    ///
    /// - Task jobs: use task_id (stable, naturally spreads over time)
    /// - Warmup jobs: use (shard_id, file_pos)
    #[inline]
    fn routing_key(&self) -> u64 {
        match self {
            Self::Task(job, _) => job.task_id as u64,
            Self::Warmup(job, _) => {
                let sj = &job.sub_jobs[0];
                let shard = sj.shard_id as u64;
                let pos = sj.file_pos as u64;
                (shard << 56) | (pos & ((1u64 << 56) - 1))
            }
        }
    }

    #[inline]
    fn routing_uring_id(&self, uring_count: usize) -> usize {
        debug_assert!(uring_count > 0);
        (mix64(self.routing_key()) as usize) % uring_count
    }

    /// Gets a reference to the entry cache.
    ///
    /// # Returns
    /// Reference to the entry cache
    fn get_entry_cache(&self) -> &Arc<EntryCache> {
        match self {
            Self::Task(job, _) => job.cache.as_ref().unwrap(),
            Self::Warmup(job, _) => job.cache.as_ref().unwrap(),
        }
    }
}

impl TaskJob {
    /// Adds a new sub-job to this task.
    ///
    /// # Arguments
    /// * `sub_id` - ID for the new sub-job
    /// * `shard_id` - ID of the shard
    /// * `file_pos` - Position in the entry file
    fn add(&mut self, sub_id: u32, shard_id: usize, file_pos: i64) {
        if self.sub_job_count < self.sub_jobs.len() {
            let i = self.sub_job_count;
            self.sub_jobs[i].reinit(sub_id, shard_id, file_pos);
        } else {
            let sj = SubJob::new(sub_id, shard_id, file_pos);
            self.sub_jobs.push(sj);
        }
        self.sub_job_count += 1;
    }

    /// Clears this task job for reuse.
    ///
    /// Resets:
    /// - Read counts
    /// - Cache
    /// - Task ID
    /// - Sub-job count
    fn clear(&mut self) {
        self.cache = None;
        self.task_id = i64::MIN;
        self.sub_job_count = 0;
        self.sub_jobs.truncate(super::SUB_JOB_RESERVE_COUNT);
        self.sub_jobs.shrink_to_fit();
    }
}

impl WarmupJob {
    /// Adds a new sub-job to this warmup job.
    ///
    /// # Arguments
    /// * `sub_id` - ID for the new sub-job
    /// * `shard_id` - ID of the shard
    /// * `file_pos` - Position in the entry file
    fn add(&mut self, sub_id: u32, shard_id: usize, file_pos: i64) {
        let sub_job = self.sub_jobs.get_mut(0).unwrap();
        sub_job.reinit(sub_id, shard_id, file_pos);
    }

    /// Clears this warmup job for reuse.
    ///
    /// Resets:
    /// - Cache
    /// - Data sender
    fn clear(&mut self) {
        self.cache = None;
    }
}

impl Default for WarmupJob {
    /// Creates a new WarmupJob with default settings.
    ///
    /// # Returns
    /// A new WarmupJob with:
    /// - No cache
    /// - No data sender
    /// - One empty sub-job
    fn default() -> Self {
        Self {
            cache: None,
            sub_jobs: [SubJob::new(0, 0, 0)],
        }
    }
}

impl JobManager {
    /// Creates a new JobManager instance.
    ///
    /// # Arguments
    /// * `uring_count` - Number of io_uring instances
    /// * `uring_size` - Size of each io_uring instance
    ///
    /// # Returns
    /// A new JobManager instance
    pub fn new(uring_count: usize, uring_size: u32) -> Self {
        let (bck_sender, _) = bounded(1);

        // Soft cap: don't oversubscribe CPU and don't exceed shard parallelism.
        let hw = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(uring_count)
            .max(1);
        let uring_count = uring_count.min(hw).min(SHARD_COUNT).max(1);

        JobManager {
            update_buffers: Vec::with_capacity(SHARD_COUNT),
            entry_files: Vec::with_capacity(SHARD_COUNT),
            done_chans: Vec::with_capacity(SHARD_COUNT),
            bck_sender,
            warmup_bck_senders: Vec::with_capacity(0),
            sub_id_senders: Vec::with_capacity(0),
            uring_count,
            uring_size,
            prune_receiver: None,
        }
    }

    /// Adds a shard to the JobManager.
    ///
    /// # Arguments
    /// * `update_buffer` - Buffer for updates
    /// * `entry_file` - File containing entries
    /// * `done_chan` - Channel for completion notifications
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

    /// Adds a sub-job and checks if it needs to read from SSD.
    ///
    /// # Arguments
    /// * `j` - Reference to the parent job
    /// * `sj` - Reference to the sub-job
    ///
    /// # Returns
    /// `true` if the sub-job needs to read from SSD, `false` otherwise
    fn add_sub_job(&self, j: &Job, sj: &SubJob) -> bool {
        // try to insert a locked entry_pos
        let cache = &j.get_entry_cache();
        let cache_hit = cache.contains(sj.shard_id as usize, sj.file_pos);
        if cache_hit {
            return false; // no need to fetch
        }
        let update_buffer = &self.update_buffers[sj.shard_id as usize];
        // in 'get_entry_bz_at', 'curr_buf' is None because we cannot read it
        let mut buf = ReadBuf::new();
        let in_disk = update_buffer.get_entry_bz(sj.file_pos, &mut buf);
        if !buf.is_empty() {
            let e = buf.as_entry_bz();
            cache.insert(sj.shard_id as usize, sj.file_pos, &e);
            return false;
        }
        in_disk
    }

    /// Builds job channels for task and warmup jobs.
    ///
    /// # Returns
    /// A tuple containing:
    /// - Arc reference to self
    /// - Receiver for task jobs
    /// - Vector of receivers for warmup jobs
    fn build_job_channel(
        mut self,
    ) -> (
        Arc<Self>,
        Receiver<Box<TaskJob>>,
        Vec<Receiver<Box<WarmupJob>>>,
    ) {
        let (bck_sender, bck_receiver) = unbounded();

        for _ in 0..super::JOB_COUNT {
            bck_sender.send(Box::new(TaskJob::default())).unwrap();
        }

        let mut warmup_bck_senders = Vec::with_capacity(self.uring_count);
        let mut warmup_bck_receivers = Vec::with_capacity(self.uring_count);
        for _ in 0..self.uring_count {
            let (sender, receiver) = unbounded();
            for _ in 0..super::WARMUP_JOB_COUNT {
                sender.send(Box::new(WarmupJob::default())).unwrap();
            }
            warmup_bck_senders.push(sender);
            warmup_bck_receivers.push(receiver);
        }
        self.bck_sender = bck_sender;
        self.warmup_bck_senders = warmup_bck_senders;
        (Arc::new(self), bck_receiver, warmup_bck_receivers)
    }

    /// Recycles a job object after completion.
    ///
    /// # Arguments
    /// * `job` - Job to recycle
    /// * `bz` - Buffer containing entry data
    fn recycle_job_object(&self, job: Arc<Job>) {
        if let Some(job) = Arc::<Job>::into_inner(job) {
            match job {
                Job::Task(mut j, mut disk) => {
                    disk.clear();
                    j.clear();
                    self.bck_sender.send(j).unwrap();
                }
                Job::Warmup(mut j, mut disk) => {
                    disk.clear();
                    let uring_id = j.sub_jobs[0].get_uring_id(self.uring_count);
                    j.clear();
                    self.warmup_bck_senders[uring_id].send(j).unwrap();
                }
            };
        }
    }

    /// Starts threads to drive io_urings.
    ///
    /// # Arguments
    /// * `uring_count` - Number of io_uring instances
    ///
    /// # Returns
    /// A tuple containing:
    /// - Arc reference to self
    /// - Receiver for task jobs
    /// - Vector of receivers for warmup jobs
    pub fn start_threads(
        mut self,
        uring_count: usize,
    ) -> (
        Arc<Self>,
        Receiver<Box<TaskJob>>,
        Vec<Receiver<Box<WarmupJob>>>,
    ) {
        // Caller might pass a stale value; always use the internally capped count.
        let uring_count = uring_count.min(self.uring_count).max(1);

        self.sub_id_senders = Vec::with_capacity(uring_count);
        let mut sub_id_receivers: Vec<Receiver<Arc<Job>>> = Vec::with_capacity(uring_count);

        // 创建N个通道
        let mut prune_txs = Vec::with_capacity(uring_count);
        let mut prune_rxs = Vec::with_capacity(uring_count);

        for _ in 0..uring_count {
            let (sub_tx, sub_rx) = unbounded();
            self.sub_id_senders.push(sub_tx);
            sub_id_receivers.push(sub_rx);
            
            // 控制通道通常比较小，bounded 即可
            let (ptx, prx) = crossbeam::channel::bounded::<PruneCmd>(128);
            prune_txs.push(ptx);
            prune_rxs.push(prx);
        }

        
        // [新增] 2. 启动一个 Dispatcher 线程 (广播员)
        // 它的唯一工作就是从总接收口拿消息，复制 N 份发给每个 Worker
        let master_prune_rx = self.prune_receiver.as_ref().expect("Prune receiver not set").clone(); // 假设你在 new() 里已经把 Receiver 放进 self 了
        
        std::thread::spawn(move || {
            while let Ok(cmd) = master_prune_rx.recv() {
                for tx in &prune_txs {
                    let _ = tx.send(cmd); // 忽略发送错误（如果 worker 挂了）
                }
            }
        });

        let (job_man, bck_receiver, warmup_bck_receivers) = self.build_job_channel();
        
        for _ in 0..uring_count {
            let sub_id_receiver = sub_id_receivers.pop().unwrap();
            let prune_receiver = prune_rxs.pop().unwrap(); // [新增] 拿出专属通道
            let job_man = job_man.clone();
            std::thread::spawn(move || {
                // 将 prune_receiver 传入 loop
                Self::uring_loop(job_man, sub_id_receiver, prune_receiver);
            });
        }
        (job_man, bck_receiver, warmup_bck_receivers)
    }

    /// Adds a job to be processed.
    ///
    /// # Arguments
    /// * `job` - Job to add
    /// * `only_disk` - Whether to only read from disk
    fn add_job(&self, job: Job) {
        let mut job = job;

        for i in 0..job.get_sub_job_count() {
            let sj = job.get_sub_job(i);
            let need_read_ssd = self.add_sub_job(&job, sj);
            sj.need_read_ssd.store(need_read_ssd, SC);
        }

        // Pin the whole job onto a single io_uring worker to avoid cross-worker sharing.
        let uring_id = job.routing_uring_id(self.sub_id_senders.len());
        job.disk_sub_ids_mut().clear();

        for i in 0..job.get_sub_job_count() {
            // Keep borrows strictly scoped: read fields first, then mutate disk_sub_ids.
            let (need_read_ssd, sub_id) = {
                let sj = job.get_sub_job(i);
                (sj.need_read_ssd.load(SC), sj.sub_id)
            };
            if need_read_ssd {
                job.disk_sub_ids_mut().push(sub_id);
            }
        }

        let job = Arc::new(job);
        if !job.disk_sub_ids().is_empty() {
            self.sub_id_senders[uring_id].send(job).unwrap();
        } else {
            // No disk work to do; recycle immediately.
            self.recycle_job_object(job);
        }
    }

    /// Gets a read entry for io_uring (backward compatible version).
    ///
    /// # Arguments
    /// * `file` - File to read from
    /// * `offset` - Offset within the file
    /// * `buf` - Buffer to read into
    /// * `id` - Buffer ID
    ///
    /// # Returns
    /// io_uring entry
    pub fn get_read_e(file: &File, offset: i64, buf: &mut [u8], id: u32) -> squeue::Entry {
        let fd = file.as_raw_fd();
        let buf_ptr = buf.as_ptr() as *mut u8;
        // we are sure one entry is no larger than BLK_SIZE
        let end = offset % BLK_SIZE + (DEFAULT_ENTRY_SIZE as i64);
        let blk_count = if end > BLK_SIZE { 2 } else { 1 };
        let aligned_offset = (offset / BLK_SIZE) * BLK_SIZE;
        let read_len = (BLK_SIZE * blk_count) as u32;

        opcode::Read::new(types::Fd(fd), buf_ptr, read_len)
            .offset(aligned_offset as u64)
            .build()
            .user_data(id as u64)
    }

    /// Build io_uring read request
    /// - Uses ReadFixed to leverage pre-registered buffers
    /// - If fixed_slot available, use Fixed fd; otherwise use regular fd
    ///
    /// # Arguments
    /// * `fd` - Raw file descriptor
    /// * `fixed_slot` - Registered fd slot (if available)
    /// * `offset` - File offset
    /// * `buf` - Buffer
    /// * `buf_id` - Buffer ID (serves as both user_data and buffer index)
    ///
    /// # Returns
    /// io_uring entry
    fn build_read_entry(
        fd: RawFd,
        fixed_slot: Option<u16>,
        offset: i64,
        buf: &mut [u8],
        buf_id: u32,
    ) -> squeue::Entry {
        // we are sure one entry is no larger than BLK_SIZE
        let end = offset % BLK_SIZE + (DEFAULT_ENTRY_SIZE as i64);
        let blk_count = if end > BLK_SIZE { 2 } else { 1 };
        let aligned_offset = (offset / BLK_SIZE) * BLK_SIZE;
        let read_len = (BLK_SIZE * blk_count) as u32;

        // Use ReadFixed: leverage pre-registered fixed buffers, avoid per-I/O page pin overhead
        // buf_id serves as both user_data and buffer index
        match fixed_slot {
            Some(slot) => {
                // Use Fixed fd: avoid fd reference count overhead
                opcode::ReadFixed::new(
                    types::Fixed(slot as u32),
                    buf.as_mut_ptr(),
                    read_len,
                    buf_id as u16,
                )
                .offset(aligned_offset as u64)
                .build()
                .user_data(buf_id as u64)
            }
            None => {
                // Fall back to regular fd (when exceeding MAX_REGISTERED_FILES)
                opcode::ReadFixed::new(types::Fd(fd), buf.as_mut_ptr(), read_len, buf_id as u16)
                    .offset(aligned_offset as u64)
                    .build()
                    .user_data(buf_id as u64)
            }
        }
    }

    /// Main loop for io_uring processing.
    ///
    /// # Arguments
    /// * `job_man` - Reference to the job manager
    /// * `sub_id_receiver` - Receiver for sub-job IDs
    fn uring_loop(
        job_man: Arc<Self>,
        sub_id_receiver: Receiver<Arc<Job>>,
        prune_receiver: Receiver<PruneCmd>
    ) {
        let mut buf_list = BufList::new(job_man.uring_size);
        // For each in-flight buffer id, remember which (job_slot, sub_id) it belongs to.
        // job_slot is an index into `job_slots`.
        let mut sub_id_vec: Vec<(u32, usize)> = repeat_with(|| (0u32, usize::MAX))
            .take(job_man.uring_size as usize)
            .collect();
        let mut ring = IoUring::<squeue::Entry, cqueue::Entry>::builder()
            .setup_single_issuer()
            .build(job_man.uring_size)
            .unwrap();
        // keep files being read in hashmap to prevent it from being dropped

        // Register fixed buffers, use ReadFixed to avoid per-I/O page pin overhead
        let iovecs = buf_list.as_iovecs();
        unsafe {
            ring.submitter()
                .register_buffers(&iovecs)
                .expect("failed to register buffers");
        }

        // Pre-allocate file descriptor slots (sparse registration) for lazy fd registration
        ring.submitter()
            .register_files_sparse(MAX_REGISTERED_FILES)
            .expect("failed to register sparse files");

        // Lazy-registered file descriptor manager
        let mut registered_files = RegisteredFiles::new();

        // In-flight IO count (submitted but not yet completed).
        let mut in_flight = 0usize;

        // Pending work queue: (job_slot, sub_id).
        let mut pending: VecDeque<(usize, u32)> = VecDeque::new();

        // Per-uring job table: keep exactly one Arc<Job> alive per job to avoid
        // per-IO Arc clone/drop ping-pong on the refcount cacheline.
        let mut job_slots: Vec<Option<JobSlot>> = Vec::new();
        let mut free_slots: Vec<usize> = Vec::new();
        let mut job_ptr_to_slot: HashMap<usize, usize> = HashMap::new();

        // Helper: intern a job Arc into the local table, returning slot index.
        fn intern_job(
            job: Arc<Job>,
            add_remaining: usize,
            job_slots: &mut Vec<Option<JobSlot>>,
            free_slots: &mut Vec<usize>,
            job_ptr_to_slot: &mut HashMap<usize, usize>,
        ) -> usize {
            let ptr = Arc::as_ptr(&job) as usize;
            if let Some(&slot_idx) = job_ptr_to_slot.get(&ptr) {
                // Already have a canonical Arc in this uring worker; drop incoming clone.
                if let Some(slot) = job_slots[slot_idx].as_mut() {
                    slot.remaining += add_remaining;
                }
                return slot_idx;
            }

            let slot_idx = free_slots.pop().unwrap_or_else(|| {
                job_slots.push(None);
                job_slots.len() - 1
            });
            job_ptr_to_slot.insert(ptr, slot_idx);
            job_slots[slot_idx] = Some(JobSlot {
                job,
                remaining: add_remaining,
            });
            slot_idx
        }

        // Receive one batch (blocking or non-blocking) and enqueue its sub-ids.
        fn recv_one_job(
            blocking: bool,
            sub_id_receiver: &Receiver<Arc<Job>>,
            pending: &mut VecDeque<(usize, u32)>,
            job_slots: &mut Vec<Option<JobSlot>>,
            free_slots: &mut Vec<usize>,
            job_ptr_to_slot: &mut HashMap<usize, usize>,
        ) -> Option<()> {
            let job = if blocking {
                match sub_id_receiver.recv() {
                    Ok(b) => b,
                    Err(_) => return None,
                }
            } else {
                match sub_id_receiver.try_recv() {
                    Ok(b) => b,
                    Err(err) => match err {
                        crossbeam::channel::TryRecvError::Empty => return Some(()),
                        crossbeam::channel::TryRecvError::Disconnected => return None,
                    },
                }
            };

            let disk_count = job.disk_sub_ids().len();
            if disk_count == 0 {
                // Should not happen (sender filters), but keep it safe.
                return Some(());
            }

            let slot_idx = intern_job(job, disk_count, job_slots, free_slots, job_ptr_to_slot);

            // Enqueue disk sub-ids from the canonical Arc kept in the slot.
            let ids: Vec<u32> = job_slots[slot_idx]
                .as_ref()
                .expect("job slot must exist")
                .job
                .disk_sub_ids()
                .to_vec();
            for sub_id in ids {
                pending.push_back((slot_idx, sub_id));
            }
            Some(())
        }

        loop {
            while let Ok(cmd) = prune_receiver.try_recv() {
                let key: u64 = ((cmd.shard_id as u64) << 56) | (cmd.seg_id as u64);
                // 调用我们在第一步实现的同步驱逐
                registered_files.evict_sync(&ring, key);
            }

            let mut sq = ring.submission();
            // If we pushed any SQEs in this iteration, prefer waiting for a minimal number of CQEs
            // to keep the pipeline saturated and avoid waiting for too many completions when
            // submissions are partial.
            let mut pushed_any = false;
            while buf_list.has_free_buf() && !sq.is_full() {
                // Ensure we have some pending work.
                if pending.is_empty() {
                    let ok = if in_flight == 0 {
                        recv_one_job(
                            true,
                            &sub_id_receiver,
                            &mut pending,
                            &mut job_slots,
                            &mut free_slots,
                            &mut job_ptr_to_slot,
                        )
                    } else {
                        recv_one_job(
                            false,
                            &sub_id_receiver,
                            &mut pending,
                            &mut job_slots,
                            &mut free_slots,
                            &mut job_ptr_to_slot,
                        )
                    };
                    if ok.is_none() {
                        return;
                    }
                    if pending.is_empty() {
                        break;
                    }
                }

                let (job_slot, sub_id) = pending.pop_front().unwrap();
                let job_ref = job_slots[job_slot].as_ref().expect("job slot must exist");
                let sj = job_ref.job.get_sub_job(sub_id as usize);
                let (buf, id) = buf_list.allocate_buf();
                let (file_guard, offset) =
                    job_man.entry_files[sj.shard_id as usize].get_file_ref(sj.file_pos);
                let file: &std::fs::File = file_guard.as_ref().unwrap();
                let raw_fd = file.as_raw_fd();

                // Use (shard_id, segment_id) as registration key to avoid fd reuse causing fixed-file slot to read wrong file
                let entry_file = &job_man.entry_files[sj.shard_id as usize];
                let logical_off = sj.file_pos % qmdb_common::def::HPFILE_RANGE;
                let seg_id = logical_off / entry_file.segment_size();
                let key: u64 =
                    ((sj.shard_id as u64) << 56) | ((seg_id as u64) & ((1u64 << 56) - 1));

                // Look up registered key, or mark as pending registration
                let fixed_slot = registered_files.lookup_or_mark_pending(key, raw_fd);

                // Build read request (prefer Fixed fd, fall back to regular fd when limit exceeded)
                let read_e = Self::build_read_entry(raw_fd, fixed_slot, offset, buf, id);
                sub_id_vec[id as usize] = (sj.sub_id, job_slot);
                unsafe { sq.push(&read_e).unwrap() };
                DIO_READ_COUNT.fetch_add(1, Ordering::Relaxed);
                pushed_any = true;
                in_flight += 1;
            }
            drop(sq);

            // Process pending fd registrations (after submission queue is released)
            registered_files.flush_pending(&ring);

            if in_flight > 0 {
                // Dynamic wait strategy to reduce syscall frequency:
                // - If no more work can be submitted (pending empty or buffers exhausted),
                //   batch-wait for more completions to amortize syscall overhead.
                // - If there's pending work and free buffers, wait for fewer completions
                //   to keep the submission queue saturated and maintain pipeline throughput.
                let buf_free = buf_list.has_free_buf();

                let wait_nr = if !pending.is_empty() && !buf_free {
                    1 
                } else if pending.is_empty() {
                    // 确实没新任务了，我们可以稍微等批量完成，减少系统调用开销
                    (in_flight / 2).clamp(MIN_WAIT, MAX_WAIT)
                } else if pushed_any {
                    // 刚塞了新任务进去，快速唤醒检查
                    MIN_WAIT
                } else {
                    MIN_WAIT
                };

                ring.submit_and_wait(wait_nr).unwrap();
            }
            for cqe in ring.completion() {
                // Defensive: avoid usize underflow in release builds; in_flight should never be 0 here.
                if in_flight == 0 {
                    panic!("io_uring completion received but in_flight == 0");
                }
                in_flight -= 1;
                let buf_id = cqe.user_data() as u32;
                let mut tmp = (0u32, usize::MAX);
                std::mem::swap(&mut sub_id_vec[buf_id as usize], &mut tmp);
                let (sub_id, job_slot) = (tmp.0, tmp.1);
                let n = cqe.result();
                if n <= 0 {
                    panic!("AA uring read error");
                }
                // Finish using a shared reference; no Arc clone needed per completion.
                {
                    let slot = job_slots[job_slot].as_ref().expect("job slot must exist");
                    job_man.uring_finish_sub_job(
                        buf_id,
                        &slot.job,
                        sub_id,
                        &mut buf_list,
                        n as u32,
                    );
                }
                buf_list.return_buf(buf_id);

                // Decrement remaining for this job in this uring worker.
                let should_recycle = {
                    let slot = job_slots[job_slot].as_mut().unwrap();
                    slot.remaining -= 1;
                    slot.remaining == 0
                };
                if should_recycle {
                    let slot = job_slots[job_slot].take().unwrap();
                    let ptr = Arc::as_ptr(&slot.job) as usize;
                    job_ptr_to_slot.remove(&ptr);
                    free_slots.push(job_slot);
                    job_man.recycle_job_object(slot.job);
                }
            }
        }
    }

    /// Finishes processing a sub-job.
    ///
    /// # Arguments
    /// * `buf_id` - ID of the buffer
    /// * `job` - Job being processed
    /// * `sub_id` - ID of the sub-job
    /// * `buf_list` - Reference to the buffer list
    /// * `read_bytes` - Number of bytes actually read by io_uring
    fn uring_finish_sub_job(
        &self,
        buf_id: u32,
        job: &Arc<Job>,
        sub_id: u32,
        buf_list: &mut BufList,
        read_bytes: u32,
    ) {
        let idx = sub_id as usize;
        let sj = job.get_sub_job(idx);
        let offset = (sj.file_pos % BLK_SIZE) as usize;
        let buf = buf_list.get_buf_mut(buf_id, offset);

        // Decode entry length from the header.
        let len = EntryBz::get_entry_len(buf);

        // Ensure the underlying read actually covered the full entry.
        let needed = offset
            .checked_add(len)
            .expect("overflow when computing needed bytes for entry");
        if (read_bytes as usize) < needed {
            panic!(
                "short read from io_uring: expected at least {} bytes (offset {} + len {}), got {}",
                needed, offset, len, read_bytes
            );
        }

        // Decrypt in-place so that cache always stores plaintext entries,
        // keeping behavior consistent with other read paths.
        #[cfg(target_os = "linux")]
        {
            let entry_file = &self.entry_files[sj.shard_id as usize];
            entry_file.decrypt_entry_in_place(sj.file_pos, &mut buf[..len]);
        }

        let e = EntryBz { bz: &buf[..len] };
        if len == 0 || len > BLK_SIZE as usize || e.key().len() < 20 {
            panic!(
                "AA file_pos={} offset={} len={}\n buf={:?}",
                sj.file_pos, offset, len, buf,
            );
        }
        job.get_entry_cache()
            .insert(sj.shard_id as usize, sj.file_pos, &e);
    }
}

/// A sub-job representing a single I/O operation.
///
/// Contains:
/// - Sub-job ID
/// - Shard ID
/// - File position
/// - SSD read status
#[derive(Debug)]
struct SubJob {
    /// ID of this sub-job
    sub_id: u32,
    /// ID of the target shard
    shard_id: u8,
    /// Whether this job needs to read from SSD
    need_read_ssd: AtomicBool,
    /// Position in the entry file
    file_pos: i64,
}

impl SubJob {
    /// Creates a new SubJob.
    ///
    /// # Arguments
    /// * `sub_id` - ID for the new sub-job
    /// * `shard_id` - ID of the shard
    /// * `file_pos` - Position in the entry file
    ///
    /// # Returns
    /// A new SubJob instance
    fn new(sub_id: u32, shard_id: usize, file_pos: i64) -> Self {
        SubJob {
            sub_id,
            shard_id: shard_id as u8,
            file_pos,
            need_read_ssd: AtomicBool::new(false),
        }
    }

    /// Reinitializes this sub-job with new values.
    ///
    /// # Arguments
    /// * `sub_id` - New sub-job ID
    /// * `shard_id` - New shard ID
    /// * `file_pos` - New file position
    fn reinit(&mut self, sub_id: u32, shard_id: usize, file_pos: i64) {
        self.sub_id = sub_id;
        self.shard_id = shard_id as u8;
        self.file_pos = file_pos;
        self.need_read_ssd.store(false, SC);
    }

    /// Calculates the io_uring instance ID for a given position.
    ///
    /// # Arguments
    /// * `file_pos` - Position in the entry file
    /// * `shard_id` - ID of the shard
    /// * `uring_count` - Total number of io_uring instances
    ///
    /// # Returns
    /// The calculated io_uring instance ID
    fn uring_id(file_pos: i64, shard_id: usize, uring_count: usize) -> usize {
        let mut r = file_pos as usize;
        r = (r >> 10) ^ (r >> 3) ^ shard_id;
        r % uring_count
    }

    /// select a io_uring in a pseudo-random way
    ///
    /// # Arguments
    /// * `uring_count` - Total number of io_uring instances
    ///
    /// # Returns
    /// The io_uring instance ID for this sub-job
    fn get_uring_id(&self, uring_count: usize) -> usize {
        Self::uring_id(self.file_pos, self.shard_id as usize, uring_count)
    }
}

/// Manages a pool of direct I/O buffers.
///
/// Provides:
/// - Buffer allocation
/// - Buffer recycling
/// - Buffer access
pub struct BufList {
    /// List of allocated buffers
    buf_list: Vec<AlignedBuf512>,
    /// List of free buffer IDs
    free_buf_ids: Vec<u32>,
}

impl BufList {
    /// Creates a new BufList.
    ///
    /// # Arguments
    /// * `n` - Number of buffers to allocate
    ///
    /// # Returns
    /// A new BufList instance
    pub fn new(n: u32) -> Self {
        let mut res = Self {
            buf_list: Vec::with_capacity(n as usize),
            free_buf_ids: Vec::with_capacity(n as usize),
        };
        for i in 0..n {
            let buf = AlignedBuf512::new_zeroed((BLK_SIZE * 2) as usize);
            res.buf_list.push(buf);
            res.free_buf_ids.push(i);
        }
        res
    }

    /// Checks if there are free buffers available.
    ///
    /// # Returns
    /// `true` if there are free buffers, `false` otherwise
    pub fn has_free_buf(&self) -> bool {
        !self.free_buf_ids.is_empty()
    }

    /// Allocates a buffer from the pool.
    ///
    /// # Returns
    /// A tuple containing:
    /// - A mutable reference to the allocated buffer
    /// - The buffer ID
    pub fn allocate_buf(&mut self) -> (&mut [u8], u32) {
        let id = self.free_buf_ids.pop().unwrap();
        (&mut self.buf_list[id as usize][..], id)
    }

    /// Gets a reference to a specific buffer.
    ///
    /// # Arguments
    /// * `id` - ID of the buffer
    /// * `offset` - Offset within the buffer
    ///
    /// # Returns
    /// A slice of the buffer starting at the specified offset
    pub fn get_buf(&self, id: u32, offset: usize) -> &[u8] {
        &self.buf_list[id as usize][offset..]
    }

    /// Gets a mutable reference to a specific buffer.
    ///
    /// # Arguments
    /// * `id` - ID of the buffer
    /// * `offset` - Offset within the buffer
    ///
    /// # Returns
    /// A mutable slice of the buffer starting at the specified offset
    pub fn get_buf_mut(&mut self, id: u32, offset: usize) -> &mut [u8] {
        &mut self.buf_list[id as usize][offset..]
    }

    /// Returns a buffer to the pool.
    ///
    /// # Arguments
    /// * `id` - ID of the buffer to return
    pub fn return_buf(&mut self, id: u32) {
        self.free_buf_ids.push(id);
    }

    /// Returns all buffers as iovecs for io_uring registration
    fn as_iovecs(&self) -> Vec<libc::iovec> {
        self.buf_list
            .iter()
            .map(|buf| libc::iovec {
                iov_base: buf.as_ptr() as *mut libc::c_void,
                iov_len: buf.len(),
            })
            .collect()
    }
}

