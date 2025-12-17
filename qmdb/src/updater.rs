use crate::{
    compactor::CompactJob,
    def::{
        is_compactible, DEFAULT_ENTRY_SIZE, IN_BLOCK_IDX_BITS, IN_BLOCK_IDX_MASK, OP_CREATE,
        OP_DELETE, OP_READ, OP_WRITE, SHARD_DIV,
    },
    entryfile::{entry::entry_equal, Entry, EntryBufferWriter, EntryBz, EntryCache, EntryFile},
    indexer::Indexer,
    tasks::TaskHub,
    utils::ringchannel::Consumer,
    utils::OpRecord,
};
use std::collections::HashMap;
use std::sync::Arc;
use chrono::Utc;
use std::time::{Duration, Instant};
use crate::metrics;
use std::sync::atomic::Ordering;

pub struct Updater {
    shard_id: usize,
    task_hub: Arc<dyn TaskHub>,
    update_buffer: EntryBufferWriter,
    cache: Arc<EntryCache>,
    entry_file: Arc<EntryFile>,
    indexer: Arc<Indexer>,
    read_entry_buf: Vec<u8>, // its content is only accessed by Updater's functions
    prev_entry_buf: Vec<u8>, // its content is only accessed by Updater's functions
    curr_version: i64,       // will be contained by the new entries
    sn_start: u64,           // increased after compacting old entries
    sn_end: u64,             // increased after appending new entries
    compact_consumer: Consumer<CompactJob>,
    compact_done_pos: i64,
    utilization_div: i64,
    utilization_ratio: i64,
    compact_thres: i64,
    next_task_id_map: HashMap<i64, i64>,
    next_task_id: i64,
    // ---------------- add this ------------------
    active_duration: Duration,
    work_start_timer: Option<Instant>,
    block_start_time: Option<Instant>,
}

impl Updater {
    pub fn new(
        shard_id: usize,
        task_hub: Arc<dyn TaskHub>,
        update_buffer: EntryBufferWriter,
        entry_file: Arc<EntryFile>,
        indexer: Arc<Indexer>,
        curr_version: i64,
        sn_start: u64,
        sn_end: u64,
        compact_consumer: Consumer<CompactJob>,
        compact_done_pos: i64,
        utilization_div: i64,
        utilization_ratio: i64,
        compact_thres: i64,
        next_task_id: i64,
    ) -> Self {
        Self {
            shard_id,
            task_hub,
            update_buffer,
            cache: Arc::new(EntryCache::new_uninit()),
            entry_file,
            indexer,
            read_entry_buf: Vec::with_capacity(DEFAULT_ENTRY_SIZE),
            prev_entry_buf: Vec::with_capacity(DEFAULT_ENTRY_SIZE),
            curr_version,
            sn_start,
            sn_end,
            compact_consumer,
            compact_done_pos,
            utilization_div,
            utilization_ratio,
            compact_thres,
            next_task_id_map: HashMap::new(),
            next_task_id,
            // --------------- add this -----------------
            active_duration: Duration::new(0, 0),
            work_start_timer: None,
            block_start_time: None,
        }
    }


    // --------------- add this -----------------
    fn resume_watch(&mut self) {
        if self.work_start_timer.is_none() {
            self.work_start_timer = Some(Instant::now());
        }
    }

    // ---------------- add this ----------------
    fn pause_watch(&mut self) {
        if let Some(start) = self.work_start_timer {
            self.active_duration += start.elapsed();
            self.work_start_timer = None;
        }
    }

    fn read_entry(&mut self, shard_id: usize, file_pos: i64) {
        let cache_hit = self.cache.lookup(shard_id, file_pos, |entry_bz| {
            self.read_entry_buf.resize(0, 0);
            self.read_entry_buf.extend_from_slice(entry_bz.bz);
        });
        if cache_hit {
            return;
        }
        let (in_disk, accessed) = self.update_buffer.get_entry_bz_at(file_pos, |entry_bz| {
            self.read_entry_buf.resize(0, 0);
            self.read_entry_buf.extend_from_slice(entry_bz.bz);
        });
        //println!("BB in_disk={} accessed={} file_pos={}", in_disk, accessed, file_pos);
        if accessed {
            let entry_bz = EntryBz {
                bz: &self.read_entry_buf[..],
            };
            let _e = Entry::from_bz(&entry_bz);
            return;
        }
        self.read_entry_buf.resize(DEFAULT_ENTRY_SIZE, 0);
        let ef = &self.entry_file;
        if in_disk {
            let size = ef.read_entry(file_pos, &mut self.read_entry_buf[..]);
            self.read_entry_buf.resize(size, 0);
            if self.read_entry_buf.len() < size {
                ef.read_entry(file_pos, &mut self.read_entry_buf[..]);
            }
        } else {
            panic!("Cannot read the entry");
        }
    }

