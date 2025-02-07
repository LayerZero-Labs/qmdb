mod entry_flusher;
mod entry_loader;
mod entry_updater;
mod proof;
pub mod task;

use crate::config::Config;
use crate::def::SHARD_COUNT;
use crate::entryfile::EntryFile;
use crate::entryfile::{EntryBz, EntryCache};
use crate::indexer::Indexer;
use crate::metadb::MetaDB;
use crate::tasks::{Task, TasksManager};
use crate::utils::byte0_to_shard_id;
use crate::utils::changeset::ChangeSet;
use crate::{AdsCore, ADS};
use entry_flusher::{EntryFlusher, FlusherShard};
use entry_loader::EntryLoader;
use entry_updater::EntryUpdater;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub struct SeqAdsWrap<T: Task> {
    tasks_manager: Arc<TasksManager<T>>,
    ads: Arc<SeqAds>,
    cache: Arc<EntryCache>,
}

impl<T: Task + 'static> SeqAdsWrap<T> {
    pub fn new(config: &Config) -> Self {
        let ads = SeqAds::new(config);
        Self {
            tasks_manager: Arc::new(TasksManager::default()),
            ads: Arc::new(ads),
            cache: Arc::new(EntryCache::new_uninit()),
        }
    }

    pub fn start_block(&mut self, _height: i64, tasks_manager: Arc<TasksManager<T>>) {
        self.cache = Arc::new(EntryCache::new());
        self.tasks_manager = tasks_manager;
    }

    pub fn get_shared(&self) -> Self {
        Self {
            tasks_manager: self.tasks_manager.clone(),
            ads: self.ads.clone(),
            cache: self.cache.clone(),
        }
    }

    pub fn commit_block(&mut self, height: i64) {
        self.ads.commit_block(height);
    }

    pub fn commit_tx(&self, task_id: i64, change_sets: &Vec<ChangeSet>) {
        self.ads.commit_tx(task_id, change_sets);
    }

    pub fn get_metadb(&self) -> Arc<RwLock<MetaDB>> {
        self.ads.meta_db.clone()
    }

    pub fn get_entry_files(&self) -> Vec<Arc<EntryFile>> {
        let mut res = Vec::with_capacity(self.ads.entry_files.len());
        for ef in self.ads.entry_files.iter() {
            res.push(ef.clone());
        }
        res
    }
}

pub struct SeqAds {
    height: AtomicI64,
    entry_files: Vec<Arc<EntryFile>>,
    pub indexer: Arc<Indexer>,
    pub meta_db: Arc<RwLock<MetaDB>>,
    entry_cache: EntryCache,
    pub entry_loaders: Vec<Mutex<EntryLoader>>,
    entry_updaters: Vec<Arc<Mutex<EntryUpdater>>>,
    entry_flusher: Arc<Mutex<EntryFlusher>>,
}

impl SeqAds {
    pub fn new(config: &Config) -> Self {
        let (data_dir, meta_dir, indexer_dir) = AdsCore::get_sub_dirs(&config.dir);

        let meta_db = MetaDB::with_dir(&meta_dir, None);
        let meta = Arc::new(RwLock::new(meta_db));

        let curr_height = meta.read().unwrap().get_curr_height();
        let indexer = Arc::new(Indexer::with_dir(indexer_dir));

        let mut entry_files = Vec::with_capacity(SHARD_COUNT);
        let mut shards: Vec<Box<FlusherShard>> = Vec::with_capacity(SHARD_COUNT);
        let mut entry_updaters = Vec::<Arc<Mutex<EntryUpdater>>>::with_capacity(SHARD_COUNT);
        let entry_cache = Arc::new(EntryCache::new());

        assert!(config.with_twig_file);
        assert!(config.aes_keys.is_none());
        for shard_id in 0..SHARD_COUNT {
            let meta_db = meta.read().unwrap();

            let (tree, ef_prune_to, oldest_active_sn) = AdsCore::recover_tree(
                &meta_db,
                data_dir.clone(),
                config.wrbuf_size,
                config.file_segment_size,
                config.with_twig_file,
                curr_height,
                shard_id,
                None,
            );
            AdsCore::index_tree(&tree, oldest_active_sn, ef_prune_to, &indexer);

            let entry_file = tree.entry_file_wr.entry_file.clone();
            entry_files.push(entry_file.clone());
            let sn_end = meta_db.get_next_serial_num(shard_id);
            let compact_start = meta_db.get_oldest_active_file_pos(shard_id);
            let oldest_active_sn = meta_db.get_oldest_active_sn(shard_id);
            let entry_file_size = meta_db.get_entry_file_size(shard_id);
            let updater = Arc::new(Mutex::new(EntryUpdater::new(
                shard_id,
                entry_file_size,
                entry_cache.clone(),
                entry_file.clone(),
                indexer.clone(),
                -1,
                sn_end,
                compact_start,
                config.utilization_div,
                config.utilization_ratio,
                config.compact_thres,
                oldest_active_sn,
            )));
            shards.push(Box::new(FlusherShard::new(
                tree,
                oldest_active_sn,
                shard_id,
                updater.clone(),
                meta.clone(),
            )));
            entry_updaters.push(updater);
        }

        let flusher = Arc::new(Mutex::new(EntryFlusher::new(shards, meta.clone())));
        let mut entry_loaders = Vec::<Mutex<EntryLoader>>::with_capacity(SHARD_COUNT);
        for i in 0..SHARD_COUNT {
            entry_loaders.push(Mutex::new(EntryLoader::new(
                i,
                entry_files[i].clone(),
                entry_cache.clone(),
                indexer.clone(),
            )));
        }

        Self {
            height: AtomicI64::from(curr_height),
            indexer,
            entry_files,
            meta_db: meta.clone(),
            entry_cache: EntryCache::new(),
            entry_loaders,
            entry_updaters,
            entry_flusher: flusher,
        }
    }

