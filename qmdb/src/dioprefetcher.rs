use crate::def::{SHARD_COUNT, SUB_JOB_RESERVE_COUNT};
use crate::entryfile::{EntryBuffer, EntryBz, EntryCache, EntryFile};
use crate::indexer::Indexer;
use crate::tasks::TaskHub;
use crate::utils::byte0_to_shard_id;
use crate::utils::bytescache::new_cache_pos;
use chrono::Utc;
use crate::def::{IN_BLOCK_IDX_MASK};
use std::time::Instant;
use dashmap::DashMap;
use hpfile::{direct_io, IO_BLK_SIZE};
use lazy_static::lazy_static;
use log::error;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;
use std::usize;
use threadpool::ThreadPool;
use {
    crate::def::{IN_BLOCK_IDX_BITS, JOB_COUNT},
    std::collections::HashMap,
    std::fs::File,
    std::os::unix::io::AsRawFd,
    std::sync::mpsc::sync_channel,
};

use io_uring::{cqueue, opcode, squeue, types, IoUring};

lazy_static! {
    pub static ref NULL_CACHE: Arc<EntryCache> = Arc::new(EntryCache::new_uninit());
}

const TASK_ID: i64 = 0x0000004e000008;

pub struct Prefetcher {
    task_hub: Arc<dyn TaskHub>,
    cache: Arc<EntryCache>,
    indexer: Arc<Indexer>,
    job_man: Arc<JobManager>,
    tpool: Arc<ThreadPool>,
    bck_receiver: Receiver<Box<Job>>,
}

pub struct JobManager {
    update_buffers: Vec<Arc<EntryBuffer>>,
    entry_files: Vec<Arc<EntryFile>>,
    done_chans: Vec<SyncSender<(i64, i64)>>,
    sub_id_to_job: DashMap<u64, Arc<Box<Job>>>,
    bck_sender: SyncSender<Box<Job>>,
    sub_id_senders: Vec<SyncSender<u64>>,
    uring_count: usize,
    uring_size: u32,
    sub_id_chan_size: usize,
}

impl Prefetcher {
    pub fn new(
        task_hub: Arc<dyn TaskHub>,
        cache: Arc<EntryCache>,
        indexer: Arc<Indexer>,
        tpool: Arc<ThreadPool>,
        job_man: JobManager,
    ) -> Self {
        let uring_count = job_man.uring_count;
        let sub_id_chan_size = job_man.uring_count;
        let (job_man, bck_receiver) = job_man.start_threads(uring_count, sub_id_chan_size);
        Self {
            task_hub,
            cache,
            indexer,
            job_man,
            tpool,
            bck_receiver,
        }
    }

    pub fn start_threads(mut self, task_receiver: Receiver<i64>) {
        std::thread::spawn(move || loop {
            let task_id = task_receiver.recv().unwrap_or(-1);
            if task_id == -1 {
                error!("task receiver in prefetcher exit!");
                return;
            }
            self.run_task(task_id);
        });
    }