    // handle out-of-order id
    pub fn run_task_with_ooo_id(&mut self, task_id: i64, next_task_id: i64) {
        // insert them so they are viewed as "ready to run"
        self.next_task_id_map.insert(task_id, next_task_id);
        let mut next_task_id = self.next_task_id;
        // try to step forward in the task_id linked list
        loop {
            if let Some(next) = self.next_task_id_map.remove(&next_task_id) {
                self.run_task(next_task_id);
                next_task_id = next; //follow the linked list
            } else {
                break; // not "ready to run"
            }
        }
        self.next_task_id = next_task_id;
    }

    pub fn run_task(&mut self, task_id: i64) {
        // ------------- add this ---------------
        self.resume_watch();

        let (cache_for_new_block, end_block) = self.task_hub.check_begin_end(task_id);
        if let Some(cache) = cache_for_new_block {
            self.cache = cache;
        }
        let task_hub = self.task_hub.clone();
        if (task_id & IN_BLOCK_IDX_MASK) == 0 {
            //task_index ==0 and new block start
            self.curr_version = task_id;
            
            // ------------- add this ----------------
            self.block_start_time = Some(Instant::now());
            // ------------- add this ----------------
            self.active_duration = Duration::new(0, 0);

            // -------------- add this -----------------
            let height = task_id >> IN_BLOCK_IDX_BITS;
            println!(
                "PIPELINE_LOG: UPDATER_START height={} shard={} timestamp={}",
                height,
                self.shard_id,
                Utc::now().to_rfc3339()
            );
            // ---------------------------------

        }
        
        for change_set in &*task_hub.get_change_sets(task_id) {
            change_set.run_in_shard(self.shard_id, |op, key_hash, k, v, r| {
                self.compare_active_info(r);
                match op {
                    OP_WRITE => self.write_kv(key_hash, k, v, r),
                    OP_CREATE => self.create_kv(key_hash, k, v, r),
                    OP_DELETE => self.delete_kv(key_hash, k, r),
                    OP_READ => (), //used for debug
                    _ => {
                        panic!("Updater: unsupported operation");
                    }
                }
            });
            self.curr_version += 1;
        }

        if end_block {
            // --------- add this --------------
            self.pause_watch();
            // --------- add timestamp ---------
            let height = task_id >> IN_BLOCK_IDX_BITS;

            let active_ratio = if let Some(block_start) = self.block_start_time {
                let total_secs = block_start.elapsed().as_secs_f64();
                let active_secs = self.active_duration.as_secs_f64();
                if total_secs > 0.0 {
                    active_secs / total_secs
                } else {
                    0.0
                }
            } else {
                0.0
            };

            println!(
                "PIPELINE_LOG: UPDATER_END height={} shard={} timestamp={} active_ratio={:.4}",
                height,
                self.shard_id,
                Utc::now().to_rfc3339(),
                active_ratio // 输出占比，例如 0.8523 表示 85.23%
            );
            self.block_start_time = None;
            // ---------------------------------  

            self.update_buffer
                .end_block(self.compact_done_pos, self.sn_start, self.sn_end);
        }
        // ---------------- add this ------------------
        self.pause_watch();
    }

    fn write_kv(
        &mut self,
        key_hash: &[u8; 32],
        key: &[u8],
        value: &[u8],
        r: Option<&Box<OpRecord>>,
    ) {
        let height = self.curr_version >> IN_BLOCK_IDX_BITS;
        let mut old_pos = -1;
        let indexer = self.indexer.clone();
        indexer.for_each_value(height, &key_hash[..], |file_pos| -> bool {
            self.read_entry(self.shard_id, file_pos);
            let old_entry = EntryBz {
                bz: &self.read_entry_buf[..],
            };
            if old_entry.key() == key {
                old_pos = file_pos;
            }
            old_pos >= 0 // break if old_pos was assigned
        });
        if old_pos < 0 {
            panic!(
                "Write to non-exist key shard_id={} key={:?} key_hash={:?}",
                self.shard_id, key, key_hash
            );
        }
        let old_entry = EntryBz {
            bz: &self.read_entry_buf[..],
        };
        let new_entry = Entry {
            key,
            value,
            next_key_hash: old_entry.next_key_hash(),
            version: self.curr_version,
            serial_number: self.sn_end,
        };
        let dsn_list: [u64; 1] = [old_entry.serial_number()];

        // -------------------- add this --------------------
        let entry_len = new_entry.get_serialized_len(dsn_list.len());
        metrics::PHYSICAL_WRITE_BYTES_USER.fetch_add(entry_len as u64, Ordering::Relaxed);
        // --------------------------------------------------

        let new_pos = self.update_buffer.append(&new_entry, &dsn_list[..]);
        self.indexer
            .change_kv(&key_hash[..], old_pos, new_pos, dsn_list[0], self.sn_end);
        self.sn_end += 1;
        if self.is_compactible() {
            // println!("compact when write kv");
            self.compact(r, 0);
        }
    }

