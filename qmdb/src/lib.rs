extern crate core;
pub mod compactor;
pub mod config;
pub mod def;
#[cfg(all(target_os = "linux", feature = "directio"))]
pub mod dioprefetcher;
pub mod entryfile;
pub mod flusher;
pub mod indexer;
pub mod merkletree;
pub mod metadb;
pub mod seqads;
pub mod stateless;
pub mod tasks;
#[cfg(not(all(target_os = "linux", feature = "directio")))]
pub mod uniprefetcher;
pub mod updater;
pub mod utils;

// for test
pub mod test_helper;

use aes_gcm::{Aes256Gcm, Key, KeyInit};
use byteorder::{BigEndian, ByteOrder};
use entryfile::entrybuffer;
use merkletree::proof::ProofPath;
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use threadpool::ThreadPool;

use crate::compactor::{CompactJob, Compactor};
use crate::def::{
    COMPACT_RING_SIZE, DEFAULT_ENTRY_SIZE, IN_BLOCK_IDX_BITS, SENTRY_COUNT, SHARD_COUNT, SHARD_DIV,
    TWIG_SHIFT,
};
use crate::entryfile::{entry::sentry_entry, EntryBz, EntryCache, EntryFile};
use crate::flusher::{Flusher, FlusherShard, ProofReqElem};
use crate::indexer::Indexer;
use crate::merkletree::{
    recover::{bytes_to_edge_nodes, recover_tree},
    Tree,
};
use crate::metadb::{MetaDB, MetaInfo};
use log::{debug, error, info};

#[cfg(all(target_os = "linux", feature = "directio"))]
use crate::dioprefetcher::{JobManager, Prefetcher};
#[cfg(not(all(target_os = "linux", feature = "directio")))]
use crate::uniprefetcher::{JobManager, Prefetcher};

use crate::tasks::{BlockPairTaskHub, Task, TaskHub, TasksManager};
use crate::updater::Updater;
use crate::utils::hasher;
use crate::utils::ringchannel;
// pub use utils::changeset::ChangeSet;

pub struct AdsCore {
    task_hub: Arc<dyn TaskHub>,
    task_senders: Vec<SyncSender<i64>>,
    indexer: Arc<Indexer>,
    entry_files: Vec<Arc<EntryFile>>,
    meta: Arc<RwLock<MetaDB>>,
    wrbuf_size: usize,
    proof_req_senders: Vec<SyncSender<ProofReqElem>>,
}

fn get_ciphers(
    aes_keys: &Option<[u8; 96]>,
) -> (
    VecDeque<Option<Aes256Gcm>>,
    Arc<Option<Aes256Gcm>>,
    Option<Aes256Gcm>,
) {
    let mut vec = VecDeque::with_capacity(SHARD_COUNT);
    for _ in 0..SHARD_COUNT {
        vec.push_back(None);
    }
    if cfg!(not(feature = "tee_cipher")) {
        return (vec, Arc::new(None), None);
    }

    let _aes_keys = aes_keys.as_ref().unwrap();
    if _aes_keys.len() != 96 {
        panic!("Invalid length for aes_keys");
    }
    let mut key = [0u8; 33];
    key[1..].copy_from_slice(&_aes_keys[..32]);
    for i in 0..SHARD_COUNT {
        key[0] = i as u8;
        let hash = hasher::hash(key);
        let aes_key: &Key<Aes256Gcm> = (&hash).into();
        vec[i] = Some(Aes256Gcm::new(aes_key));
    }
    let aes_key = Key::<Aes256Gcm>::from_slice(&_aes_keys[32..64]);
    let indexer_cipher = Some(Aes256Gcm::new(aes_key));
    let aes_key = Key::<Aes256Gcm>::from_slice(&_aes_keys[64..96]);
    let meta_db_cipher = Some(Aes256Gcm::new(aes_key));
    (vec, Arc::new(indexer_cipher), meta_db_cipher)
}

impl AdsCore {
    pub fn get_sub_dirs(dir: &str) -> (String, String, String) {
        let data_dir = dir.to_owned() + "/data";
        let meta_dir = dir.to_owned() + "/metadb";
        let indexer_dir = dir.to_owned() + "/indexer";
        (data_dir, meta_dir, indexer_dir)
    }