    fn run_task(&mut self, task_id: i64) {
        //if (task_id >> 24) > 76 {
        //    println!("HH run_task {:#016x}", task_id);
        //}
        let (cache_for_new_block, end_block) = self.task_hub.check_begin_end(task_id);
        if cache_for_new_block.is_some() {
            self.cache = cache_for_new_block.unwrap();
            
            // ------------- add timestamp -------------
            println!(
                "PIPELINE_LOG: PREFETCHER_ENTER height={} timestamp={}",
                task_id >> IN_BLOCK_IDX_BITS,
                Utc::now().to_rfc3339()
            );
            // -----------------------------------------

        }
        let mut next_task_id = task_id + 1;
        let height = task_id >> IN_BLOCK_IDX_BITS;
        if end_block {
            //next_task_id is the first task of the next block
            next_task_id = (height + 1) << IN_BLOCK_IDX_BITS;
        }
        let task_hub = self.task_hub.clone();
        let change_sets = task_hub.get_change_sets(task_id).clone();
        //println!("AA to_get_job task_id={:#016x}", task_id);

        // let start_wait = Instant::now(); // <--- 计时开始
        let mut job = self.bck_receiver.recv().unwrap();
        // let wait_job_time = start_wait.elapsed(); // <--- 计时结束
        // if wait_job_time.as_micros() > 1000 {
            //  println!("WARN: Job Pool Starvation! Waited {:?} for a free job.", wait_job_time);
        // }

        job.task_id = task_id;
        job.next_task_id = next_task_id;
        job.cache = self.cache.clone();
        
        // ---------------- add this -----------------
        job.is_last_task_of_block = end_block;
        // -------------------------------------------

        let indexer = self.indexer.clone();
        let change_sets = change_sets.clone();
        let job_man = self.job_man.clone();
        self.tpool.execute(move || {

            // ------------- add timestamp -------------
            if (task_id & IN_BLOCK_IDX_MASK) == 0 {
                println!(
                    "PIPELINE_LOG: PREFETCHER_READ_START height={} timestamp={}",
                    height,
                    Utc::now().to_rfc3339()
                );
            }
            // -----------------------------------------
            
            let mut sub_id = (task_id << 32) as u64;

            // let start_index = Instant::now();
            for change_set in change_sets.iter() {
                change_set.run_all(|op, kh: &[u8; 32], _k, _v, _r| {
                    let shard_id = byte0_to_shard_id(kh[0]);
                    //if shard_id==0 && task_id==TASK_ID  {
                    //    println!("HH kh={:?}", &kh[..4]);
                    //}
                    indexer.for_each(height, op, &kh[..], |_k, offset| -> bool {
                        //if shard_id==0 && task_id==TASK_ID  {
                        //    println!("HH add sub_id={:#016x} {:?} {}", sub_id, &kh[..4], job.sub_job_count);
                        //}
                        job.add(sub_id, shard_id, offset);
                        sub_id += 1;
                        false // do not exit loop
                    });
                });
            }

            // let index_time = start_index.elapsed(); // <--- 计时结束

            // 如果索引耗时超过 5ms (对于内存操作来说这很久了)，说明 CPU 是瓶颈
            // if index_time.as_millis() > 5 {
            //      println!("WARN: Indexing Slow! Took {:?}.", index_time);
            // }
            //if task_id==TASK_ID {
            //    println!("HH read_counts[0]={}", job.read_counts[0].load(Ordering::SeqCst));
            //}
            //println!("AA mid task_id={:#016x}", task_id);

            // --------------------- delete this ----------------
            // for shard_id in 0..SHARD_COUNT {
            //     if job.read_counts[shard_id].load(Ordering::SeqCst) == 0 {
            //         //if (task_id >> 24) == 1 {
            //         //    println!("S{} {:#016x}-{:#016x} a", shard_id, task_id, next_task_id);
            //         //}
            //         job_man.done_chans[shard_id]
            //             .send((task_id, next_task_id))
            //             .unwrap();
            //     }
            // }
            //job_man.add_job(Arc::new(job));

            // ------------------------ add this -------------------
            let job_arc = Arc::new(job); // job_arc 现在是 Arc<Box<Job>>
            for shard_id in 0..SHARD_COUNT {
                if job_arc.read_counts[shard_id].load(Ordering::SeqCst) == 0 {
                    job_man.handoff_and_check_completion(&job_arc, shard_id); 
                }
            }
            job_man.add_job(job_arc);

        });
    }
}

impl JobManager {
    pub fn new(uring_count: usize, uring_size: u32, sub_id_chan_size: usize) -> Self {
        let (bck_sender, _) = sync_channel(1);
        JobManager {
            update_buffers: Vec::with_capacity(SHARD_COUNT),
            entry_files: Vec::with_capacity(SHARD_COUNT),
            done_chans: Vec::with_capacity(SHARD_COUNT),
            sub_id_to_job: DashMap::new(),
            bck_sender,
            sub_id_senders: Vec::with_capacity(0),
            uring_count,
            uring_size,
            sub_id_chan_size,
        }
    }

    pub fn add_shard(
        &mut self,
        update_buffer: Arc<EntryBuffer>,
        entry_file: Arc<EntryFile>,
        done_chan: SyncSender<(i64, i64)>,
    ) {
        self.update_buffers.push(update_buffer);
        self.entry_files.push(entry_file);
        self.done_chans.push(done_chan);
    }