    fn delete_kv(&mut self, key_hash: &[u8; 32], key: &[u8], r: Option<&Box<OpRecord>>) {
        let height = self.curr_version >> IN_BLOCK_IDX_BITS;
        let mut del_entry_pos = -1;
        let mut del_entry_sn = 0;
        let mut old_next_key_hash = [0u8; 32];
        let mut prev_k80 = [0u8; 10];
        let mut old_pos = -1;
        let indexer = self.indexer.clone();
        indexer.for_each_adjacent_value(height, &key_hash[..], |k_buf, file_pos| -> bool {
            self.read_entry(self.shard_id, file_pos);
            let entry_bz = EntryBz {
                bz: &self.read_entry_buf[..],
            };
            if del_entry_pos < 0 && entry_bz.key() == key {
                compare_old_entry(r, &entry_bz);
                del_entry_pos = file_pos;
                del_entry_sn = entry_bz.serial_number();
                old_next_key_hash.copy_from_slice(entry_bz.next_key_hash());
            } else if old_pos < 0 && entry_bz.next_key_hash() == key_hash {
                self.prev_entry_buf.clear();
                self.prev_entry_buf
                    .extend_from_slice(&self.read_entry_buf[..]);
                compare_prev_entry(r, &entry_bz);
                prev_k80.copy_from_slice(k_buf);
                old_pos = file_pos;
            }
            // exit loop if del_entry_pos and old_pos were assigned
            del_entry_pos >= 0 && old_pos >= 0
        });
        if del_entry_pos < 0 {
            panic!("Delete non-exist key at id={} key={:?}", del_entry_pos, key);
        }
        if old_pos < 0 {
            panic!("Cannot find prevEntry");
        }
        let prev_entry = EntryBz {
            bz: &self.prev_entry_buf[..],
        };
        let prev_changed = Entry {
            key: prev_entry.key(),
            value: prev_entry.value(),
            next_key_hash: &old_next_key_hash[..],
            version: self.curr_version,
            serial_number: self.sn_end,
        };
        let dsn_list: [u64; 2] = [del_entry_sn, prev_entry.serial_number()];
        compare_prev_changed(r, &prev_changed, &dsn_list[..]);

        // -------------------- add this --------------------
        let entry_len = prev_changed.get_serialized_len(dsn_list.len());
        metrics::PHYSICAL_WRITE_BYTES_USER.fetch_add(entry_len as u64, Ordering::Relaxed);
        // --------------------------------------------------

        let new_pos = self.update_buffer.append(&prev_changed, &dsn_list[..]);

        self.indexer
            .erase_kv(&key_hash[..], del_entry_pos, del_entry_sn);
        self.indexer
            .change_kv(&prev_k80[..], old_pos, new_pos, dsn_list[1], self.sn_end);
        self.sn_end += 1;
    }

    fn create_kv(
        &mut self,
        key_hash: &[u8; 32],
        key: &[u8],
        value: &[u8],
        r: Option<&Box<OpRecord>>,
    ) {
        let height = self.curr_version >> IN_BLOCK_IDX_BITS;
        let mut old_pos = -1;
        let mut prev_k80 = [0u8; 10];
        let indexer = self.indexer.clone();
        indexer.for_each_adjacent_value(height, &key_hash[..], |k_buf, file_pos| -> bool {
            self.read_entry(self.shard_id, file_pos);
            let prev_entry = EntryBz {
                bz: &self.read_entry_buf[..],
            };
            if prev_entry.key_hash() < *key_hash && &key_hash[..] < prev_entry.next_key_hash() {
                compare_prev_entry(r, &prev_entry);
                prev_k80.copy_from_slice(k_buf);
                old_pos = file_pos;
            }
            old_pos >= 0
        });
        if old_pos < 0 {
            indexer.for_each_adjacent_value(height, &key_hash[..], |key, file_pos| -> bool {
                println!("FF key = {:?} file_pos = {}", key, file_pos);
                false
            });
            panic!(
                "Cannot find prevKey when creating shard_id={} key={:?}",
                self.shard_id, key
            );
        }
        let prev_entry = EntryBz {
            bz: &self.read_entry_buf[..],
        };
        let new_entry = Entry {
            key,
            value,
            next_key_hash: prev_entry.next_key_hash(),
            version: self.curr_version,
            serial_number: self.sn_end,
        };
        compare_new_entry(r, &new_entry, &[]);
        let create_pos = self.update_buffer.append(&new_entry, &[]);
        //println!("create_pos:{:?}", create_pos);
        let prev_changed = Entry {
            key: prev_entry.key(),
            value: prev_entry.value(),
            next_key_hash: &key_hash[..],
            version: self.curr_version,
            serial_number: self.sn_end + 1,
        };
        //println!(
        //    "prev_changed:{:?}, {:?}",
        //    prev_changed.version, prev_changed.serial_number
        //);
        let dsn_list: [u64; 1] = [prev_entry.serial_number()];
        compare_prev_changed(r, &prev_changed, &dsn_list[..]);

        // -------------------- add this --------------------
        let entry_len = new_entry.get_serialized_len(dsn_list.len()) + prev_changed.get_serialized_len(dsn_list.len());
        metrics::PHYSICAL_WRITE_BYTES_USER.fetch_add(entry_len as u64, Ordering::Relaxed);
        // --------------------------------------------------


        let new_pos = self.update_buffer.append(&prev_changed, &dsn_list[..]);
        //println!("new_pos:{:?}", new_pos);
        self.indexer.add_kv(&key_hash[..], create_pos, self.sn_end);
        self.indexer.change_kv(
            &prev_k80[..],
            old_pos,
            new_pos,
            dsn_list[0],
            self.sn_end + 1,
        );
        self.sn_end += 2;
        if self.is_compactible() {
            //println!("compact when create kv");
            self.compact(r, 0);
            self.compact(r, 1);
        }
    }

