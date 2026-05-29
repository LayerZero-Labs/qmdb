use std::sync::Arc;

use parking_lot::RwLock;
use qmdb::{
    config::Config, tasks::TasksManager, test_helper::SimpleTask, AdsCore, AdsWrap, SharedAdsWrap,
    ADS,
};
use qmdb_common::{
    entryfile::{entry::entry_key_hash, readbuf::ReadBuf},
    task::taskid::join_task_id,
};

pub static mut ADS: [Option<AdsWrap<SimpleTask>>; 2] = [None, None];

pub fn init(qmdb_dir: &str, table_id: usize) {
    // init QMDB
    let qmdb_dir_with_table_id = format!("{qmdb_dir}-{table_id}");
    let config = Config::from_dir(&qmdb_dir_with_table_id);
    AdsCore::init_dir(&config);
    let ads = AdsWrap::<SimpleTask>::new(&config);
    unsafe {
        ADS[table_id] = Some(ads);
    }
}

pub fn create_kv(tid: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    let task_count = task_list.len();
    let mut ads = unsafe { ADS[tid].take().unwrap() };
    //warmup_indexer(&indexer, height, &task_list);
    //fake_indexer(&idx_file, page_count, &task_list);
    ads.start_block(height, Arc::new(TasksManager::new(task_list)));
    let shared_ads = ads.get_shared();
    shared_ads.insert_extra_data(height, "".to_owned());
    for idx in 0..task_count {
        let task_id = join_task_id(height, idx, idx == task_count - 1);
        //println!("AA bench height={} task_id={:#08x}", height, task_id);
        shared_ads.add_task(task_id);
    }
    unsafe {
        ADS[tid] = Some(ads);
    }
}

pub fn update_kv(tid: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    create_kv(tid, height, task_list);
}

pub fn delete_kv(tid: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    create_kv(tid, height, task_list);
}

pub fn flush(tid: usize) {
    let mut ads = unsafe { ADS[tid].take().unwrap() };
    ads.flush();
    unsafe {
        ADS[tid] = Some(ads);
    }
}

pub fn get_ads(tid: usize) -> AdsWrap<SimpleTask> {
    unsafe { ADS[tid].take().unwrap() }
}

pub fn return_ads(tid: usize, ads: AdsWrap<SimpleTask>) {
    unsafe {
        ADS[tid] = Some(ads);
    }
}

//let shared_ads = &ads.get_shared();
pub fn read_kv(shared_ads: &SharedAdsWrap, key_list: &[[u8; 52]]) {
    rayon::scope(|s| {
        s.spawn(move |_| {
            let mut buf = ReadBuf::new();
            for k in key_list.iter() {
                let kh = entry_key_hash(&k[..]);
                let _ = shared_ads.read_entry(-1, &kh[..], &[], &mut buf);
                if buf.is_empty() {
                    panic!("Cannot read entry kh={kh:?} ");
                }
                buf.clear();
            }
        });
    });
}

/// Read values for a batch of keys for correctness checking.
///
/// Only compiled when the `qmdb_bench_correctness` feature is enabled.
/// This helper avoids panicking on missing entries and instead returns
/// `None` for keys that cannot be read.
#[cfg(feature = "qmdb_bench_correctness")]
pub fn read_kv_values(tid: usize, key_list: &[[u8; 52]]) -> Vec<Option<Vec<u8>>> {
    let mut ads = unsafe { ADS[tid].take().expect("ADS not initialized") };
    let shared_ads = ads.get_shared();
    let mut buf = ReadBuf::new();
    let mut result = Vec::with_capacity(key_list.len());

    for k in key_list.iter() {
        let kh = entry_key_hash(&k[..]);
        let _ = shared_ads.read_entry(-1, &kh[..], &[], &mut buf);
        if buf.is_empty() {
            result.push(None);
        } else {
            result.push(Some(buf.as_slice().to_vec()));
            buf.clear();
        }
    }

    unsafe {
        ADS[tid] = Some(ads);
    }

    result
}
