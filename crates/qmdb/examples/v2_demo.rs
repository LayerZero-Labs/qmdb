use parking_lot::RwLock;
use qmdb::config::Config;
use qmdb::tasks::TasksManager;
use qmdb::test_helper::SimpleTask;
use qmdb::{AdsCore, AdsWrap, ADS};
use qmdb_common::def::OP_CREATE;
use qmdb_common::entryfile::readbuf::ReadBuf;
use qmdb_common::task::taskid::join_task_id;
use qmdb_common::utils::changeset::ChangeSet;
use std::sync::Arc;

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    let ads_dir = "ADS";
    let config = Config::from_dir(ads_dir);
    // initialize a default ADS with only sentry entries
    AdsCore::init_dir(&config);
    let mut ads = AdsWrap::new(&config);

    // for each block, the Create/Update/Delete operation must be organized into a ordered task list
    let mut task_list = Vec::with_capacity(10);
    for i in 0..10 {
        // each task can have multiple transactions, and each transaction has a changeset
        let mut cset_list = Vec::with_capacity(2);
        for j in 0..2 {
            let mut cset = ChangeSet::new();
            let mut k = [0u8; 32];
            let mut v = [1u8; 32];
            k[0] = i as u8;
            k[1] = j as u8;
            for n in 0..5 {
                k[2] = n as u8;
                v[0] = n as u8;
                let kh = qmdb_common::entryfile::entry::entry_key_hash(&k[..]);
                // add a Create operation into the changeset
                cset.add_op(OP_CREATE, &kh, &k[..], &v[..], None);
            }
            // the operations in changeset must be ordered too
            cset.sort();
            cset_list.push(cset);
        }
        let task = SimpleTask::new(cset_list);
        task_list.push(RwLock::new(Some(task)));
    }

    let height = 1;
    let task_count = task_list.len();
    //add the tasks into QMDB
    ads.start_block(height, Arc::new(TasksManager::new(task_list)));
    //multiple shared_ads can be shared by different threads
    let shared_ads = ads.get_shared();
    //you can associate some extra data in json format to each block
    shared_ads.insert_extra_data(height, "".to_owned());
    for idx in 0..task_count {
        let task_id = join_task_id(height, idx, idx == task_count - 1);
        //pump tasks into QMDB's pipeline
        //in this demo we prepared all the tasks before pumpling them
        //in production you can pump a task immediately after getting it ready
        shared_ads.add_task(task_id);
    }

    //flush QMDB's pipeline to make sure the Create operations are done
    ads.flush();

    let mut k = [0u8; 32];
    k[0] = 1;
    k[1] = 1;
    k[2] = 3;
    let kh = qmdb_common::entryfile::entry::entry_key_hash(&k[..]);
    // now we use another shared_ads to read entry out
    let shared_ads = ads.get_shared();
    let mut buf = ReadBuf::new();
    let _ = shared_ads.read_entry(-1, &kh[..], &[], &mut buf);
    let e = buf.as_entry_bz();
    println!(
        "entry={:?} value={:?} ok={}",
        buf.as_slice(),
        e.value(),
        !buf.is_empty()
    );
}