    fn handoff_and_check_completion(&self, job: &Arc<Box<Job>>, shard_id: usize) {
        // 1. 移交任务给 Updater 线程
        self.done_chans[shard_id]
            .send((job.task_id, job.next_task_id))
            .unwrap();

        // 2. 检查这是否是 *Block* 的最后一个 *Task*
        if job.is_last_task_of_block {
            // 3. 递减“剩余分片”计数器。
            // 如果这是最后一个分片 (计数器从 1 变为 0)，则打印日志
            if job.shards_remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
                
                // ++++++++++ 3. PREFETCHER_LEAVE (Block 离开) ++++++++++
                println!(
                    "PIPELINE_LOG: PREFETCHER_LEAVE height={} timestamp={}",
                    job.task_id >> IN_BLOCK_IDX_BITS,
                    Utc::now().to_rfc3339()
                );
            }
        }
    }

    // return whether need to read SSD
    fn add_sub_job(&self, j: &Box<Job>, sj: &SubJob) -> bool {
        let entry_pos = new_cache_pos();
        // try to insert a locked entry_pos
        let cache_hit = !j
            .cache
            .allocate_if_missing(sj.shard_id, sj.file_pos, entry_pos);
        if cache_hit {
            return false; // no need to fetch
        }
        let update_buffer = &self.update_buffers[sj.shard_id];
        // in 'get_entry_bz_at', 'curr_buf' is None because we cannot read it
        let (in_disk, have_accessed) = update_buffer.get_entry_bz(sj.file_pos, |entry_bz| {
            j.cache.insert(sj.shard_id, sj.file_pos, &entry_bz);
        });
        in_disk && !have_accessed
    }

    fn build_job_channel(mut self) -> (Arc<Self>, Receiver<Box<Job>>) {
        let (bck_sender, bck_receiver) = sync_channel(JOB_COUNT * 2);
        for _ in 0..JOB_COUNT {
            bck_sender.send(Box::new(Job::default())).unwrap();
        }
        self.bck_sender = bck_sender;
        (Arc::new(self), bck_receiver)
    }

    fn recycle_job_object(&self, job: Arc<Box<Job>>) {
        if let Some(mut job) = Arc::<Box<Job>>::into_inner(job) {
            job.clear();
            self.bck_sender.send(job).unwrap();
        }
    }

    // start threads to drive io_urings
    pub fn start_threads(
        mut self,
        uring_count: usize,
        sub_id_chan_size: usize,
    ) -> (Arc<Self>, Receiver<Box<Job>>) {
        self.sub_id_senders = Vec::with_capacity(uring_count);
        let mut sub_id_receivers = Vec::with_capacity(uring_count);
        for _ in 0..uring_count {
            let (sub_id_sender, sub_id_receiver) = sync_channel(sub_id_chan_size);
            self.sub_id_senders.push(sub_id_sender);
            sub_id_receivers.push(sub_id_receiver);
        }
        let (job_man, bck_receiver) = self.build_job_channel();
        for _ in 0..uring_count {
            let sub_id_receiver = sub_id_receivers.pop().unwrap();
            let job_man = job_man.clone();
            std::thread::spawn(move || {
                Self::uring_loop(job_man, sub_id_receiver);
            });
        }
        (job_man, bck_receiver)
    }

    fn add_job(&self, job: Arc<Box<Job>>) {
        for i in 0..job.sub_job_count {
            let sj = &job.sub_jobs[i];
            let need_read_ssd = self.add_sub_job(&*job, sj);
            sj.need_read_ssd.store(need_read_ssd, Ordering::SeqCst);
            //println!("AA SET id={:#016x} need={}", sj.sub_id, need_read_ssd);
            if need_read_ssd {
                self.sub_id_to_job.insert(sj.sub_id, job.clone());
            } else {
                if job.read_counts[sj.shard_id].fetch_sub(1, Ordering::SeqCst) == 1 {
                    // --------------- delete this --------------
                    // self.done_chans[sj.shard_id]
                    //     .send((job.task_id, job.next_task_id))
                    //     .unwrap();

                    // --------------- add this -----------------
                    self.handoff_and_check_completion(&job, sj.shard_id);
                }
            }
        }
        for i in 0..job.sub_job_count {
            let sj = &job.sub_jobs[i];
            let need_read_ssd = sj.need_read_ssd.load(Ordering::SeqCst);
            //println!("AA GET id={:#016x} need={}", sj.sub_id, need_read_ssd);
            if need_read_ssd {
                let n = sj.sub_id as usize;
                // select a io_uring in a pseudo-random way
                let idx = (n ^ (n >> 32)) % self.sub_id_senders.len();
                //println!("AA send sub_id {:#016x}", sj.sub_id);

                // let start_send = Instant::now(); // <--- 计时开始
                self.sub_id_senders[idx].send(sj.sub_id);
                // let send_duration = start_send.elapsed(); // <--- 计时结束
                // 如果发送 sub_id 都要等，说明 IO 队列满了！
                // if send_duration.as_micros() > 100 { 
                    // println!("WARN: IO Queue Full! Blocked {:?} sending sub_id.", send_duration);
                // }
            }
        }
        self.recycle_job_object(job);
    }

    fn get_read_e(&self, sj: &SubJob) -> (squeue::Entry, Arc<(File, bool)>) {
        let (file, offset) = self.entry_files[sj.shard_id].get_file_and_pos(sj.file_pos);
        let fd = file.0.as_raw_fd();
        let buf_ptr = sj.buf.as_ptr() as *mut u8;
        let blk_size = IO_BLK_SIZE as i64;
        // we are sure one entry is no larger than IO_BLK_SIZE
        let blk_count = if offset % blk_size == 0 { 1 } else { 2 };
        let aligned_offset = (offset / blk_size) * blk_size;
        let read_len = (IO_BLK_SIZE * blk_count) as u32;
        let e = opcode::Read::new(types::Fd(fd), buf_ptr, read_len)
            .offset(aligned_offset as u64)
            .build()
            .user_data(sj.sub_id);
        (e, file)
    }

    fn uring_loop(job_man: Arc<Self>, sub_id_receiver: Receiver<u64>) {
        let mut ring = IoUring::<squeue::Entry, cqueue::Entry>::builder()
            //.setup_sqpoll(5000)
            .setup_single_issuer()
            .build(job_man.uring_size)
            .unwrap();
        //new(URING_SIZE).unwrap();
        // keep files being read in hashmap to prevent it from being dropped
        let mut sub_id_to_file = HashMap::new();

        loop {
            let mut can_submit = false;
            let mut sq = ring.submission();
            while !sq.is_full() {
                //println!("AA sq is not full");
                if let Ok(sub_id) = sub_id_receiver.try_recv() {
                    //println!("AA get sub_id {:#016x}", sub_id);
                    can_submit = true;
                    let job = job_man.sub_id_to_job.get(&sub_id).unwrap();
                    let idx = (sub_id as u32) as usize;
                    let sj = &job.sub_jobs[idx];
                    let (read_e, file) = job_man.get_read_e(sj);
                    if !file.1 {
                        // still a file being appended
                        sub_id_to_file.insert(sub_id, file);
                    }
                    unsafe { sq.push(&read_e).unwrap() };
                } else {
                    break;
                }
            }
            drop(sq);
            if can_submit {
                ring.submit().unwrap();
            }
            let mut cq = ring.completion();
            if !cq.is_empty() {
                let cqe = cq.next().unwrap();
                let sub_id = cqe.user_data();
                if cqe.result() < 0 {
                    let old = job_man.sub_id_to_job.get(&sub_id);
                    let job = old.unwrap();
                    let idx = (sub_id as u32) as usize;
                    let sj = &job.sub_jobs[idx];
                    panic!("AA cqe file_pos={} sub_id={:#016x}", sj.file_pos, sub_id);
                }
                assert!(cqe.result() >= 0, "uring read error: {}", cqe.result());
                //println!("AA done sub_id {:#016x}", sub_id);
                job_man.uring_finish_sub_job(sub_id);
                sub_id_to_file.remove(&sub_id);
            }
        }
    }

    fn uring_finish_sub_job(&self, sub_id: u64) {
        let old = self.sub_id_to_job.remove(&sub_id);
        let job: Arc<Box<Job>> = old.unwrap().1;
        let idx = (sub_id as u32) as usize;
        let sj = &job.sub_jobs[idx];
        let offset = sj.file_pos as usize % IO_BLK_SIZE;
        let buf = &sj.buf[offset..];
        let len = EntryBz::get_entry_len(buf);
        let e = EntryBz { bz: &buf[..len] };
        if len > IO_BLK_SIZE || e.key().len() < 32 {
            panic!(
                "AA file_pos={} offset={} len={}\n buf={:?}\n sjbuf={:?}",
                sj.file_pos, offset, len, buf, sj.buf
            );
        }
        job.cache.insert(sj.shard_id, sj.file_pos, &e);
        // the last finished sub_job send task_id to done_chan
        // ------------------ delete this ----------------
        // if job.read_counts[sj.shard_id].fetch_sub(1, Ordering::SeqCst) == 1 {
        //     self.done_chans[sj.shard_id]
        //         .send((job.task_id, job.next_task_id))
        //         .unwrap();
        // }
        // ------------------ add this -------------------
        let shard_id = sj.shard_id;
        if job.read_counts[shard_id].fetch_sub(1, Ordering::SeqCst) == 1 {
            self.handoff_and_check_completion(&job, shard_id);
        }
        self.recycle_job_object(job);
    }
}