    pub fn new(
        task_hub: Arc<dyn TaskHub>,
        config: &config::Config,
    ) -> (Self, Receiver<Arc<MetaInfo>>, Flusher) {
        #[cfg(feature = "tee_cipher")]
        assert!(config.aes_keys.unwrap().len() == 96);

        Self::_new(
            task_hub,
            &config.dir,
            config.wrbuf_size,
            config.file_segment_size,
            config.with_twig_file,
            &config.aes_keys,
        )
    }

    fn _new(
        task_hub: Arc<dyn TaskHub>,
        dir: &str,
        wrbuf_size: usize,
        file_segment_size: usize,
        with_twig_file: bool,
        aes_keys: &Option<[u8; 96]>,
    ) -> (Self, Receiver<Arc<MetaInfo>>, Flusher) {
        let (ciphers, idx_cipher, meta_db_cipher) = get_ciphers(aes_keys);
        let (data_dir, meta_dir, _indexer_dir) = Self::get_sub_dirs(dir);

        let dir = (dir.to_owned() + "/idx").to_owned();
        let indexer = Arc::new(Indexer::with_dir_and_cipher(dir, idx_cipher));
        let (eb_sender, eb_receiver) = sync_channel(2);
        let meta = MetaDB::with_dir(&meta_dir, meta_db_cipher);
        let curr_height = meta.get_curr_height();
        let meta = Arc::new(RwLock::new(meta));

        // recover trees in parallel
        let mut trees = Self::recover_trees(
            meta.clone(),
            data_dir,
            indexer.clone(),
            wrbuf_size,
            file_segment_size,
            with_twig_file,
            curr_height,
            ciphers,
        );

        let mut entry_files = Vec::with_capacity(SHARD_COUNT);
        let mut shards: Vec<Box<FlusherShard>> = Vec::with_capacity(SHARD_COUNT);

        for shard_id in 0..SHARD_COUNT {
            let (tree, oldest_active_sn) = trees.remove(0);
            entry_files.push(tree.entry_file_wr.entry_file.clone());
            shards.push(Box::new(FlusherShard::new(
                tree,
                oldest_active_sn,
                shard_id,
            )));
        }

        let max_kept_height = 10; //1000;
        let flusher = Flusher::new(
            shards,
            meta.clone(),
            curr_height,
            max_kept_height,
            eb_sender,
        );

        let ads_core = Self {
            task_hub,
            task_senders: Vec::with_capacity(1),
            indexer,
            entry_files,
            meta: meta.clone(),
            wrbuf_size,
            proof_req_senders: flusher.get_proof_req_senders(),
        };
        (ads_core, eb_receiver, flusher)
    }

    pub fn get_proof(&self, shard_id: usize, sn: u64) -> Result<ProofPath, String> {
        if cfg!(feature = "slow_hashing") {
            return Err("do not support proof in slow hashing mode".to_owned());
        }

        let pair = Arc::new((Mutex::new((sn, Option::None)), Condvar::new()));

        if let Err(er) = self.proof_req_senders[shard_id].send(Arc::clone(&pair)) {
            return Err(format!("send proof request failed: {:?}", er));
        }

        // wait for the request to be handled
        let (lock, cvar) = &*pair;
        let mut sn_proof = lock.lock().unwrap();
        while sn_proof.1.is_none() {
            sn_proof = cvar.wait(sn_proof).unwrap();
        }

        if let Err(er) = sn_proof.1.as_ref().unwrap() {
            return Err(format!("get proof failed: {:?}", er));
        }
        sn_proof.1.take().unwrap()
    }

    pub fn get_entry_files(&self) -> Vec<Arc<EntryFile>> {
        let mut res = Vec::with_capacity(self.entry_files.len());
        for ef in self.entry_files.iter() {
            res.push(ef.clone());
        }
        res
    }

