use parking_lot::RwLock;
use qmdb::config::Config;
use qmdb::def::{IN_BLOCK_IDX_BITS, OP_CREATE};
use qmdb::entryfile::EntryBz;
use qmdb::tasks::TasksManager;
use qmdb::test_helper::{SimpleTask, TempDir};
use qmdb::utils::changeset::ChangeSet;
use qmdb::utils::{byte0_to_shard_id, hasher};
use qmdb::{AdsCore, AdsWrap, ADS};
use std::fs::remove_dir_all;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

#[cfg(not(feature = "tee_cipher"))]
#[test]
fn test_consistency() {
    let ads_dir = "./SEQADS-consistency";
    let _tmp_dir = TempDir::new(ads_dir);

    let config = Config::from_dir_and_compact_opt(ads_dir, 1, 1, 1);
    AdsCore::init_dir(&config);
    let mut ads = AdsWrap::new(&config);
    let mut change_set = ChangeSet::new();
    let address = [1; 20];
    let key_hash = hasher::hash(address);
    let shard_id = byte0_to_shard_id(key_hash[0]) as u8;
    println!("shard id:{}", shard_id);
    change_set.add_op(
        OP_CREATE,
        shard_id,
        &key_hash,    //used during sort as key_hash
        &address[..], //the key
        &[2; 20],     //the value
        None,
    );
    let address2 = [3; 20];
    let key_hash2 = hasher::hash(address2);
    let shard_id2 = byte0_to_shard_id(key_hash2[0]) as u8;
    println!("shard id2:{}", shard_id2);
    change_set.add_op(
        OP_CREATE,
        shard_id2,
        &key_hash2,    //used during sort as key_hash
        &address2[..], //the key
        &[4; 20],      //the value
        None,
    );
    change_set.sort();
    let mut change_set2 = ChangeSet::new();
    let address3 = [5; 20];
    let key_hash3 = hasher::hash(address3);
    let shard_id3 = byte0_to_shard_id(key_hash3[0]) as u8;
    println!("shard id3:{}", shard_id3);
    change_set2.add_op(
        OP_CREATE,
        shard_id3,
        &key_hash3,    //used during sort as key_hash
        &address3[..], //the key
        &[6; 20],      //the value
        None,
    );
    change_set2.sort();
    let change_sets = vec![change_set];
    let task = SimpleTask::new(change_sets);
    let mut tasks = vec![];
    tasks.push(RwLock::new(Some(task)));
    let change_sets = vec![change_set2];
    let task2 = SimpleTask::new(change_sets);
    tasks.push(RwLock::new(Some(task2)));
    let last_task_id = (1 << IN_BLOCK_IDX_BITS) | 1;
    ads.start_block(1, Arc::new(TasksManager::new(tasks, last_task_id)));
    let shared_ads = ads.get_shared();
    let task_id = 1 << IN_BLOCK_IDX_BITS;
    shared_ads.add_task(task_id);
    shared_ads.insert_extra_data(0, "".to_owned());
    let task_id = (1 << IN_BLOCK_IDX_BITS) | 1;
    shared_ads.add_task(task_id);
    shared_ads.insert_extra_data(1, "".to_owned());
    sleep(3 * Duration::from_secs(3));
    let mut bz = [0; 300];
    let (size, found_it) = ads.get_shared().read_entry(1, &key_hash, &address, &mut bz);
    assert!(found_it);
    let entry_bz = EntryBz { bz: &bz[..size] };
    assert_eq!(entry_bz.value(), &[2; 20]);
    let mut bz = [0; 300];
    let (size, found_it) = ads
        .get_shared()
        .read_entry(1, &key_hash2, &address2, &mut bz);
    assert!(found_it);
    let entry_bz = EntryBz { bz: &bz[..size] };
    assert_eq!(entry_bz.value(), &[4; 20]);
    let mut bz = [0; 300];
    let (size, found_it) = ads
        .get_shared()
        .read_entry(1, &key_hash3, &address3, &mut bz);
    assert!(found_it);
    let entry_bz = EntryBz { bz: &bz[..size] };
    assert_eq!(entry_bz.value(), &[6; 20]);
    let root = ads.get_metadb().read().get_root_hash(shard_id as usize);
    println!("root:{:?}", root);
    let root = ads.get_metadb().read().get_root_hash(shard_id2 as usize);
    println!("root2:{:?}", root);
    let root = ads.get_metadb().read().get_root_hash(shard_id3 as usize);
    println!("root3:{:?}", root);
    let _ = remove_dir_all(ads_dir);
}