    pub fn compact(&mut self, r: Option<&Box<OpRecord>>, comp_idx: usize) {
        let (job, kh) = loop {
            // ------------- add this -------------
            self.pause_watch();

            //println!("before updater what something from consumer channel");
            let job = self.compact_consumer.consume();

            // -------------- add this --------------
            self.resume_watch();

            let e = EntryBz { bz: &job.entry_bz };
            let kh = e.key_hash();

            if self.indexer.key_exists(&kh, job.old_pos, e.serial_number()) {
                break (job, kh);
            }
            self.compact_consumer.send_returned(job);
        };

        metrics::PHYSICAL_WRITE_BYTES_GC.fetch_add(job.entry_bz.len() as u64, Ordering::Relaxed);

        let entry_bz = EntryBz { bz: &job.entry_bz };

        compare_dig_entry(r, &entry_bz, comp_idx);

        let new_entry = Entry {
            key: entry_bz.key(),
            value: entry_bz.value(),
            next_key_hash: entry_bz.next_key_hash(),
            version: entry_bz.version(),
            serial_number: self.sn_end,
        };

        let dsn_list = [entry_bz.serial_number()];
        compare_put_entry(r, &new_entry, &dsn_list, comp_idx);

        let new_pos = self.update_buffer.append(&new_entry, &dsn_list);
        self.indexer
            .change_kv(&kh, job.old_pos, new_pos, dsn_list[0], self.sn_end);

        self.sn_end += 1;
        self.sn_start = entry_bz.serial_number() + 1;
        self.compact_done_pos = job.old_pos + entry_bz.len() as i64;

        let job_clone = job.clone();
        //println!("before updater what send something from consumer channel position B");
        self.compact_consumer.send_returned(job_clone);
    }

    fn is_compactible(&self) -> bool {
        is_compactible(
            self.utilization_div,
            self.utilization_ratio,
            self.compact_thres,
            self.indexer.len(self.shard_id),
            self.sn_start,
            self.sn_end,
        )
    }
    fn compare_active_info(&self, rec: Option<&Box<OpRecord>>) {
        if cfg!(feature = "check_rec") {
            _compare_active_info(self, rec);
        }
    }
}

fn _compare_active_info(updater: &Updater, rec: Option<&Box<OpRecord>>) {
    if let Some(rec) = rec {
        let num_active = updater.indexer.len(updater.shard_id);
        assert_eq!(rec.num_active, num_active, "incorrect num_active");
        assert_eq!(rec.oldest_active_sn, updater.sn_start, "incorrect sn_start");
    }
}

fn _compare_old_entry(rec: Option<&Box<OpRecord>>, entry_bz: &EntryBz) {
    if let Some(rec) = rec {
        let v = rec.rd_list.last().unwrap();
        assert_eq!(&v[..], entry_bz.bz, "compare_old_entry failed");
    }
}

fn _compare_prev_entry(rec: Option<&Box<OpRecord>>, entry_bz: &EntryBz) {
    if let Some(rec) = rec {
        let v = rec.rd_list.first().unwrap();
        //if &v[..] != entry_bz.bz {
        //    let e = EntryBz { bz: &v[..] };
        //    println!("ref k={} v={}", hex::encode(e.key()), hex::encode(e.value()));
        //    println!("ref sn={:#016x} ver={:#016x}", e.serial_number(), e.version());
        //    println!("imp k={} v={}", hex::encode(entry_bz.key()), hex::encode(entry_bz.value()));
        //    println!("imp sn={:#016x} ver={:#016x}", entry_bz.serial_number(), entry_bz.version());
        //}
        assert_eq!(&v[..], entry_bz.bz, "compare_prev_entry failed");
    }
}

