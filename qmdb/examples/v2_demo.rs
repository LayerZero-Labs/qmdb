use parking_lot::RwLock;
use qmdb::config::Config;
use qmdb::def::{DEFAULT_ENTRY_SIZE, IN_BLOCK_IDX_BITS, OP_CREATE};
use qmdb::entryfile::EntryBz;
use qmdb::tasks::TasksManager;
use qmdb::test_helper::SimpleTask;
use qmdb::utils::byte0_to_shard_id;
use qmdb::utils::changeset::ChangeSet;
use qmdb::utils::hasher;
use qmdb::{AdsCore, AdsWrap, ADS};
use std::sync::Arc;

#[cfg(all(not(target_env = "msvc"), feature = "tikv-jemallocator"))]
use tikv_jemallocator::Jemalloc;

#[cfg(all(not(target_env = "msvc"), feature = "tikv-jemallocator"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

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
                let kh = hasher::hash(&k[..]);
                let shard_id = byte0_to_shard_id(kh[0]) as u8;
                // add a Create operation into the changeset
                cset.add_op(OP_CREATE, shard_id, &kh, &k[..], &v[..], None);
            }
            // the operations in changeset must be ordered too
            cset.sort();
            cset_list.push(cset);
        }
        let task = SimpleTask::new(cset_list);
        task_list.push(RwLock::new(Some(task)));
    }

    let height = 1;
    let task_count = task_list.len() as i64;
    //task id's high 40 bits is block height and low 24 bits is task index
    let last_task_id = (height << IN_BLOCK_IDX_BITS) | (task_count - 1);
    //add the tasks into QMDB
    ads.start_block(height, Arc::new(TasksManager::new(task_list, last_task_id)));
    //multiple shared_ads can be shared by different threads
    let shared_ads = ads.get_shared();
    //you can associate some extra data in json format to each block
    shared_ads.insert_extra_data(height, "".to_owned());
    for idx in 0..task_count {
        let task_id = (height << IN_BLOCK_IDX_BITS) | idx;
        //pump tasks into QMDB's pipeline
        //in this demo we prepared all the tasks before pumpling them
        //in production you can pump a task immediately after getting it ready
        shared_ads.add_task(task_id);
    }

    //flush QMDB's pipeline to make sure the Create operations are done
    ads.flush();

    let mut buf = [0; DEFAULT_ENTRY_SIZE];
    let mut k = [0u8; 32];
    k[0] = 1;
    k[1] = 1;
    k[2] = 3;
    let kh = hasher::hash(&k[..]);
    // now we use another shared_ads to read entry out
    let shared_ads = ads.get_shared();
    let (n, ok) = shared_ads.read_entry(-1, &kh[..], &[], &mut buf);
    let e = EntryBz { bz: &buf[..n] };
    println!("entry={:?} value={:?} ok={}", &buf[..n], e.value(), ok);
}