    #[cfg(not(feature = "use_hybridindexer"))]
    fn recover_trees(
        meta: Arc<RwLock<MetaDB>>,
        data_dir: String,
        indexer: Arc<Indexer>,
        wrbuf_size: usize,
        file_segment_size: usize,
        with_twig_file: bool,
        curr_height: i64,
        mut ciphers: VecDeque<Option<Aes256Gcm>>,
    ) -> Vec<(Tree, u64)> {
        use log::debug;

        let mut recover_handles = Vec::with_capacity(SHARD_COUNT);
        for shard_id in 0..SHARD_COUNT {
            let meta = meta.clone();
            let data_dir = data_dir.clone();
            let indexer = indexer.clone();
            let cipher = ciphers.pop_front().unwrap();
            let handle = thread::spawn(move || {
                let meta = meta.read_arc();
                let (tree, ef_prune_to, oldest_active_sn) = Self::recover_tree(
                    &meta,
                    data_dir,
                    wrbuf_size,
                    file_segment_size,
                    with_twig_file,
                    curr_height,
                    shard_id,
                    cipher,
                );

                Self::index_tree(&tree, oldest_active_sn, ef_prune_to, &indexer);

                (tree, oldest_active_sn)
            });
            recover_handles.push(handle);
        }

        let mut result = Vec::with_capacity(SENTRY_COUNT);
        for _shard_id in 0..SHARD_COUNT {
            let handle = recover_handles.remove(0);
            let (tree, oldest_active_sn) = handle.join().unwrap();
            result.push((tree, oldest_active_sn));
        }
        debug!("finish recover_tree");
        result
    }

    #[cfg(feature = "use_hybridindexer")]
    fn recover_trees(
        meta: Arc<RwLock<MetaDB>>,
        data_dir: String,
        indexer: Arc<Indexer>,
        wrbuf_size: usize,
        file_segment_size: usize,
        with_twig_file: bool,
        curr_height: i64,
        mut ciphers: VecDeque<Option<Aes256Gcm>>,
    ) -> Vec<(Tree, u64)> {
        let mut recover_handles = Vec::with_capacity(SHARD_COUNT);
        for shard_id in 0..SHARD_COUNT {
            let meta = meta.clone();
            let data_dir = data_dir.clone();
            let cipher = ciphers.pop_front().unwrap();
            let handle = thread::spawn(move || {
                let meta = meta.read_arc();
                let (tree, ef_prune_to, oldest_active_sn) = Self::recover_tree(
                    &meta,
                    data_dir,
                    wrbuf_size,
                    file_segment_size,
                    with_twig_file,
                    curr_height,
                    shard_id,
                    cipher,
                );

                (tree, ef_prune_to, oldest_active_sn)
            });
            recover_handles.push(handle);
        }

        let mut result = Vec::with_capacity(SENTRY_COUNT);
        for shard_id in 0..SHARD_COUNT {
            let handle = recover_handles.remove(0);
            let (tree, ef_prune_to, oldest_active_sn) = handle.join().unwrap();

            Self::index_tree(&tree, oldest_active_sn, ef_prune_to, &indexer);
            indexer.dump_mem_to_file(shard_id);

            result.push((tree, oldest_active_sn));
        }
        result
    }

    pub fn recover_tree(
        meta: &MetaDB,
        data_dir: String,
        buffer_size: usize,
        file_segment_size: usize,
        with_twig_file: bool,
        curr_height: i64,
        shard_id: usize,
        cipher: Option<Aes256Gcm>,
    ) -> (Tree, i64, u64) {
        // let meta = meta.read_arc();
        let oldest_active_sn = meta.get_oldest_active_sn(shard_id);
        let youngest_twig_id = meta.get_youngest_twig_id(shard_id);
        let edge_nodes = bytes_to_edge_nodes(&meta.get_edge_nodes(shard_id));
        let (last_pruned_twig_id, ef_prune_to) = meta.get_last_pruned_twig(shard_id);
        let root = meta.get_root_hash(shard_id);
        let entryfile_size = meta.get_entry_file_size(shard_id);
        let twigfile_size = meta.get_twig_file_size(shard_id);
        let (tree, recovered_root) = recover_tree(
            shard_id,
            buffer_size,
            file_segment_size,
            with_twig_file,
            data_dir,
            format!("{}", shard_id),
            &edge_nodes,
            last_pruned_twig_id,
            ef_prune_to,
            oldest_active_sn >> TWIG_SHIFT,
            youngest_twig_id,
            &[entryfile_size, twigfile_size],
            cipher,
        );
        if shard_id == 0 {
            info!("edge_nodes len:{}, last_pruned_twig_id:{}, oldest_active_twig_id:{}, youngest_twig_id:{}, entryfile_size:{}, twigfile_size:{}, recovered_root:{:?}", edge_nodes.len(), last_pruned_twig_id, oldest_active_sn >> TWIG_SHIFT, youngest_twig_id, entryfile_size, twigfile_size, recovered_root);
        }

        if root != recovered_root && curr_height != 0 {
            error!(
                "root mismatch, shard_id: {}, root: {:?}, recovered_root: {:?}",
                shard_id, root, recovered_root
            );
            panic!("recover error");
        }

        (tree, ef_prune_to, oldest_active_sn)
    }

