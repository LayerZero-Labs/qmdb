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
    ads.start_block(height, Arc::new(TasksManager::new(task_list)));
    let shared_ads = ads.get_shared();
    shared_ads.insert_extra_data(height, "".to_owned());
    for idx in 0..task_count {
        let task_id = join_task_id(height, idx, idx == task_count - 1);
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

pub fn read_kv(shared_ads: &SharedAdsWrap, key_list: &[[u8; 20]]) {
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