    pub fn commit_tx(&self, task_id: i64, change_sets: &Vec<ChangeSet>) {
        let height = self.height.load(Ordering::SeqCst);
        for loader in &self.entry_loaders {
            loader.lock().unwrap().run_task(height, change_sets);
        }
        for updater in &self.entry_updaters {
            updater.lock().unwrap().run_task(task_id, change_sets);
        }
        self.entry_flusher
            .lock()
            .unwrap()
            .flush_tx(SHARD_COUNT, height + 1); // height is update in commit_block
    }

    pub fn commit_block(&self, height: i64) {
        self.height.store(height, Ordering::SeqCst);
        self.entry_flusher.lock().unwrap().flush_block(height);
    }
}

impl<T: Task + 'static> ADS for SeqAdsWrap<T> {
    fn read_entry(
        &self,
        height: i64,
        key_hash: &[u8],
        key: &[u8],
        buf: &mut [u8],
    ) -> (usize, bool) {
        let shard_id = byte0_to_shard_id(key_hash[0]);
        let mut size = 0;
        let mut found_it = false;
        self.ads
            .indexer
            .for_each_value(height, key_hash, |file_pos| -> bool {
                let mut buf_too_small = false;
                self.ads.entry_cache.lookup(shard_id, file_pos, |entry_bz| {
                    found_it = AdsCore::check_entry(key_hash, key, &entry_bz);
                    if found_it {
                        size = entry_bz.len();
                        if buf.len() < size {
                            buf_too_small = true;
                        } else {
                            buf[..size].copy_from_slice(entry_bz.bz);
                        }
                    }
                });
                if found_it || buf_too_small {
                    return true; //stop loop if key matches or buf is too small
                }
                size = self.ads.entry_files[shard_id].read_entry(file_pos, buf);
                if buf.len() < size {
                    return true; //stop loop if buf is too small
                }
                let entry_bz = EntryBz { bz: &buf[..size] };
                found_it = AdsCore::check_entry(key_hash, key, &entry_bz);
                if found_it {
                    self.ads.entry_cache.insert(shard_id, file_pos, &entry_bz);
                }
                found_it // stop loop if key matches
            });
        (size, found_it)
    }

    fn warmup<F>(&self, _: i64, _: &[u8], _: F)
    where
        F: FnMut(EntryBz),
    {
        todo!()
    }

    fn add_task(&self, task_id: i64) {
        let change_sets = self.tasks_manager.get_tasks_change_sets(task_id as usize);
        self.ads.commit_tx(task_id, &change_sets);
    }

    fn insert_extra_data(&self, height: i64, data: String) {
        self.get_metadb()
            .write()
            .unwrap()
            .insert_extra_data(height, data);
    }

    fn get_root_hash_of_height(&self, height: i64) -> [u8; 32] {
        return self
            .get_metadb()
            .read()
            .unwrap()
            .get_hash_of_root_hash(height);
    }
}