    pub fn index_tree(tree: &Tree, oldest_active_sn: u64, ef_prune_to: i64, indexer: &Indexer) {
        tree.scan_entries_lite(
            oldest_active_sn >> TWIG_SHIFT,
            ef_prune_to,
            |k80, _nkh, pos, sn| {
                if tree.get_active_bit(sn) {
                    indexer.add_kv(k80, pos, sn);
                }
            },
        );
    }

    pub fn init_dir(config: &config::Config) {
        #[cfg(feature = "tee_cipher")]
        assert!(config.aes_keys.unwrap().len() == 96);

        Self::_init_dir(
            &config.dir,
            config.file_segment_size,
            config.with_twig_file,
            &config.aes_keys,
        );
    }

    fn _init_dir(
        dir: &str,
        file_segment_size: usize,
        with_twig_file: bool,
        aes_keys: &Option<[u8; 96]>,
    ) {
        let (data_dir, meta_dir, _indexer_dir) = Self::get_sub_dirs(dir);

        if Path::new(dir).exists() {
            fs::remove_dir_all(dir).unwrap();
        }
        fs::create_dir(dir).unwrap();
        let (mut ciphers, _, meta_db_cipher) = get_ciphers(aes_keys);
        let mut meta = MetaDB::with_dir(&meta_dir, meta_db_cipher);
        for shard_id in 0..SHARD_COUNT {
            let mut tree = Tree::new(
                shard_id,
                8192,
                file_segment_size as i64,
                data_dir.clone(),
                format!("{}", shard_id),
                with_twig_file,
                ciphers.pop_front().unwrap(),
            );
            let mut bz = [0u8; DEFAULT_ENTRY_SIZE];
            for sn in 0..SENTRY_COUNT {
                let e = sentry_entry(shard_id, sn as u64, &mut bz[..]);
                tree.append_entry(&e).unwrap();
            }
            tree.flush_files(0, 0);
            let (entry_file_size, twig_file_size) = tree.get_file_sizes();
            meta.set_entry_file_size(shard_id, entry_file_size);
            meta.set_twig_file_size(shard_id, twig_file_size);
            meta.set_next_serial_num(shard_id, SENTRY_COUNT as u64);
        }
        meta.insert_extra_data(0, "".to_owned());
        meta.commit();
    }

    pub fn check_entry(key_hash: &[u8], key: &[u8], entry_bz: &EntryBz) -> bool {
        if key.is_empty() {
            entry_bz.key_hash() == key_hash
        } else {
            entry_bz.key() == key
        }
    }

    pub fn warmup_cache<F>(&self, height: i64, k80: &[u8], cache: &EntryCache, mut access: F)
    where
        F: FnMut(EntryBz),
    {
        let k64 = BigEndian::read_u64(&k80[..8]);
        let shard_id = (k64 >> 60) as usize;
        let mut buf = [0u8; DEFAULT_ENTRY_SIZE];
        let idx = &self.indexer;
        idx.for_each_value_warmup(height, k80, |file_pos| -> bool {
            let mut found_it = false;
            cache.lookup(shard_id, file_pos, |_| {
                found_it = true;
            });
            if found_it {
                return false; //continue to next 'file_pos
            }
            let mut bz = &mut buf[..];
            let size = self.entry_files[shard_id].read_entry(file_pos, bz);
            let mut v;
            if buf.len() < size {
                v = vec![0u8; size];
                bz = &mut v[..];
                self.entry_files[shard_id].read_entry(file_pos, bz);
            } else {
                bz = &mut buf[..size];
            }
            let entry_bz = EntryBz { bz };
            let key_hash = entry_bz.key_hash();
            if key_hash[..10] == k80[..10] {
                cache.insert(shard_id, file_pos, &entry_bz);
                access(entry_bz);
            }
            false
        });
    }

