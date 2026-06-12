use qmdb::merkletree::{check, helpers::build_test_tree, singleproof::verify_single_proof};
use qmdb_common::def::ENTRY_BASE_LENGTH;
use qmdb_common::def::TWIG_MASK;
use qmdb_common::entryfile::entry;

//fn check_equal(pp: &ProofPath, other: &ProofPath) -> String {
//    if pp.upper_path.len() != other.upper_path.len() {
//        return String::from("UpperPath's length not equal");
//    }
//    if pp.serial_num != other.serial_num {
//        return String::from("SerialNum not equal");
//    }
//    if pp.left_of_twig[0].self_hash != other.left_of_twig[0].self_hash {
//        return String::from("LeftOfTwig.SelfHash not equal");
//    }
//    for i in 0..11 {
//        if pp.left_of_twig[i].peer_hash != other.left_of_twig[i].peer_hash {
//            return String::from("LeftOfTwig.PeerHash not equal");
//        }
//        if pp.left_of_twig[i].peer_at_left != other.left_of_twig[i].peer_at_left {
//            return format!("LeftOfTwig.PeerAtLeft[{i}] not equal");
//        }
//    }
//    if pp.right_of_twig[0].self_hash != other.right_of_twig[0].self_hash {
//        return String::from("RightOfTwig.SelfHash not equal");
//    }
//    for i in 0..3 {
//        if pp.right_of_twig[i].peer_hash != other.right_of_twig[i].peer_hash {
//            return String::from("RightOfTwig.PeerHash not equal");
//        }
//        if pp.right_of_twig[i].peer_at_left != other.right_of_twig[i].peer_at_left {
//            return String::from("RightOfTwig.PeerAtLeft not equal");
//        }
//    }
//    for i in 0..pp.upper_path.len() {
//        if pp.upper_path[i].peer_hash != other.upper_path[i].peer_hash {
//            return String::from("UpperPath.PeerHash not equal");
//        }
//        if pp.upper_path[i].peer_at_left != other.upper_path[i].peer_at_left {
//            return String::from("UpperPath.PeerAtLeft not equal");
//        }
//    }
//    if pp.root != other.root {
//        return String::from("Root not equal");
//    }
//    String::from("")
//}

#[test]
fn test_tree_proof() {
    let temp_dir = ::tempfile::Builder::new()
        .prefix("DataTree-proof")
        .tempdir()
        .unwrap();

    let deact_sn_list: Vec<u64> = (0..2048)
        .chain(vec![5000, 5500, 5700, 5813, 6001])
        .collect();

    let (mut tree, _, _, _) = build_test_tree(
        temp_dir.path().to_str().unwrap(),
        &vec![],
        TWIG_MASK as i32 * 4,
        1600,
    );
    deact_sn_list.iter().for_each(|sn| tree.deactive_entry(*sn));

    let n_list = tree.flush_files(0, 0);
    let n_list = tree.upper_tree.evict_twigs(n_list, 0, 0);

    tree.upper_tree
        .sync_upper_nodes(n_list, tree.youngest_twig_id);
    check::check_hash_consistency(&tree);

    let max_sn = TWIG_MASK as i32 * 4 + 1600;
    for i in 0..max_sn {
        //let mut proof_path = tree.get_proof(i as u64).unwrap();
        //proof_path.check(false).unwrap();

        //let bz = proof_path.to_bytes();
        //let mut path2 = proof::bytes_to_proof_path(&bz).unwrap();
        //let r = check_equal(&proof_path, &path2);
        //assert_eq!(r, String::from(""));
        //path2.check(true).unwrap();

        // Also test compact single-proof (ignores activeness)
        let sp = tree.get_single_proof(i as u64).unwrap();
        assert!(verify_single_proof(&sp));
    }

    // check null entries
    let _max_sn = max_sn + 452;
    let mut bz = [0u8; ENTRY_BASE_LENGTH + 8];
    let null_hash = entry::null_entry(&mut bz[..]).hash();
    for i in max_sn.._max_sn {
        //let mut proof_path = tree.get_proof(i as u64).unwrap();
        //assert_eq!(proof_path.left_of_twig[0].self_hash, null_hash);
        //proof_path.check(false).unwrap();

        //let bz = proof_path.to_bytes();
        //let mut path2 = proof::bytes_to_proof_path(&bz).unwrap();
        //let r = check_equal(&proof_path, &path2);
        //assert_eq!(r, String::from(""));
        //path2.check(true).unwrap();

        // Also test compact single-proof for null entries
        let sp = tree.get_single_proof(i as u64).unwrap();
        assert_eq!(sp.leaf_hash, null_hash);
        assert!(verify_single_proof(&sp));
    }
}

