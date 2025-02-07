use byteorder::{ByteOrder, LittleEndian};
use qmdb::def::{
    calc_max_level, DEFAULT_ENTRY_SIZE, ENTRY_BASE_LENGTH, FIRST_LEVEL_ABOVE_TWIG,
    LEAF_COUNT_IN_TWIG, MAX_UPPER_LEVEL, TWIG_MASK, TWIG_SHARD_COUNT,
};
use qmdb::entryfile::{entry, EntryBz};
use qmdb::merkletree::{check, helpers::build_test_tree, recover, tree, twig, Tree};
use qmdb::test_helper::TempDir;
use qmdb::utils::hasher::{self, Hash32, ZERO_HASH32};
use serial_test::serial;

#[test]
#[serial]
fn test_tree_new() {
    let dir_name = "./DataTree-new";
    let _tmp_dir = TempDir::new(dir_name);

    let tree = Tree::new(
        1,
        1024,
        8 * 1024,
        dir_name.to_string(),
        "test".to_string(),
        true,
        None,
    );
    assert_eq!(
        tree.mtree_for_youngest_twig.as_ref(),
        twig::NULL_MT_FOR_TWIG.as_ref()
    );
    assert_eq!(64, tree.upper_tree.nodes.len());
    assert_eq!(51, MAX_UPPER_LEVEL);
}

#[test]
fn test_tree_init() {
    let l8 = [0u8; 32];
    let mut l9 = [0u8; 32];
    let mut l10 = [0u8; 32];
    let mut l11 = [0u8; 32];
    hasher::node_hash_inplace(8, &mut l9, &l8, &l8);
    hasher::node_hash_inplace(9, &mut l10, &l9, &l9);
    hasher::node_hash_inplace(10, &mut l11, &l10, &l10);

    let null_twig = twig::NULL_TWIG.clone();
    assert_eq!(l9, null_twig.active_bits_mtl1[0]);
    assert_eq!(l9, null_twig.active_bits_mtl1[1]);
    assert_eq!(l9, null_twig.active_bits_mtl1[2]);
    assert_eq!(l9, null_twig.active_bits_mtl1[3]);
    assert_eq!(l10, null_twig.active_bits_mtl2[0]);
    assert_eq!(l10, null_twig.active_bits_mtl2[1]);
    assert_eq!(l11, null_twig.active_bits_mtl3);

    let mut lvl = [[0u8; 32]; 21];
    let mut null_entry_bz = [0u8; ENTRY_BASE_LENGTH + 8];
    let null_hash = entry::null_entry(null_entry_bz.as_mut_slice()).hash();

    lvl[0].copy_from_slice(&null_hash[..]);
    for i in 1..21 {
        let next_level = lvl[i - 1];
        if i == 12 {
            let next_level_right = null_twig.active_bits_mtl3;
            lvl[i].copy_from_slice(
                hasher::hash2((i - 1) as u8, &next_level, &next_level_right).as_slice(),
            );
            continue;
        }
        lvl[i].copy_from_slice(hasher::hash2((i - 1) as u8, &next_level, &next_level).as_slice());
    }

    let null_mt_for_twig = twig::NULL_MT_FOR_TWIG.clone();
    for i in 0..2048 {
        assert_eq!(lvl[0], null_mt_for_twig[2048 + i]);
    }
    for i in 0..1024 {
        assert_eq!(lvl[1], null_mt_for_twig[1024 + i]);
    }
    for i in 0..512 {
        assert_eq!(lvl[2], null_mt_for_twig[512 + i]);
    }
    for i in 0..256 {
        assert_eq!(lvl[3], null_mt_for_twig[256 + i]);
    }
    for i in 0..128 {
        assert_eq!(lvl[4], null_mt_for_twig[128 + i]);
    }
    for i in 0..64 {
        assert_eq!(lvl[5], null_mt_for_twig[64 + i]);
    }
    for i in 0..32 {
        assert_eq!(lvl[6], null_mt_for_twig[32 + i]);
    }
    for i in 0..16 {
        assert_eq!(lvl[7], null_mt_for_twig[16 + i]);
    }
    for i in 0..8 {
        assert_eq!(lvl[8], null_mt_for_twig[8 + i]);
    }
    for i in 0..4 {
        assert_eq!(lvl[9], null_mt_for_twig[4 + i]);
    }
    for i in 0..2 {
        assert_eq!(lvl[10], null_mt_for_twig[2 + i]);
    }
    assert_eq!(lvl[11], null_mt_for_twig[1]);
    assert_eq!(lvl[11], null_twig.left_root);
    assert_eq!(lvl[12], null_twig.twig_root);
    let null_node_in_higher_tree = *twig::NULL_NODE_IN_HIGHER_TREE;
    for i in 13..21 {
        assert_eq!(lvl[i], null_node_in_higher_tree[i]);
    }
}

