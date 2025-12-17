// use std::sync::Arc;

// use parking_lot::RwLock;
// pub static mut ADS: [Option<AdsWrap<SimpleTask>>; 2] = [None, None];
use std::sync::{Arc, Mutex}; // Use std::sync::Mutex
use lazy_static::lazy_static;
use parking_lot::RwLock;
use chrono::Utc;

lazy_static! {
    static ref GLOBAL_ADS: [Mutex<Option<AdsWrap<SimpleTask>>>; 2] =
        [Mutex::new(None), Mutex::new(None)];
}

use qmdb::{
    config::Config,
    def::{DEFAULT_ENTRY_SIZE, IN_BLOCK_IDX_BITS},
    tasks::TasksManager,
    test_helper::SimpleTask,
    utils::hasher,
    AdsCore, AdsWrap, SharedAdsWrap, ADS, // <-- 在这里添加 ADS trait
    metadb::MetaDB, indexer::Indexer,
};


pub fn init(qmdb_dir: &str, table_id: usize) {
    // init QMDB
    let qmdb_dir_with_table_id = format!("{}-{}", qmdb_dir, table_id);
    let config = Config::from_dir(&qmdb_dir_with_table_id);
    AdsCore::init_dir(&config);
    let ads = AdsWrap::<SimpleTask>::new(&config);
    
    // Lock the mutex to safely initialize the value inside
    let mut ads_guard = GLOBAL_ADS[table_id].lock().unwrap();
    *ads_guard = Some(ads);
}


pub fn create_kv(tid: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    let task_count = task_list.len() as i64;
    let last_task_id = (height << IN_BLOCK_IDX_BITS) | (task_count - 1);

    // ------------- add timestamp -------------   
    println!(
        "PIPELINE_LOG: GLOBAL_LOCK_WAIT_START height={} timestamp={}",
        height,
        Utc::now().to_rfc3339()
    );
    // -----------------------------------------

    // 1. Lock the mutex. Other threads will wait here if the lock is held.
    let mut ads_guard = GLOBAL_ADS[tid].lock().unwrap();

    // ------------- add timestamp -------------   
    println!(
        "PIPELINE_LOG: GLOBAL_LOCK_WAIT_END height={} timestamp={}",
        height,
        Utc::now().to_rfc3339()
    );
    // -----------------------------------------

    
    // 2. Take the value from *inside* the mutex guard.
    let mut ads = ads_guard.take().unwrap();
    
    // --- The rest of the function is the same ---
    ads.start_block(height, Arc::new(TasksManager::new(task_list, last_task_id)));
    let shared_ads = ads.get_shared();

    // ------------- add timestamp -------------
    println!(
        "PIPELINE_LOG: CLIENT_DISPATCH_START height={} timestamp={}",
        height,
        Utc::now().to_rfc3339()
    );   
    // -----------------------------------------

    shared_ads.insert_extra_data(height, "".to_owned());
    for idx in 0..task_count {
        let task_id = (height << IN_BLOCK_IDX_BITS) | idx;
        shared_ads.add_task(task_id);
    }

    // ------------- add timestamp -------------   
    println!(
        "PIPELINE_LOG: CLIENT_DISPATCH_START height={} timestamp={}",
        height,
        Utc::now().to_rfc3339()
    );
    // -----------------------------------------
    

    // 3. Put the `ads` object back inside the guard.
    *ads_guard = Some(ads);
    
    // 4. The mutex is automatically unlocked when `ads_guard` goes out of scope here.
}


pub fn update_kv(tid: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    create_kv(tid, height, task_list);
}

pub fn delete_kv(tid: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    create_kv(tid, height, task_list);
}

pub fn flush(tid: usize) {
    let mut ads_guard = GLOBAL_ADS[tid].lock().unwrap();
    let mut ads = ads_guard.take().unwrap();
    ads.flush();
    *ads_guard = Some(ads);
}

pub fn get_ads(tid: usize) -> AdsWrap<SimpleTask> {
    // 1. 先 lock() 来获取 MutexGuard
    // 2. 然后在 MutexGuard 保护的 Option 上调用 .take()
    GLOBAL_ADS[tid].lock().unwrap().take().unwrap()
}

pub fn return_ads(tid: usize, ads: AdsWrap<SimpleTask>) {
    // 1. 先 lock() 来获取 MutexGuard (它像一个 &mut Option<...>)
    // 2. 使用 * (解引用) 来给 Mutex 内部的数据赋值
    *GLOBAL_ADS[tid].lock().unwrap() = Some(ads);
}

//let shared_ads = &ads.get_shared();
// pub fn read_kv(shared_ads: &SharedAdsWrap, key_list: &Vec<[u8; 52]>) {
//     rayon::scope(|s| {
//         s.spawn(move |_| {
//             let mut buf = [0; DEFAULT_ENTRY_SIZE];
//             for k in key_list.iter() {
//                 let kh = hasher::hash(&k[..]);
//                 let (_, ok) = shared_ads.read_entry(-1, &kh[..], &[], &mut buf);
//                 if !ok {
//                     panic!("Cannot read entry kh={:?} ", kh);
//                 }
//             }
//         });
//     });
// }

pub fn read_kv(tid: usize, key_list: &Vec<[u8; 52]>) {

    let ads_guard = GLOBAL_ADS[tid].lock().unwrap();
    let shared_ads = if let Some(ads) = ads_guard.as_ref() {
        ads.get_shared()
    } else {
        panic!("read_kv called on uninitialized table_id: {}", tid);
    };
    rayon::scope(|s| {
        s.spawn(move |_| {
            let mut buf = [0; DEFAULT_ENTRY_SIZE];
            for k in key_list.iter() {
                let kh = hasher::hash(&k[..]);
                let (_, ok) = shared_ads.read_entry(-1, &kh[..], &[], &mut buf);
                if !ok {
                    panic!("Cannot read entry kh={:?} ", kh);
                }
            }
        });
    });
}

pub fn get_db_handles(table_id: usize) -> Option<(Arc<RwLock<MetaDB>>, Arc<Indexer>)> {
    // 像其他函数一样锁定全局 ADS
    let ads_guard = GLOBAL_ADS[table_id].lock().unwrap();

    // 使用 as_ref() 来 "窥视" Option 内部，而不 take() 它
    // 这对于并发的只读访问是安全和正确的
    if let Some(ads) = ads_guard.as_ref() {
        // AdsWrap 有 get_metadb 和 get_indexer 方法
        Some((ads.get_metadb(), ads.get_indexer()))
    } else {
        None // DB 可能还没被初始化
    }
}