    pub fn read_entry(
        &self,
        height: i64,
        key_hash: &[u8],
        key: &[u8],
        cache: Option<&EntryCache>,
        buf: &mut [u8],
    ) -> (usize, bool) {
        let shard_id = key_hash[0] as usize * 256 / SHARD_DIV;
        let mut size = 0;
        let mut found_it = false;
        let idx = &self.indexer;
        idx.for_each_value(height, key_hash, |file_pos| -> bool {
            let mut buf_too_small = false;
            if cache.is_some() {
                cache.unwrap().lookup(shard_id, file_pos, |entry_bz| {
                    found_it = Self::check_entry(key_hash, key, &entry_bz);
                    if found_it {
                        size = entry_bz.len();
                        if buf.len() < size {
                            buf_too_small = true;
                        } else {
                            buf[..size].copy_from_slice(entry_bz.bz);
                        }
                    }
                });
            }
            if found_it || buf_too_small {
                return true; //stop loop if key matches or buf is too small
            }
            size = self.entry_files[shard_id].read_entry(file_pos, buf);
            if buf.len() < size {
                return true; //stop loop if buf is too small
            }
            let entry_bz = EntryBz { bz: &buf[..size] };
            found_it = Self::check_entry(key_hash, key, &entry_bz);
            if found_it && cache.is_some() {
                cache.unwrap().insert(shard_id, file_pos, &entry_bz);
            }
            found_it // stop loop if key matches
        });
        (size, found_it)
    }

    pub fn add_task(&self, task_id: i64) {
        for sender in &self.task_senders {
            sender.send(task_id).unwrap();
        }
    }

    pub fn start_threads(
        &mut self,
        mut flusher: Flusher,
        compact_thres: i64,
        utilization_ratio: i64,
        utilization_div: i64,
        task_chan_size: usize,
        prefetcher_thread_count: usize,
        #[cfg(feature = "directio")] uring_count: usize,
        #[cfg(feature = "directio")] uring_size: u32,
        #[cfg(feature = "directio")] sub_id_chan_size: usize,
    ) {
        let meta = self.meta.read_arc();
        let curr_height = meta.get_curr_height() + 1;

        Indexer::start_compacting(self.indexer.clone());

        let job = CompactJob {
            old_pos: 0,
            entry_bz: Vec::with_capacity(DEFAULT_ENTRY_SIZE),
        };
        #[cfg(all(target_os = "linux", feature = "directio"))]
        let mut job_man = JobManager::new(uring_count, uring_size, sub_id_chan_size);
        #[cfg(not(all(target_os = "linux", feature = "directio")))]
        let mut job_man = JobManager::new();

        let (task_sender, task_receiver) = sync_channel(task_chan_size);
        self.task_senders.push(task_sender);
        for shard_id in 0..SHARD_COUNT {
            let (mid_sender, mid_receiver) = sync_channel(task_chan_size);
            let entryfile_size = meta.get_entry_file_size(shard_id);
            let (u_eb_wr, u_eb_rd) = entrybuffer::new(entryfile_size, self.wrbuf_size);
            let entry_file = flusher.get_entry_file(shard_id);
            job_man.add_shard(u_eb_wr.entry_buffer.clone(), entry_file.clone(), mid_sender);

            let (cmpt_producer, cmpt_consumer) = ringchannel::new(COMPACT_RING_SIZE, &job);
            let mut compactor = Compactor::new(
                shard_id,
                compact_thres as usize / 10,
                entry_file.clone(),
                self.indexer.clone(),
                cmpt_producer,
            );
            let compact_start = meta.get_oldest_active_file_pos(shard_id);
            thread::spawn(move || {
                compactor.fill_compact_chan(compact_start);
            });

            let sn_start = meta.get_oldest_active_sn(shard_id);
            let sn_end = meta.get_next_serial_num(shard_id);
            let mut updater = Updater::new(
                shard_id,
                self.task_hub.clone(),
                u_eb_wr,
                entry_file,
                self.indexer.clone(),
                -1, // curr_version, will be overwritten
                sn_start,
                sn_end,
                cmpt_consumer,
                compact_start,
                utilization_div,
                utilization_ratio,
                compact_thres,
                curr_height << IN_BLOCK_IDX_BITS,
            );
            thread::spawn(move || loop {
                let (task_id, next_task_id) = mid_receiver.recv().unwrap_or((-1, -1));
                if task_id == -1 {
                    error!("mid_receiver for updater exit!!");
                    return;
                }
                updater.run_task_with_ooo_id(task_id, next_task_id);
            });
            flusher.set_entry_buf_reader(shard_id, u_eb_rd)
        }
        let prefetcher = Prefetcher::new(
            self.task_hub.clone(),
            Arc::new(EntryCache::new_uninit()),
            self.indexer.clone(),
            Arc::new(ThreadPool::new(prefetcher_thread_count)),
            job_man,
        );
        prefetcher.start_threads(task_receiver);
        drop(meta);
        thread::spawn(move || {
            flusher.flush(SHARD_COUNT);
        });
    }

