use qmdb::merkletree::{check, helpers::build_test_tree, singleproof::verify_single_proof};
use qmdb_common::def::ENTRY_BASE_LENGTH;
use qmdb_common::def::TWIG_MASK;
use qmdb_common::entryfile::entry;

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
        // Also test compact single-proof (ignores activeness)
        let sp = tree.get_single_proof(i as u64).unwrap();
        assert!(verify_single_proof(&sp));
    }

    // check null entries
    let _max_sn = max_sn + 452;
    let mut bz = [0u8; ENTRY_BASE_LENGTH + 8];
    let null_hash = entry::null_entry(&mut bz[..]).hash();
    for i in max_sn.._max_sn {
        // Also test compact single-proof for null entries
        let sp = tree.get_single_proof(i as u64).unwrap();
        assert_eq!(sp.leaf_hash, null_hash);
        assert!(verify_single_proof(&sp));
    }
}