#[test]
fn test_tree_active_bits() {
    let mut active_bits = twig::ActiveBits::new();
    for i in 0..2048 {
        assert!(!active_bits.get_bit(i));
    }
    let ones = vec![
        1, 2, 3, 54, 199, 200, 29, 37, 1000, 2000, 2008, 799, 2045, 2046, 2047,
    ];
    for i in &ones {
        active_bits.set_bit(*i);
    }
    for i in &ones {
        assert!(active_bits.get_bit(*i));
    }
    for i in &ones {
        active_bits.clear_bit(*i);
    }
    for i in &ones {
        assert!(!active_bits.get_bit(*i));
    }
}

#[test]
fn test_tree_edge_node() {
    let en0 = tree::EdgeNode {
        pos: tree::NodePos::pos(1, 1000),
        value: [10u8; 32],
    };
    let en1 = tree::EdgeNode {
        pos: tree::NodePos::pos(12, 31000),
        value: [11u8; 32],
    };
    let en2 = tree::EdgeNode {
        pos: tree::NodePos::pos(31, 100380),
        value: [12u8; 32],
    };

    let bz = recover::edge_nodes_to_bytes(&vec![en0.clone(), en1.clone(), en2.clone()]);
    let edge_nodes = recover::bytes_to_edge_nodes(bz.as_slice());
    assert_eq!(3, edge_nodes.len());
    assert!(en0.eq(&edge_nodes[0]));
    assert!(en1.eq(&edge_nodes[1]));
    assert!(en2.eq(&edge_nodes[2]));
}

#[test]
fn test_tree_max_level() {
    assert_eq!(12, calc_max_level(0));
    assert_eq!(13, calc_max_level(1));
    assert_eq!(14, calc_max_level(2));
    assert_eq!(14, calc_max_level(3));
    assert_eq!(15, calc_max_level(4));
    assert_eq!(15, calc_max_level(7));
    assert_eq!(16, calc_max_level(8));
    assert_eq!(16, calc_max_level(15));
    assert_eq!(17, calc_max_level(16));
    assert_eq!(18, calc_max_level(32));
    assert_eq!(19, calc_max_level(64));
    assert_eq!(20, calc_max_level(128));
    assert_eq!(21, calc_max_level(256));
    assert_eq!(21, calc_max_level(511));
}

#[test]
fn test_max_n_at_level() {
    assert_eq!(39, calc_max_level(123456789));
    assert_eq!(0, tree::max_n_at_level(123456789, 39));
    assert_eq!(1, tree::max_n_at_level(123456789, 38));
    assert_eq!(3, tree::max_n_at_level(123456789, 37));
    assert_eq!(7, tree::max_n_at_level(123456789, 36));
    assert_eq!(14, tree::max_n_at_level(123456789, 35));
    assert_eq!(29, tree::max_n_at_level(123456789, 34));
}