    pub fn get_metadb(&self) -> Arc<RwLock<MetaDB>> {
        self.meta.clone()
    }
}

pub struct AdsWrap<T: Task> {
    task_hub: Arc<BlockPairTaskHub<T>>,
    ads: Arc<AdsCore>,
    cache: Arc<EntryCache>,
    cache_list: Vec<Arc<EntryCache>>,
    // when ads finish the prev block disk job, end_block_chan will receive MetaInfo
    end_block_chan: Receiver<Arc<MetaInfo>>,
    stop_height: i64,
}

pub struct SharedAdsWrap {
    ads: Arc<AdsCore>,
    cache: Arc<EntryCache>,
}

impl<T: Task + 'static> AdsWrap<T> {
    pub fn new(config: &config::Config) -> Self {
        #[cfg(feature = "tee_cipher")]
        assert!(config.aes_keys.unwrap().len() == 96);

        Self::_new(
            &config.dir,
            config.wrbuf_size,
            config.file_segment_size,
            config.with_twig_file,
            config.compact_thres,
            config.utilization_ratio,
            config.utilization_div,
            &config.aes_keys,
            config.task_chan_size,
            config.prefetcher_thread_count,
            #[cfg(feature = "directio")]
            config.uring_count,
            #[cfg(feature = "directio")]
            config.uring_size,
            #[cfg(feature = "directio")]
            config.sub_id_chan_size,
        )
    }

    fn _new(
        dir: &str,
        wrbuf_size: usize,
        file_segment_size: usize,
        with_twig_file: bool,
        compact_thres: i64,
        utilization_ratio: i64,
        utilization_div: i64,
        aes_keys: &Option<[u8; 96]>,
        task_chan_size: usize,
        prefetcher_thread_count: usize,
        #[cfg(feature = "directio")] uring_count: usize,
        #[cfg(feature = "directio")] uring_size: u32,
        #[cfg(feature = "directio")] sub_id_chan_size: usize,
    ) -> Self {
        let task_hub = Arc::new(BlockPairTaskHub::<T>::new());
        let (mut ads, end_block_chan, flusher) = AdsCore::_new(
            task_hub.clone(),
            dir,
            wrbuf_size,
            file_segment_size,
            with_twig_file,
            aes_keys,
        );
        ads.start_threads(
            flusher,
            compact_thres,
            utilization_ratio,
            utilization_div,
            task_chan_size,
            prefetcher_thread_count,
            #[cfg(feature = "directio")]
            uring_count,
            #[cfg(feature = "directio")]
            uring_size,
            #[cfg(feature = "directio")]
            sub_id_chan_size,
        );

        Self {
            task_hub,
            ads: Arc::new(ads),
            cache: Arc::new(EntryCache::new_uninit()),
            cache_list: Vec::new(),
            end_block_chan,
            stop_height: -1,
        }
    }

    pub fn get_indexer(&self) -> Arc<Indexer> {
        self.ads.indexer.clone()
    }

    pub fn get_entry_files(&self) -> Vec<Arc<EntryFile>> {
        self.ads.get_entry_files()
    }

    pub fn get_proof(&self, shard_id: usize, sn: u64) -> Result<ProofPath, String> {
        self.ads.get_proof(shard_id, sn)
    }

    pub fn flush(&mut self) -> Vec<Arc<MetaInfo>> {
        let mut v = Vec::with_capacity(2);
        while self.task_hub.free_slot_count() < 2 {
            let meta_info = self.end_block_chan.recv().unwrap();
            self.task_hub.end_block(meta_info.curr_height);
            v.push(meta_info);
        }
        v
    }

    fn allocate_cache(&mut self) -> Arc<EntryCache> {
        let mut idx = usize::MAX;
        for (i, arc) in self.cache_list.iter().enumerate() {
            if Arc::strong_count(arc) == 1 && Arc::weak_count(arc) == 0 {
                idx = i;
                break;
            }
        }
        if idx != usize::MAX {
            let cache = self.cache_list[idx].clone();
            cache.clear();
            return cache;
        }
        let cache = Arc::new(EntryCache::new());
        self.cache_list.push(cache.clone());
        cache
    }

    pub fn set_stop_block(&mut self, height: i64) {
        self.stop_height = height;
    }

    pub fn start_block(
        &mut self,
        height: i64,
        tasks_manager: Arc<TasksManager<T>>,
    ) -> (bool, Option<Arc<MetaInfo>>) {
        if height == self.stop_height + 1 {
            return (false, Option::None);
        }
        self.cache = self.allocate_cache();

        let mut meta_info = Option::None;
        if self.task_hub.free_slot_count() == 0 {
            // adscore and task_hub are busy, wait for them to finish an old block
            let _meta_info = self.end_block_chan.recv().unwrap();
            self.task_hub.end_block(_meta_info.curr_height);
            meta_info = Some(_meta_info);
        }

        self.task_hub
            .start_block(height, tasks_manager, self.cache.clone());
        (true, meta_info)
    }

    pub fn get_shared(&self) -> SharedAdsWrap {
        SharedAdsWrap {
            ads: self.ads.clone(),
            cache: self.cache.clone(),
        }
    }

    pub fn get_metadb(&self) -> Arc<RwLock<MetaDB>> {
        self.ads.get_metadb()
    }
}