pub struct Job {
    read_counts: Arc<Vec<AtomicUsize>>,
    cache: Arc<EntryCache>,
    task_id: i64,
    next_task_id: i64,
    sub_jobs: Vec<SubJob>,
    sub_job_count: usize,
    // -------------- add -----------------
    is_last_task_of_block: bool,
    shards_remaining: AtomicUsize,
    // ------------------------------------
}

struct SubJob {
    sub_id: u64,
    shard_id: usize,
    need_read_ssd: AtomicBool,
    file_pos: i64,
    buf: Box<[u8]>,
}

impl Default for Job {
    fn default() -> Self {
        let mut v = Vec::with_capacity(SHARD_COUNT);
        for _ in 0..SHARD_COUNT {
            v.push(AtomicUsize::new(0));
        }
        Self {
            read_counts: Arc::new(v),
            // -------------- add -----------------
            shards_remaining: AtomicUsize::new(SHARD_COUNT),
            is_last_task_of_block: false,
            // ------------------------------------
            cache: NULL_CACHE.clone(),
            task_id: i64::MIN,
            next_task_id: i64::MIN,
            sub_jobs: Vec::new(),
            sub_job_count: 0,
        }
    }
}

impl SubJob {
    fn new(sub_id: u64, shard_id: usize, file_pos: i64) -> Self {
        let v = direct_io::allocate_aligned_vec(IO_BLK_SIZE * 2, IO_BLK_SIZE);
        let mut buf = v.into_boxed_slice();
        //buf.fill(0);
        SubJob {
            sub_id,
            shard_id,
            file_pos,
            buf,
            need_read_ssd: AtomicBool::new(false),
        }
    }