#[test]
#[serial]
fn test_tree_prune_nodes() {
    let dir_name = "./DataTree-prune-nodes";
    let _tmp_dir = TempDir::new(dir_name);

    let mut tree = Tree::new(
        0,
        1024,
        1024,
        dir_name.to_string(),
        "".to_string(),
        true,
        None,
    );
    let mut stride = 32;
    for level in FIRST_LEVEL_ABOVE_TWIG - 1..FIRST_LEVEL_ABOVE_TWIG + 5 {
        for i in 0..stride {
            tree.upper_tree
                .set_node(tree::NodePos::pos(level as u64, i), [0u8; 32]);
        }
        stride >>= 1;
    }

    tree.youngest_twig_id = 15;
    let bz = tree.upper_tree.prune_nodes(0, 3, tree.youngest_twig_id);
    let edge_nodes = recover::bytes_to_edge_nodes(bz.as_slice());
    assert_eq!(5, edge_nodes.len());
    let zero_bz = [0u8; 32];
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(12, 3),
            value: zero_bz,
        },
        edge_nodes[0]
    );
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(13, 0),
            value: zero_bz,
        },
        edge_nodes[1]
    );
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(14, 0),
            value: zero_bz,
        },
        edge_nodes[2]
    );
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(15, 0),
            value: zero_bz,
        },
        edge_nodes[3]
    );
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(16, 0),
            value: zero_bz,
        },
        edge_nodes[4]
    );

    let bz = tree.upper_tree.prune_nodes(3, 4, tree.youngest_twig_id);
    let edge_nodes = recover::bytes_to_edge_nodes(bz.as_slice());
    assert_eq!(5, edge_nodes.len());
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(12, 4),
            value: zero_bz,
        },
        edge_nodes[0]
    );
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(13, 2),
            value: zero_bz,
        },
        edge_nodes[1]
    );
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(14, 0),
            value: zero_bz,
        },
        edge_nodes[2]
    );
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(15, 0),
            value: zero_bz,
        },
        edge_nodes[3]
    );
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(16, 0),
            value: zero_bz,
        },
        edge_nodes[4]
    );

    let bz = tree.upper_tree.prune_nodes(4, 22, tree.youngest_twig_id);
    let edge_nodes = recover::bytes_to_edge_nodes(bz.as_slice());
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(12, 22),
            value: zero_bz,
        },
        edge_nodes[0]
    );
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(13, 10),
            value: zero_bz,
        },
        edge_nodes[1]
    );
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(14, 4),
            value: zero_bz,
        },
        edge_nodes[2]
    );
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(15, 2),
            value: zero_bz,
        },
        edge_nodes[3]
    );
    assert_eq!(
        tree::EdgeNode {
            pos: tree::NodePos::pos(16, 0),
            value: zero_bz,
        },
        edge_nodes[4]
    );

    for i in 23..32 {
        let n = tree.upper_tree.get_node(tree::NodePos::pos(12, i));
        assert!(n.is_some());
        assert_eq!(&zero_bz, n.unwrap());
    }

    assert_zero(&tree, tree::NodePos::pos(13, 10));
    assert_zero(&tree, tree::NodePos::pos(13, 11));
    assert_zero(&tree, tree::NodePos::pos(13, 12));
    assert_zero(&tree, tree::NodePos::pos(13, 13));
    assert_zero(&tree, tree::NodePos::pos(13, 14));
    assert_zero(&tree, tree::NodePos::pos(13, 15));
    assert_zero(&tree, tree::NodePos::pos(14, 4));
    assert_zero(&tree, tree::NodePos::pos(14, 5));
    assert_zero(&tree, tree::NodePos::pos(14, 6));
    assert_zero(&tree, tree::NodePos::pos(14, 7));
    assert_zero(&tree, tree::NodePos::pos(15, 2));
    assert_zero(&tree, tree::NodePos::pos(15, 3));
    assert_zero(&tree, tree::NodePos::pos(16, 0));
    assert_zero(&tree, tree::NodePos::pos(16, 1));
    assert_zero(&tree, tree::NodePos::pos(17, 0));
}