pub trait ADS: Send + Sync + 'static {
    fn read_entry(&self, height: i64, key_hash: &[u8], key: &[u8], buf: &mut [u8])
        -> (usize, bool);
    fn warmup<F>(&self, height: i64, k80: &[u8], access: F)
    where
        F: FnMut(EntryBz);
    fn add_task(&self, task_id: i64);

    fn insert_extra_data(&self, height: i64, data: String);

    fn get_root_hash_of_height(&self, height: i64) -> [u8; 32];
}

impl ADS for SharedAdsWrap {
    fn read_entry(
        &self,
        height: i64,
        key_hash: &[u8],
        key: &[u8],
        buf: &mut [u8],
    ) -> (usize, bool) {
        let cache = if height < 0 {
            None
        } else {
            Some(self.cache.as_ref())
        };
        self.ads.read_entry(height, key_hash, key, cache, buf)
    }

    fn warmup<F>(&self, height: i64, k80: &[u8], access: F)
    where
        F: FnMut(EntryBz),
    {
        self.ads.warmup_cache(height, k80, &self.cache, access);
    }

    fn add_task(&self, task_id: i64) {
        self.ads.add_task(task_id);
    }

    fn insert_extra_data(&self, height: i64, data: String) {
        self.ads
            .get_metadb()
            .write()
            .insert_extra_data(height, data);
    }

    fn get_root_hash_of_height(&self, height: i64) -> [u8; 32] {
        return self.ads.get_metadb().read().get_hash_of_root_hash(height);
    }
}

impl SharedAdsWrap {
    pub fn new(ads: Arc<AdsCore>, cache: Arc<EntryCache>) -> Self {
        SharedAdsWrap { ads, cache }
    }
}

#[cfg(test)]
mod tests {

    use std::collections::HashSet;

    use config::Config;
    use seqads::task::TaskBuilder;

