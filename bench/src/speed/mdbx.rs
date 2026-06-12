use std::{fs, path::Path};

use log::debug;
use once_cell::sync::OnceCell;
use parking_lot::RwLock;
use qmdb::test_helper::SimpleTask;
use qmdb_common::def::{OP_CREATE, OP_DELETE, OP_READ, OP_WRITE};
use qmdb_common::task::task::Task;
use rayon;
use rayon::iter::*;
use reth_libmdbx::{Environment, Geometry, ObjectLength, WriteFlags};
use std::sync::Arc;

static MDBX_ENV: OnceCell<Arc<RwLock<Environment>>> = OnceCell::new();

pub fn init(mdbx_dir: &str) {
    // init MDBX
    if Path::new(mdbx_dir).exists() {
        fs::remove_dir_all(mdbx_dir).unwrap();
    }
    let map_size = 1024usize * 1024 * 1024 * 1024; //1TB
    let mdbx_env = Environment::builder()
        .set_geometry(Geometry {
            size: Some(0..map_size),
            ..Default::default()
        })
        .open(Path::new(mdbx_dir))
        .unwrap();
    debug!("MDBX: env.map_size={}", mdbx_env.info().unwrap().map_size());
    MDBX_ENV.set(Arc::new(RwLock::new(mdbx_env))).unwrap();
}

pub fn create_kv(_: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    let mdbx_env = MDBX_ENV.get().unwrap();
    mdbx_create_kv(&mut mdbx_env.write(), &task_list);
}

pub fn update_kv(_: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    let mdbx_env = MDBX_ENV.get().unwrap();
    mdbx_update_kv(&mut mdbx_env.write(), &task_list);
}

pub fn delete_kv(_: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    let mdbx_env = MDBX_ENV.get().unwrap();
    mdbx_update_kv(&mut mdbx_env.write(), &task_list);
}

pub fn read_kv(key_list: &[[u8; 20]]) {
    let mdbx_env = MDBX_ENV.get().unwrap();
    let env = mdbx_env.read();
    let txn = &env.begin_ro_txn().unwrap();
    let db = &txn.open_db(None).unwrap();
    rayon::scope(|s| {
        for k in key_list.iter() {
            let _ = qmdb_common::entryfile::entry::entry_key_hash(&k[..]);
            s.spawn(move |_| {
                txn.get::<ObjectLength>(db.dbi(), k).unwrap().unwrap();
            });
        }
    });
}

// ===========
fn mdbx_create_kv(env: &mut Environment, task_list: &[RwLock<Option<SimpleTask>>]) {
    let txn = env.begin_rw_txn().unwrap();
    let db = txn.open_db(None).unwrap();
    for item in task_list.iter() {
        let task_opt = item.read();
        let task = task_opt.as_ref().unwrap();
        for change_set in task.get_change_sets().iter() {
            change_set.for_each_op(|op, _, _kh, k, v, _, _r| {
                if op != OP_CREATE {
                    panic!("only OP_CREATE is allowd in first round");
                }
                txn.put(db.dbi(), k, v, WriteFlags::empty()).unwrap();
            });
        }
    }
    txn.commit().unwrap();
}

fn mdbx_update_kv(env: &mut Environment, task_list: &Vec<RwLock<Option<SimpleTask>>>) {
    std::thread::scope(|s| {
        let handler = s.spawn(|| {
            let txn = env.begin_rw_txn().unwrap();
            let db = txn.open_db(None).unwrap();
            for item in task_list.iter() {
                let task_opt = item.read();
                let task = task_opt.as_ref().unwrap();
                for change_set in task.get_change_sets().iter() {
                    change_set.for_each_op(|op, _, _kh, k, v, _, _r| {
                        if op == OP_WRITE || op == OP_CREATE {
                            txn.put(db.dbi(), k, v, WriteFlags::empty()).unwrap();
                        } else if op == OP_DELETE {
                            txn.del(db.dbi(), k, None).unwrap();
                        }
                    });
                }
            }
            txn
        });
        task_list.par_iter().for_each(|item| {
            let task_opt = item.read();
            let task = task_opt.as_ref().unwrap();
            let txn = env.begin_ro_txn().unwrap();
            let db = txn.open_db(None).unwrap();
            for change_set in task.get_change_sets().iter() {
                change_set.for_each_op(|op, _, _kh, k, _v, _, _r| {
                    if op == OP_WRITE || op == OP_READ {
                        txn.get::<ObjectLength>(db.dbi(), k).unwrap().unwrap();
                    }
                });
            }
        });
        let txn = handler.join().unwrap();
        txn.commit().unwrap();
    });
}