#[test]
#[serial]
fn test_tree_sync_mt_for_youngest_twig() {
    let dir_name = "./DataTree-sync-mt-for-youngest-twig";
    let _tmp_dir = TempDir::new(dir_name);

    let mut tree = Tree::new(
        0,
        1024,
        1024,
        dir_name.to_string(),
        "".to_string(),
        true,
        None,
    );
    tree.youngest_twig_id = 0;

    let s = tree.youngest_twig_id as usize % TWIG_SHARD_COUNT;
    let i = tree.youngest_twig_id as usize / TWIG_SHARD_COUNT;
    tree.upper_tree
        .active_twig_shards
        .get_mut(s)
        .unwrap()
        .insert(i as u64, twig::NULL_TWIG.clone());
    tree.active_bit_shards
        .get_mut(s)
        .unwrap()
        .insert(i as u64, twig::ActiveBits::new());

    tree.mtree_for_youngest_twig = generate_mt_for_youngest_twig();
    tree.mtree_for_yt_change_start = 0;
    tree.mtree_for_yt_change_end = 2047;
    tree.sync_mt_for_youngest_twig(false);
    check::check_mt(&tree.mtree_for_youngest_twig);

    change_range_in_mt_for_youngest_twig(&mut tree, 0, 0);
    tree.sync_mt_for_youngest_twig(false);
    check::check_mt(&tree.mtree_for_youngest_twig);

    change_range_in_mt_for_youngest_twig(&mut tree, 2047, 2047);
    tree.sync_mt_for_youngest_twig(false);
    check::check_mt(&tree.mtree_for_youngest_twig);

    change_range_in_mt_for_youngest_twig(&mut tree, 0, 1);
    tree.sync_mt_for_youngest_twig(false);
    check::check_mt(&tree.mtree_for_youngest_twig);

    change_range_in_mt_for_youngest_twig(&mut tree, 2046, 2047);
    tree.sync_mt_for_youngest_twig(false);
    check::check_mt(&tree.mtree_for_youngest_twig);

    change_range_in_mt_for_youngest_twig(&mut tree, 10, 11);
    tree.sync_mt_for_youngest_twig(false);
    check::check_mt(&tree.mtree_for_youngest_twig);

    change_range_in_mt_for_youngest_twig(&mut tree, 101, 1100);
    tree.sync_mt_for_youngest_twig(false);
    check::check_mt(&tree.mtree_for_youngest_twig);
}

#[test]
#[serial]
fn test_tree_sync_mt_for_active_bits() {
    let dir_name = "./DataTree-sync-mt-for-active-bits";
    let _tmp_dir = TempDir::new(dir_name);

    let mut tree = Tree::new(
        0,
        1024,
        1024,
        dir_name.to_string(),
        "".to_string(),
        true,
        None,
    );
    init_n_twigs(&mut tree, 7);
    check::check_all_twigs(&tree);

    let n_list0 = vec![0, 1, 2, 3];
    let n_list1 = vec![0, 1, 2];
    let n_list2 = vec![0, 2];

    tree.clear_touched_pos();
    flip_active_bits_every_n(&mut tree, 257);
    let phase1_list = tree.sync_mt_for_active_bits_phase1();
    let phase2_list = tree.upper_tree.sync_mt_for_active_bits_phase2(phase1_list);
    assert_eq!(n_list0, phase2_list);
    check::check_all_twigs(&tree);

    tree.clear_touched_pos();
    flip_active_bits_every_n(&mut tree, 600);
    let phase1_list = tree.sync_mt_for_active_bits_phase1();
    let phase2_list = tree.upper_tree.sync_mt_for_active_bits_phase2(phase1_list);
    assert_eq!(n_list0, phase2_list);
    check::check_all_twigs(&tree);

    tree.clear_touched_pos();
    flip_active_bits_every_n(&mut tree, 3011);
    let phase1_list = tree.sync_mt_for_active_bits_phase1();
    let phase2_list = tree.upper_tree.sync_mt_for_active_bits_phase2(phase1_list);
    assert_eq!(n_list1, phase2_list);
    check::check_all_twigs(&tree);

    tree.clear_touched_pos();
    flip_active_bits_every_n(&mut tree, 5000);
    let phase1_list = tree.sync_mt_for_active_bits_phase1();
    let phase2_list = tree.upper_tree.sync_mt_for_active_bits_phase2(phase1_list);
    assert_eq!(n_list1, phase2_list);
    check::check_all_twigs(&tree);

    tree.clear_touched_pos();
    flip_active_bits_every_n(&mut tree, 9001);
    let phase1_list = tree.sync_mt_for_active_bits_phase1();
    let phase2_list = tree.upper_tree.sync_mt_for_active_bits_phase2(phase1_list);
    assert_eq!(n_list2, phase2_list);
    check::check_all_twigs(&tree);
}