    use crate::{
        tasks::BlockPairTaskHub,
        test_helper::{SimpleTask, TempDir},
    };

    use super::*;

    #[test]
    fn test_init_dir() {
        let dir = "test_init_dir";
        let tmp_dir = TempDir::new(dir);
        let mut config = config::Config::from_dir(dir);
        config.set_with_twig_file(true);
        #[cfg(feature = "tee_cipher")]
        config.set_aes_keys([1; 96]);

        AdsCore::init_dir(&config);

        assert_eq!(
            tmp_dir.list().join(","),
            ["test_init_dir/data", "test_init_dir/metadb",].join(",")
        );
        assert_eq!(
            TempDir::list_dir("test_init_dir/data")
                .iter()
                .collect::<HashSet<_>>(),
            [
                (0..SHARD_COUNT)
                    .map(|i| format!("test_init_dir/data/entries{}", i))
                    .collect::<Vec<String>>(),
                (0..SHARD_COUNT)
                    .map(|i| format!("test_init_dir/data/twig{}", i))
                    .collect::<Vec<String>>()
            ]
            .concat()
            .iter()
            .collect::<HashSet<_>>()
        );

        let (_data_dir, meta_dir, _indexer_dir) = AdsCore::get_sub_dirs(dir);
        let mut meta = MetaDB::with_dir(&meta_dir, get_ciphers(&config.aes_keys).2);
        meta.reload_from_file();
        #[cfg(not(feature = "tee_cipher"))]
        for i in 0..SHARD_COUNT {
            assert_eq!(
                4096 * 88 * (16 / SHARD_COUNT as i64),
                meta.get_entry_file_size(i)
            );
            assert_eq!(
                147416 * (16 / SHARD_COUNT as i64),
                meta.get_twig_file_size(i)
            );
            assert_eq!(
                4096 * (16 / SHARD_COUNT as u64),
                meta.get_next_serial_num(i)
            );
        }
        #[cfg(feature = "tee_cipher")]
        for i in 0..SHARD_COUNT {
            assert_eq!(
                4096 * (88 + crate::def::TAG_SIZE) * (16 / SHARD_COUNT),
                meta.get_entry_file_size(i) as usize
            );
            assert_eq!(
                147416 * (16 / SHARD_COUNT as i64),
                meta.get_twig_file_size(i)
            );
            assert_eq!(
                4096 * (16 / SHARD_COUNT as u64),
                meta.get_next_serial_num(i)
            );
        }
    }

    #[test]
    fn test_adscore() {
        let dir = "test_adscore";
        let _tmp_dir = TempDir::new(dir);
        let task_hub = Arc::new(BlockPairTaskHub::<SimpleTask>::new());
        let mut config = config::Config::from_dir(dir);
        if cfg!(feature = "tee_cipher") {
            config.set_aes_keys([1; 96]);
        }

        AdsCore::init_dir(&config);
        let _ads_core = AdsCore::new(task_hub, &config);
        // assert_eq!("?", tmp_dir.list().join("\n"));
    }

    #[test]
    fn test_start_block() {
        let ads_dir = "./test_start_block";
        let _tmp_dir = TempDir::new(ads_dir);

        let config = Config::from_dir(ads_dir);
        AdsCore::init_dir(&config);

        let mut ads = AdsWrap::new(&config);

        for h in 1..=3 {
            let task_id = h << IN_BLOCK_IDX_BITS;
            let r = ads.start_block(
                h,
                Arc::new(TasksManager::new(
                    vec![RwLock::new(Some(
                        TaskBuilder::new()
                            .create(&(h as u64).to_be_bytes(), b"v1")
                            .build(),
                    ))],
                    task_id,
                )),
            );
            assert_eq!(r.0, true);
            if h <= 2 {
                assert!(r.1.is_none());
            } else {
                assert_eq!(r.1.as_ref().unwrap().curr_height, 1);
                assert_eq!(r.1.as_ref().unwrap().extra_data, format!("height:{}", 1));
            }
            let shared_ads = ads.get_shared();
            shared_ads.insert_extra_data(h, format!("height:{}", h));
            shared_ads.add_task(task_id);
        }
        let r = ads.flush();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].curr_height, 2);
        assert_eq!(r[1].curr_height, 3);
    }
}