    fn reinit(&mut self, sub_id: u64, shard_id: usize, file_pos: i64) {
        self.sub_id = sub_id;
        self.shard_id = shard_id;
        self.file_pos = file_pos;
        self.need_read_ssd.store(false, Ordering::SeqCst);
        //self.buf.fill(0);
    }
}

impl Job {
    fn add(&mut self, sub_id: u64, shard_id: usize, file_pos: i64) {
        self.read_counts[shard_id].fetch_add(1, Ordering::SeqCst);
        if self.sub_job_count < self.sub_jobs.len() {
            let i = self.sub_job_count;
            self.sub_jobs[i].reinit(sub_id, shard_id, file_pos);
        } else {
            let sj = SubJob::new(sub_id, shard_id, file_pos);
            self.sub_jobs.push(sj);
        }
        self.sub_job_count += 1;
    }

    fn clear(&mut self) {
        for i in 0..SHARD_COUNT {
            self.read_counts[i].store(0, Ordering::SeqCst);
        }
        // ----------------- add -----------------
        self.shards_remaining.store(SHARD_COUNT, Ordering::SeqCst);
        self.is_last_task_of_block = false;
        // ---------------------------------------
        self.cache = NULL_CACHE.clone();
        self.task_id = i64::MIN;
        self.next_task_id = i64::MIN;
        self.sub_job_count = 0;
        self.sub_jobs.truncate(SUB_JOB_RESERVE_COUNT);
    }
}