#[test]
#[serial]
fn test_tree_sync_upper_nodes() {
    let dir_name = "./DataTree-sync-upper-nodes";
    let _tmp_dir = TempDir::new(dir_name);

    let mut tree = Tree::new(
        0,
        1024,
        1024,
        dir_name.to_string(),
        "".to_string(),
        true,
        None,
    );
    init_twigs_and_upper_nodes(&mut tree, 171);
    check_node_existence(&tree, 0, 85);
    check::check_upper_nodes(&tree);

    for twig_id in 0..=80 {
        let pos = tree::NodePos::pos(FIRST_LEVEL_ABOVE_TWIG as u64 - 1, twig_id);
        let s = twig_id as usize % TWIG_SHARD_COUNT;
        let k = twig_id / TWIG_SHARD_COUNT as u64;
        let twig = tree.upper_tree.active_twig_shards[s]
            .get(&k)
            .unwrap()
            .clone();
        tree.upper_tree.set_node(pos, twig.twig_root);
        tree.upper_tree.active_twig_shards[s].remove(&k);
        tree.active_bit_shards[s].remove(&k);
    }
    let bz = tree.upper_tree.prune_nodes(0, 80, tree.youngest_twig_id);
    let edge_nodes = recover::bytes_to_edge_nodes(bz.as_slice());
    for en in edge_nodes {
        let n = en.pos;
        println!("edge node: {}-{}", n.level(), n.nth());
    }
    check::check_upper_nodes(&tree);

    let n_list = change_twig_roots(
        &mut tree,
        vec![81, 82, 99, 122, 123, 133, 139, 155, 166, 169, 170],
    );
    tree.upper_tree
        .sync_upper_nodes(n_list, tree.youngest_twig_id);
}

#[test]
#[serial]
fn test_tree_append_entry() {
    let dir_name = "./DataTree-append-entry";
    let _tmp_dir = TempDir::new(dir_name);

    let deact_sn_list = vec![101, 999, 1002];
    let (mut tree, pos_list, max_serial_num, _) =
        build_test_tree(dir_name, &deact_sn_list, TWIG_MASK as i32, 6);

    let n_list = tree.flush_files(0, 0);
    let n_list = tree.upper_tree.evict_twigs(n_list, 0, 0);
    tree.upper_tree
        .sync_upper_nodes(n_list, tree.youngest_twig_id);

    let mut buf = vec![0u8; DEFAULT_ENTRY_SIZE];
    for (i, pos) in pos_list.iter().enumerate() {
        let size = tree
            .entry_file_wr
            .entry_file
            .read_entry(*pos.as_ref().expect("Invalid position"), &mut buf);
        let entry_bz;
        if size <= buf.len() {
            entry_bz = EntryBz { bz: &buf[..size] };
        } else {
            buf = vec![0u8; size];
            tree.entry_file_wr
                .entry_file
                .read_entry(*pos.as_ref().expect("Invalid position"), &mut buf[..]);
            entry_bz = EntryBz { bz: &buf[..size] };
        }
        assert_eq!(i as u64, entry_bz.serial_number());
        if i == TWIG_MASK as usize {
            assert_eq!(deact_sn_list.len(), entry_bz.dsn_count());
            for j in 0..deact_sn_list.len() {
                assert_eq!(deact_sn_list[j], entry_bz.get_deactived_sn(j));
            }
        } else {
            assert_eq!(0, entry_bz.dsn_count());
        }
    }

    let mut active_list = Vec::with_capacity(max_serial_num as usize);
    for i in 0..=max_serial_num {
        let mut active = true;
        for j in 0..deact_sn_list.len() {
            if i == deact_sn_list[j] {
                active = false;
                break;
            }
        }
        assert_eq!(active, tree.get_active_bit(i));
        if active {
            active_list.push(i);
        }
    }

    check::check_hash_consistency(&tree);
    tree.close();
}

// --- helper functions ---

fn assert_zero(tree: &Tree, pos: tree::NodePos) {
    let zero = [0u8; 32];
    let v = tree.upper_tree.get_node(pos);
    assert!(v.is_some());
    assert_eq!(zero.to_vec(), v.unwrap());
}

fn generate_mt_for_youngest_twig() -> Box<[Hash32]> {
    let mut mt = vec![ZERO_HASH32; 4096].into_boxed_slice();
    for i in 0..4096 {
        for j in (0..32).step_by(2) {
            LittleEndian::write_u16(mt[i][j..j + 2].as_mut(), i as u16);
        }
    }
    mt
}