fn _compare_prev_changed(rec: Option<&Box<OpRecord>>, entry: &Entry, dsn_list: &[u64]) {
    if let Some(rec) = rec {
        let v = rec.wr_list.first().unwrap();
        let equal = entry_equal(&v[..], entry, dsn_list);
        if !equal {
            let tmp = EntryBz { bz: &v[..] };
            let r = Entry::from_bz(&tmp);
            let key_hash = tmp.key_hash();
            let shard_id = key_hash[0] as usize * 256 / SHARD_DIV;
            println!(
                "AA cmpr prev_C shard_id={}\nref={:?}\nimp={:?}\ndsn_list={:?}",
                shard_id, r, entry, dsn_list
            );
            for (_, sn) in tmp.dsn_iter() {
                println!("--{}", sn);
            }
        }
        assert!(equal, "compare_prev_changed failed");
    }
}

fn _compare_new_entry(rec: Option<&Box<OpRecord>>, entry: &Entry, dsn_list: &[u64]) {
    if let Some(rec) = rec {
        let v = rec.wr_list.last().unwrap();
        let equal = entry_equal(&v[..], entry, dsn_list);
        assert!(equal, "compare_new_entry failed");
    }
}

fn _compare_dig_entry(rec: Option<&Box<OpRecord>>, entry_bz: &EntryBz, comp_idx: usize) {
    if let Some(rec) = rec {
        let v = rec.dig_list.get(comp_idx).unwrap();
        if &v[..] != entry_bz.bz {
            let tmp = EntryBz { bz: &v[..] };
            let r = Entry::from_bz(&tmp);
            let i = Entry::from_bz(entry_bz);
            let key_hash = entry_bz.key_hash();
            let shard_id = key_hash[0] >> 4;
            println!(
                "AA cmpr dig_E shard_id={}\nref={:?}\nimp={:?}\nref={:?}\nimp={:?}",
                shard_id,
                r,
                i,
                &v[..],
                entry_bz.bz
            );
        }
        assert_eq!(&v[..], entry_bz.bz, "compare_dig_entry failed");
    }
}

fn _compare_put_entry(
    rec: Option<&Box<OpRecord>>,
    entry: &Entry,
    dsn_list: &[u64],
    comp_idx: usize,
) {
    if let Some(rec) = rec {
        let v = rec.put_list.get(comp_idx).unwrap();
        assert!(
            entry_equal(&v[..], entry, dsn_list),
            "compare_put_entry failed"
        );
    }
}

fn compare_old_entry(rec: Option<&Box<OpRecord>>, entry_bz: &EntryBz) {
    if cfg!(feature = "check_rec") {
        _compare_old_entry(rec, entry_bz)
    }
}

fn compare_prev_entry(rec: Option<&Box<OpRecord>>, entry_bz: &EntryBz) {
    if cfg!(feature = "check_rec") {
        _compare_prev_entry(rec, entry_bz)
    }
}

fn compare_prev_changed(rec: Option<&Box<OpRecord>>, entry: &Entry, dsn_list: &[u64]) {
    if cfg!(feature = "check_rec") {
        _compare_prev_changed(rec, entry, dsn_list);
    }
}

fn compare_new_entry(rec: Option<&Box<OpRecord>>, entry: &Entry, dsn_list: &[u64]) {
    if cfg!(feature = "check_rec") {
        _compare_new_entry(rec, entry, dsn_list);
    }
}

fn compare_dig_entry(rec: Option<&Box<OpRecord>>, entry_bz: &EntryBz, comp_idx: usize) {
    if cfg!(feature = "check_rec") {
        _compare_dig_entry(rec, entry_bz, comp_idx);
    }
}

fn compare_put_entry(
    rec: Option<&Box<OpRecord>>,
    entry: &Entry,
    dsn_list: &[u64],
    comp_idx: usize,
) {
    if cfg!(feature = "check_rec") {
        _compare_put_entry(rec, entry, dsn_list, comp_idx);
    }
}

#[cfg(test)]
mod updater_tests {
    use std::vec;

    use crate::{
        entryfile::{entry::entry_to_bytes, entrybuffer, EntryFileWriter},
        tasks::BlockPairTaskHub,
        test_helper::{to_k80, SimpleTask, TempDir},
        utils::ringchannel::{self, Producer},
    };

    use super::*;