//#[test]
//fn test_tree_get_proof() {
//    let temp_dir = ::tempfile::Builder::new()
//        .prefix("test_tree_get_proof")
//        .tempdir()
//        .unwrap();
//
//    let mut config = Config::from_dir(temp_dir.path().to_str().unwrap());
//    AdsCore::init_dir(&config);
//    config.set_with_twig_file(true);
//
//    let mut ads = Box::new(AdsWrap::new(&config));
//    let ads_p = &mut *ads as *mut AdsWrap<SingleCsTask>;
//
//    let key = b"k12";
//    let hash = qmdb_common::entryfile::entry::entry_key_hash(key);
//    let shard_id = byte0_to_shard_id(hash[0]);
//
//    let handle = thread::spawn(move || {
//        if SHARD_COUNT == 16 {
//            let sp = ads.get_proof(shard_id, 4096).unwrap();
//            assert_eq!(
//                sp.leaf_hash,
//                [
//                    204, 183, 250, 248, 100, 111, 202, 125, 46, 50, 211, 25, 164, 252, 226, 23, 40,
//                    244, 123, 136, 126, 125, 106, 155, 199, 70, 186, 215, 186, 159, 212, 193
//                ]
//            );
//            assert!(verify_single_proof(&sp));
//        } else {
//            let sp = ads.get_proof(shard_id, 4096 * 2).unwrap();
//            assert_eq!(
//                sp.leaf_hash,
//                [
//                    178, 204, 221, 231, 195, 49, 52, 140, 46, 37, 231, 156, 110, 125, 145, 158,
//                    122, 105, 115, 96, 110, 38, 173, 198, 150, 249, 101, 216, 249, 137, 186, 163
//                ]
//            );
//            assert!(verify_single_proof(&sp));
//        }
//    });
//
//    unsafe {
//        let ads = &mut (*ads_p);
//        let task_id = join_task_id(1, 0, true);
//        ads.start_block(
//            1,
//            Arc::new(TasksManager::new(
//                vec![RwLock::new(Some(SingleCsTask::new().create(key, b"v1")))],
//                1,
//            )),
//        );
//        let shared_ads = ads.get_shared();
//        shared_ads.insert_extra_data(1, "".to_owned());
//        shared_ads.add_task(task_id);
//    }
//    handle.join().unwrap();
//}
//
//#[test]
//fn test_tree_get_proof_err() {
//    let temp_dir = ::tempfile::Builder::new()
//        .prefix("test_tree_get_proof_err")
//        .tempdir()
//        .unwrap();
//
//    let config = Config::from_dir(temp_dir.path().to_str().unwrap());
//    AdsCore::init_dir(&config);
//
//    let mut ads = Box::new(AdsWrap::new(&config));
//    let ads_p = &mut *ads as *mut AdsWrap<SingleCsTask>;
//
//    let handle = thread::spawn(move || {
//        let proof = ads.get_proof(5, 4095);
//        assert!(proof.is_err());
//        if let Err(e) = proof {
//            assert_eq!(e, format!("get proof failed: \"twig_file is empty\""));
//        }
//    });
//
//    unsafe {
//        let ads = &mut (*ads_p);
//        let task_id = join_task_id(1, 0, true);
//        ads.start_block(
//            1,
//            Arc::new(TasksManager::new(
//                vec![RwLock::new(Some(SingleCsTask::new().create(b"k12", b"v1")))],
//                1,
//            )),
//        );
//        let shared_ads = ads.get_shared();
//        shared_ads.insert_extra_data(1, "".to_owned());
//        shared_ads.add_task(task_id);
//    }
//    handle.join().unwrap();
//}