fn change_range_in_mt_for_youngest_twig(tree: &mut Tree, start: usize, end: usize) {
    for i in start..=end {
        for j in 0..32 {
            match tree.mtree_for_youngest_twig[2048 + i][j] {
                255 => tree.mtree_for_youngest_twig[2048 + i][j] = 0,
                _ => tree.mtree_for_youngest_twig[2048 + i][j] += 1,
            }
        }
    }
    tree.mtree_for_yt_change_start = start as i32;
    tree.mtree_for_yt_change_end = end as i32;
}

fn init_n_twigs(tree: &mut Tree, n: u64) {
    for i in 0..n {
        let s = i as usize % TWIG_SHARD_COUNT;
        let i = i / TWIG_SHARD_COUNT as u64;
        tree.upper_tree.active_twig_shards[s].insert(i, twig::NULL_TWIG.clone());
        tree.active_bit_shards[s].insert(i, twig::ActiveBits::new());
    }
    tree.youngest_twig_id = n - 1;
}

fn flip_active_bits_every_n(tree: &mut Tree, n: usize) {
    let end = (1 + tree.youngest_twig_id) * LEAF_COUNT_IN_TWIG as u64;
    for i in (0..end).step_by(n) {
        let active = tree.get_active_bit(i);
        tree.set_entry_activiation(i, !active);
    }
}

fn max_n_plus_1_at_level(youngest_twig_id: u64, level: u8) -> u64 {
    if level < FIRST_LEVEL_ABOVE_TWIG as u8 {
        panic!("level is too small");
    }
    let shift = level - FIRST_LEVEL_ABOVE_TWIG as u8;
    let max_n = youngest_twig_id >> shift;
    let mask = (1 << shift) - 1;
    if (youngest_twig_id & mask) != 0 {
        max_n + 1
    } else {
        max_n
    }
}

fn check_node_existence(tree: &Tree, start: u64, end: u64) {
    let max_level = calc_max_level(end);
    for level in FIRST_LEVEL_ABOVE_TWIG..=max_level {
        let s = tree::max_n_at_level(start, level);
        let e = max_n_plus_1_at_level(end, level as u8);
        for i in s..e {
            if tree
                .upper_tree
                .get_node(tree::NodePos::pos(level as u64, i))
                .is_none()
            {
                panic!("Cannot find node at {}-{}", level, i);
            }
        }
    }
}

fn init_twigs_and_upper_nodes(tree: &mut Tree, n: u64) {
    let mut n_list = Vec::with_capacity(n as usize);
    for i in 0..n {
        let s = i as usize % TWIG_SHARD_COUNT;
        let k = i / TWIG_SHARD_COUNT as u64;
        tree.upper_tree.active_twig_shards[s].insert(k, twig::NULL_TWIG.clone());
        tree.active_bit_shards[s].insert(k, twig::ActiveBits::new());

        let twig = tree.upper_tree.active_twig_shards[s].get_mut(&k).unwrap();
        for j in (0..32).step_by(2) {
            LittleEndian::write_u16(twig.twig_root[j..j + 2].as_mut(), i as u16);
        }
        if n_list.is_empty() || n_list[n_list.len() - 1] != i / 2 {
            n_list.push(i / 2);
        }
    }
    tree.youngest_twig_id = n - 1;
    tree.upper_tree
        .sync_upper_nodes(n_list, tree.youngest_twig_id);
}

fn change_twig_roots(tree: &mut Tree, id_list: Vec<u64>) -> Vec<u64> {
    let mut n_list = Vec::with_capacity(id_list.len());
    for twig_id in id_list {
        let s = twig_id as usize % TWIG_SHARD_COUNT;
        let k = twig_id / TWIG_SHARD_COUNT as u64;
        let twig = tree.upper_tree.active_twig_shards[s].get_mut(&k).unwrap();
        for i in 0..32 {
            twig.twig_root[i] += 1;
        }
        if n_list.is_empty() || n_list[n_list.len() - 1] != twig_id / 2 {
            n_list.push(twig_id / 2);
        }
    }
    n_list
}