    fn new_updater(dir: &str) -> (TempDir, Updater, Producer<CompactJob>) {
        let temp_dir = TempDir::new(dir);
        let (entry_buffer_w, _entry_buffer_r) = entrybuffer::new(8, 1024);
        let cache_arc = Arc::new(EntryCache::new());
        let entry_file_arc = Arc::new(EntryFile::new(
            512,
            2048,
            dir.to_string(),
            cfg!(feature = "directio"),
            None,
        ));
        let btree_arc = Arc::new(Indexer::new(16));
        let job = CompactJob {
            old_pos: 0,
            entry_bz: Vec::new(),
        };
        let (producer, consumer) = ringchannel::new(100, &job);
        let updater = Updater {
            shard_id: 0,
            task_hub: Arc::new(BlockPairTaskHub::<SimpleTask>::new()),
            update_buffer: entry_buffer_w,
            cache: cache_arc,
            entry_file: entry_file_arc,
            indexer: btree_arc,
            read_entry_buf: vec![0u8; 1024],
            prev_entry_buf: vec![0u8; 1024],
            curr_version: 0,
            sn_start: 0,
            sn_end: 0,
            compact_done_pos: 0,
            utilization_div: 10,
            utilization_ratio: 7,
            compact_thres: 8,
            next_task_id_map: HashMap::new(),
            next_task_id: 0,
            compact_consumer: consumer,
        };
        (temp_dir, updater, producer)
    }

    fn new_test_entry<'a>() -> Entry<'a> {
        Entry {
            key: "key".as_bytes(),
            value: "value".as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        }
    }

    fn append_and_flush_entry_to_file(
        entry_file: Arc<EntryFile>,
        entry: &Entry,
        dsn_list: &[u64],
    ) -> i64 {
        let mut w = EntryFileWriter::new(entry_file.clone(), 512);
        let mut entry_bz = [0u8; 512];
        let _entry_size = entry.dump(&mut entry_bz, dsn_list);
        let pos = w.append(&EntryBz { bz: &entry_bz[..] }).unwrap();
        let _ = w.flush();
        pos
    }

    fn put_entry_in_cache(updater: &Updater, file_pos: i64, entry: &Entry, dsn_list: &[u64]) {
        let mut entry_buf = [0u8; 1024];
        let entry_size = entry.dump(&mut entry_buf[..], dsn_list);
        let entry_bz = EntryBz {
            bz: &entry_buf[..entry_size],
        };
        updater.cache.insert(updater.shard_id, file_pos, &entry_bz);
    }

    #[test]
    fn test_read_entry_cache_hit() {
        let (_dir, mut updater, _producer) = new_updater("test_read_entry_cache_hit");

        let entry = new_test_entry();
        let dsn_list = [1, 2, 3, 4];
        put_entry_in_cache(&updater, 123, &entry, &dsn_list);

        updater.read_entry(updater.shard_id, 123);
        assert_eq!(
            "03050000046b657976616c7565ababababababab",
            hex::encode(&updater.read_entry_buf[0..20])
        );
    }

    #[test]
    fn test_read_entry_from_buffer() {
        let (_dir, mut updater, _producer) = new_updater("test_read_entry_from_buffer");
        let entry = new_test_entry();
        let dsn_list = [1, 2, 3, 4];
        let pos = updater.update_buffer.append(&entry, &dsn_list);

        updater.read_entry(7, pos);
        assert_eq!(
            "03050000046b657976616c7565ababababababab",
            hex::encode(&updater.read_entry_buf[0..20])
        );
    }

    #[test]
    fn test_read_entry_from_file() {
        let (_dir, mut updater, _producer) = new_updater("test_read_entry_from_file");
        let entry = new_test_entry();
        let dsn_list = [1, 2, 3, 4];
        let pos = append_and_flush_entry_to_file(updater.entry_file.clone(), &entry, &dsn_list);

        updater.read_entry(7, pos);
        assert_eq!(
            "03050000046b657976616c7565ababababababab",
            hex::encode(&updater.read_entry_buf[0..20])
        );
    }

    #[test]
    #[should_panic(expected = "incorrect num_active")]
    fn test_compare_active_info1() {
        let (_dir, updater, _producer) = new_updater("test_compare_active_info1");
        let mut op = Box::new(OpRecord::new(0));
        op.num_active = 123;
        let rec = Option::Some(&op);
        _compare_active_info(&updater, rec);
    }

    #[test]
    #[should_panic(expected = "incorrect sn_start")]
    fn test_compare_active_info2() {
        let (_dir, updater, _producer) = new_updater("test_compare_active_info2");
        let mut op = Box::new(OpRecord::new(0));
        op.oldest_active_sn = 123;
        let rec = Option::Some(&op);
        _compare_active_info(&updater, rec);
    }

    #[test]
    #[should_panic(expected = "Cannot find prevKey when creating shard_id=0 key=[107, 101, 121]")]
    fn test_create_kv_non_exist_key() {
        let (_dir, mut updater, _producer) = new_updater("test_create_kv_non_exist_key");
        updater.create_kv(
            &[5u8; 32],
            "key".as_bytes(),
            "value".as_bytes(),
            Option::None,
        );
    }

    #[test]
    fn test_create_kv() {
        let (_dir, mut updater, _producer) = new_updater("test_create_kv");

        let entry = new_test_entry();
        let dsn_list = [];
        let pos = append_and_flush_entry_to_file(updater.entry_file.clone(), &entry, &dsn_list);

        updater
            .indexer
            .add_kv(&to_k80(0x7777_0000_0000_0000), pos, 0);
        assert_eq!(1, updater.indexer.len(0));
        assert_eq!(0, updater.sn_end);

        updater.create_kv(
            &[0x77u8; 32],
            "key".as_bytes(),
            "value".as_bytes(),
            Option::None,
        );

        assert_eq!(2, updater.indexer.len(0));
        assert_eq!(2, updater.sn_end);
        // TODO: check more
    }

    #[test]
    #[should_panic(expected = "Cannot find prevKey when creating shard_id=0 key=[107, 101, 121]")]
    fn test_write_kv_non_exist_key() {
        let (_dir, mut updater, _producer) = new_updater("test_write_kv_non_exist_key");
        updater.create_kv(
            &[5u8; 32],
            "key".as_bytes(),
            "value".as_bytes(),
            Option::None,
        );
    }

    #[test]
    fn test_write_kv() {
        let (_dir, mut updater, _producer) = new_updater("test_write_kv");
        let entry = new_test_entry();
        let dsn_list = [];
        let pos = append_and_flush_entry_to_file(updater.entry_file.clone(), &entry, &dsn_list);

        updater
            .indexer
            .add_kv(&to_k80(0x7777_0000_0000_0000), pos, 0);
        updater.create_kv(
            &[0x77u8; 32],
            "key".as_bytes(),
            "value".as_bytes(),
            Option::None,
        );
        assert_eq!(2, updater.indexer.len(0));
        assert_eq!(2, updater.sn_end);

        updater.write_kv(
            &[0x77u8; 32],
            "key".as_bytes(),
            "val2".as_bytes(),
            Option::None,
        );
        assert_eq!(2, updater.indexer.len(0));
        assert_eq!(3, updater.sn_end);
        // TODO: check more
    }

    #[test]
    #[should_panic(expected = "Delete non-exist key")]
    fn test_delete_kv_non_exist_key() {
        let (_dir, mut updater, _producer) = new_updater("test_delete_kv_non_exist_key");
        updater.delete_kv(&[3u8; 32], "key".as_bytes(), Option::None);
    }

    #[test]
    #[should_panic(expected = "Cannot find prevEntry")]
    fn test_delete_kv_no_prev_entry() {
        let (_dir, mut updater, _producer) = new_updater("test_delete_kv_no_prev_entry");

        let entry = new_test_entry();
        let dsn_list = [];
        let pos = append_and_flush_entry_to_file(updater.entry_file.clone(), &entry, &dsn_list);
        updater
            .indexer
            .add_kv(&to_k80(0x7777_7777_7777_7777), pos, 0);

        updater.delete_kv(&[0x77u8; 32], "key".as_bytes(), Option::None);
    }

    #[test]
    fn test_delete_kv() {
        let (_dir, mut updater, _producer) = new_updater("test_delete_kv");
        let entry = new_test_entry();
        let dsn_list = [];
        let pos1 = append_and_flush_entry_to_file(updater.entry_file.clone(), &entry, &dsn_list);
        updater
            .indexer
            .add_kv(&to_k80(0x7777_2000_0000_0000), pos1, 0);
        updater.create_kv(
            &[0x77u8; 32],
            "key".as_bytes(),
            "value".as_bytes(),
            Option::None,
        );
        assert_eq!(2, updater.indexer.len(0));
        assert_eq!(2, updater.sn_end);

        let entry2 = Entry {
            key: "key2".as_bytes(),
            value: "val2".as_bytes(),
            next_key_hash: &[0x77u8; 32],
            version: 12345,
            serial_number: 100000,
        };
        let pos2: i64 =
            append_and_flush_entry_to_file(updater.entry_file.clone(), &entry2, &dsn_list);
        put_entry_in_cache(&updater, pos2, &entry2, &dsn_list);
        updater
            .indexer
            .add_kv(&to_k80(0x7777_3000_0000_0000), pos2, 0);
        assert_eq!(3, updater.indexer.len(0));
        assert_eq!(2, updater.sn_end);

        updater.delete_kv(&[0x77u8; 32], "key".as_bytes(), Option::None);
        assert_eq!(2, updater.indexer.len(0));
        assert_eq!(3, updater.sn_end);
        // TODO: check more
    }

    #[test]
    fn test_is_compactible() {
        // utilization: 60%
        let (_dir, mut updater, _producer) = new_updater("test_is_compactible");
        updater.sn_start = 0;
        updater.sn_end = 20;
        updater.compact_thres = 10;

        for i in 0..20 {
            updater.indexer.add_kv(&to_k80(i), (i * 8) as i64, 0);
            assert_eq!(8 < i && i < 14, updater.is_compactible());
        }

        updater.sn_end = 40;
        assert!(updater.is_compactible());

        updater.compact_thres = 41;
        assert!(!updater.is_compactible());
    }

    #[test]
    fn test_try_compact() {
        let (_dir, mut updater, mut producer) = new_updater("test_try_compact");
        let entry = new_test_entry();
        let dsn_list = [0u64; 0];
        let mut entry_buf = [0u8; 500];
        let entry_bz = entry_to_bytes(&entry, &dsn_list, &mut entry_buf);
        let pos = append_and_flush_entry_to_file(updater.entry_file.clone(), &entry, &dsn_list);
        let kh = entry_bz.key_hash();
        updater.indexer.add_kv(&kh[..], pos, 0);
        updater.sn_end = 10;
        updater.compact_thres = 0;
        updater.utilization_ratio = 1;
        assert!(updater.is_compactible());
        assert_eq!(1, updater.indexer.len(0));
        assert_eq!(10, updater.sn_end);

        producer
            .produce(CompactJob {
                old_pos: 0,
                entry_bz: entry_buf.to_vec(),
            })
            .unwrap();
        producer.receive_returned().unwrap();

        updater.compact(Option::None, 0);
        assert_eq!(1, updater.indexer.len(0));
        assert_eq!(11, updater.sn_end);
        // TODO: check mores
    }

    #[test]
    fn test_run_task() {
        // todo
    }
}

#[cfg(test)]
mod compare_tests {
    use super::*;
    use crate::{
        entryfile::{Entry, EntryBz},
        test_helper::EntryBuilder,
        utils::OpRecord,
    };

    fn new_test_entry<'a>() -> Entry<'a> {
        Entry {
            key: "key".as_bytes(),
            value: "value".as_bytes(),
            next_key_hash: &[0xab; 32],
            version: 12345,
            serial_number: 99999,
        }
    }

    #[test]
    #[should_panic(expected = "compare_old_entry failed")]
    fn test_compare_old_entry() {
        let mut op = Box::new(OpRecord::new(0));
        op.rd_list.push(vec![4, 5, 6]);
        op.rd_list.push(vec![1, 2, 3]);
        let rec = Option::Some(&op);
        let bz: [u8; 3] = [4, 5, 6];
        _compare_old_entry(rec, &EntryBz { bz: &bz[..] });
    }

    #[test]
    #[should_panic(expected = "compare_prev_entry failed")]
    fn test_compare_prev_entry() {
        let mut op = Box::new(OpRecord::new(0));
        op.rd_list.push(vec![1, 2, 3]);
        op.rd_list.push(vec![4, 5, 6]);
        let rec = Option::Some(&op);
        let bz: [u8; 3] = [4, 5, 6];
        _compare_prev_entry(rec, &EntryBz { bz: &bz[..] });
    }

    #[test]
    #[should_panic(expected = "compare_prev_changed failed")]
    fn test_compare_prev_changed() {
        let entry = new_test_entry();
        let dsn_list: [u64; 4] = [1, 2, 3, 4];

        let mut op = Box::new(OpRecord::new(0));
        op.wr_list
            .push(EntryBuilder::kv("abc", "def").build_and_dump(&[]));
        op.wr_list.push(vec![4, 5, 6]);
        let rec = Option::Some(&op);
        _compare_prev_changed(rec, &entry, &dsn_list);
    }

    #[test]
    #[should_panic(expected = "compare_new_entry failed")]
    fn test_compare_new_entry() {
        let entry = new_test_entry();
        let dsn_list: [u64; 4] = [1, 2, 3, 4];

        let mut op = Box::new(OpRecord::new(0));
        op.wr_list.push(vec![1, 2, 3]);
        op.wr_list.push(vec![4, 5, 6]);
        let rec = Option::Some(&op);
        _compare_new_entry(rec, &entry, &dsn_list);
    }

    #[test]
    #[should_panic(expected = "compare_dig_entry failed")]
    fn test_compare_dig_entry() {
        let entry1 = EntryBuilder::kv("abc", "def").build_and_dump(&[]);
        let entry2 = EntryBuilder::kv("hhh", "www").build_and_dump(&[]);
        let entry3 = EntryBuilder::kv("123", "456").build_and_dump(&[]);

        let mut op = Box::new(OpRecord::new(0));
        op.dig_list.push(entry1);
        op.rd_list.push(entry2);
        let rec = Option::Some(&op);
        _compare_dig_entry(rec, &EntryBz { bz: &entry3 }, 0);
    }

    #[test]
    #[should_panic(expected = "compare_put_entry failed")]
    fn test_compare_put_entry() {
        let entry = new_test_entry();
        let dsn_list: [u64; 4] = [1, 2, 3, 4];

        let mut op = Box::new(OpRecord::new(0));
        op.put_list.push(vec![1, 2, 3]);
        op.put_list.push(vec![4, 5, 6]);
        let rec = Option::Some(&op);
        _compare_put_entry(rec, &entry, &dsn_list, 1);
    }
}
