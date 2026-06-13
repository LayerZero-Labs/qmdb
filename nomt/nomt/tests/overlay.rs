mod common;

use bitvec::prelude::*;
use common::Test;

fn expected_root(items: Vec<([u8; 32], Vec<u8>)>) -> nomt_core::trie::Node {
    nomt_core::update::build_trie::<nomt::hasher::Blake3Hasher>(
        0,
        items
            .into_iter()
            .map(|(k, v)| (k, *blake3::hash(&v).as_bytes())),
        |_| {},
    )
}

#[test]
fn overlay_multiple_forks() {
    let mut test = Test::new("overlay_multiple_forks");

    let overlay_a = test.update().0;
    let overlay_b1 = {
        test.start_overlay_session([&overlay_a]);
        test.write([1; 32], Some(vec![1, 2, 3]));
        test.update().0
    };
    let overlay_b2 = {
        test.start_overlay_session([&overlay_a]);
        test.write([1; 32], Some(vec![4, 5, 6]));
        test.update().0
    };

    {
        test.start_overlay_session([&overlay_b1, &overlay_a]);
        assert_eq!(test.read([1; 32]), Some(vec![1, 2, 3]));
    }

    {
        test.start_overlay_session([&overlay_b2, &overlay_a]);
        assert_eq!(test.read([1; 32]), Some(vec![4, 5, 6]));
    }
}

#[test]
fn overlay_root_calculation() {
    let mut test = Test::new("overlay_root_calculation");
    test.write([1; 32], Some(vec![1, 2, 3]));
    let overlay_a = test.update().0;

    assert_eq!(
        overlay_a.root().into_inner(),
        expected_root(vec![([1; 32], vec![1, 2, 3])]),
    );

    test.start_overlay_session([&overlay_a]);
    test.write([2; 32], Some(vec![4, 5, 6]));
    let overlay_b = test.update().0;

    assert_eq!(
        overlay_b.root().into_inner(),
        expected_root(vec![([1; 32], vec![1, 2, 3]), ([2; 32], vec![4, 5, 6])]),
    );

    test.start_overlay_session([&overlay_b, &overlay_a]);
    test.write([1; 32], Some(vec![7, 8, 9]));
    test.write([3; 32], Some(vec![0, 1, 0]));
    let overlay_c = test.update().0;

    assert_eq!(
        overlay_c.root().into_inner(),
        expected_root(vec![
            ([1; 32], vec![7, 8, 9]),
            ([2; 32], vec![4, 5, 6]),
            ([3; 32], vec![0, 1, 0])
        ]),
    );
}

#[test]
#[should_panic]
fn overlays_must_be_committed_in_order() {
    let mut test = Test::new("overlays_committed_in_order");
    let overlay_a = test.update().0;
    test.start_overlay_session([&overlay_a]);
    let overlay_b = test.update().0;

    test.commit_overlay(overlay_b);
}

#[test]
#[should_panic]
fn overlay_competing_committed() {
    let mut test = Test::new("overlays_competing_committed");
    let overlay_a = test.update().0;
    test.start_overlay_session([&overlay_a]);
    let overlay_b1 = test.update().0;
    test.start_overlay_session([&overlay_a]);
    let overlay_b2 = test.update().0;

    test.commit_overlay(overlay_a);
    test.commit_overlay(overlay_b1);

    test.commit_overlay(overlay_b2);
}

#[test]
fn overlay_commit_in_order_works() {
    let mut test = Test::new("overlays_commit_in_order_works");
    let overlay_a = test.update().0;
    test.start_overlay_session([&overlay_a]);
    let overlay_b = test.update().0;

    test.commit_overlay(overlay_a);
    test.commit_overlay(overlay_b);
}

#[test]
fn overlay_changes_land_on_disk_when_committed() {
    {
        let mut test = Test::new("overlay_changes_land_on_disk");
        test.write([1; 32], Some(vec![1, 2, 3]));
        test.write([2; 32], Some(vec![4, 5, 6]));
        test.write([3; 32], Some(vec![7, 8, 9]));

        let overlay = test.update().0;
        test.commit_overlay(overlay);
    }

    let mut test = Test::new_with_params(
        "overlay_changes_land_on_disk",
        /* commit_concurrency */ 1,
        /* hashtable_buckets */ 1,
        /* panic_on_sync */ None,
        /* cleanup_dir */ false,
    );

    assert_eq!(test.read([1; 32]), Some(vec![1, 2, 3]));
    assert_eq!(test.read([2; 32]), Some(vec![4, 5, 6]));
    assert_eq!(test.read([3; 32]), Some(vec![7, 8, 9]));
}

#[test]
fn overlay_uncommitted_not_on_disk() {
    {
        let mut test = Test::new("overlay_uncommitted_not_on_disk");
        test.write([1; 32], Some(vec![1, 2, 3]));
        test.write([2; 32], Some(vec![4, 5, 6]));
        test.write([3; 32], Some(vec![7, 8, 9]));

        let _overlay = test.update().0;
    }

    let mut test = Test::new_with_params(
        "overlay_uncommitted_not_on_disk",
        /* commit_concurrency */ 1,
        /* hashtable_buckets */ 1,
        /* panic_on_sync */ None,
        /* cleanup_dir */ false,
    );

    assert_eq!(test.read([1; 32]), None);
    assert_eq!(test.read([2; 32]), None);
    assert_eq!(test.read([3; 32]), None);
}

#[test]
fn overlay_deletions() {
    let test_db = || -> Test {
        let mut test = Test::new("overlay_deletions");
        // subtree at 0000000_0/1
        test.write([0; 32], Some(vec![1, 1]));
        test.write([1; 32], Some(vec![2, 2]));

        // subtree at 001000_00/01/10
        test.write([32; 32], Some(vec![1, 1]));
        test.write([33; 32], Some(vec![2, 2]));
        test.write([34; 32], Some(vec![3, 3]));

        // subtree at 100000_00/01/10/11
        test.write([128; 32], Some(vec![4, 4]));
        test.write([129; 32], Some(vec![5, 5]));
        test.write([130; 32], Some(vec![6, 6]));
        test.write([131; 32], Some(vec![7, 7]));

        test.commit();
        test
    };

    // Delete the first item for each subtree
    let mut test = test_db();

    test.write([0; 32], None);
    test.write([32; 32], None);
    test.write([128; 32], None);
    let overlay_a = test.update().0;

    test.start_overlay_session([&overlay_a]);
    assert_eq!(test.read([0; 32]), None);
    assert_eq!(test.read([1; 32]), Some(vec![2, 2]));

    assert_eq!(test.read([32; 32]), None);
    assert_eq!(test.read([33; 32]), Some(vec![2, 2]));
    assert_eq!(test.read([34; 32]), Some(vec![3, 3]));

    assert_eq!(test.read([128; 32]), None);
    assert_eq!(test.read([129; 32]), Some(vec![5, 5]));
    assert_eq!(test.read([130; 32]), Some(vec![6, 6]));
    assert_eq!(test.read([131; 32]), Some(vec![7, 7]));

    let _overlay_b = test.update().0;

    // Delete the second item for each subtree
    let mut test = test_db();

    test.write([1; 32], None);
    test.write([33; 32], None);
    test.write([129; 32], None);
    let overlay_a = test.update().0;

    test.start_overlay_session([&overlay_a]);
    assert_eq!(test.read([0; 32]), Some(vec![1, 1]));
    assert_eq!(test.read([1; 32]), None);

    assert_eq!(test.read([32; 32]), Some(vec![1, 1]));
    assert_eq!(test.read([33; 32]), None);
    assert_eq!(test.read([34; 32]), Some(vec![3, 3]));

    assert_eq!(test.read([128; 32]), Some(vec![4, 4]));
    assert_eq!(test.read([129; 32]), None);
    assert_eq!(test.read([130; 32]), Some(vec![6, 6]));
    assert_eq!(test.read([131; 32]), Some(vec![7, 7]));

    let _overlay_b = test.update().0;

    // Sequence of deletes
    let mut test = test_db();

    test.write([32; 32], None);
    test.write([33; 32], None);
    test.write([128; 32], None);
    test.write([129; 32], None);
    test.write([131; 32], None);
    let overlay_a = test.update().0;

    test.start_overlay_session([&overlay_a]);
    assert_eq!(test.read([32; 32]), None);
    assert_eq!(test.read([33; 32]), None);
    assert_eq!(test.read([34; 32]), Some(vec![3, 3]));

    assert_eq!(test.read([128; 32]), None);
    assert_eq!(test.read([129; 32]), None);
    assert_eq!(test.read([130; 32]), Some(vec![6, 6]));
    assert_eq!(test.read([131; 32]), None);

    let _overlay_b = test.update().0;
}

macro_rules! key_path {
    ($($t:tt)+) => {{
        let mut path = [0u8; 32];
        let slice = bits![u8, Msb0; $($t)+];
        path.view_bits_mut::<Msb0>()[..slice.len()].copy_from_bitslice(&slice);
        path
    }}
}

#[test]
fn overlay_deletions_respected_in_seek() {
    let mut test = Test::new("overlay_deletions_respected_in_seek");

    // key_a will be on disk, deleted in overlay.
    let key_a = key_path![0, 0, 1, 0, 1];

    // key_b will be on disk, after key_a
    let key_b = key_path![0, 0, 1, 0, 1, 1];

    // key_c constrains the leaf node to the path [0, 0, 1]
    let key_c = key_path![0, 0, 0];

    // populate. [0, 0, 1] is an internal node with children for key_a and key_b
    test.write(key_a, Some(vec![42]));
    test.write(key_b, Some(vec![43]));
    test.write(key_c, Some(vec![44]));
    let _ = test.commit();

    // first overlay: delete key_a. [0, 0, 1] is now a leaf for key_b
    test.start_overlay_session(None);
    test.write(key_a, None);
    let overlay_a = test.update().0;

    // second overlay: update key_b's value.
    // the b-tree iterator contains key_a (deleted in overlay) and key_b,
    // so seek must skip over key_a.
    test.start_overlay_session([&overlay_a]);
    test.write(key_b, Some(vec![22]));
    let overlay_b = test.update().0;

    // Now ensure that the trie root is accurate for overlay_c.
    assert_eq!(
        overlay_b.root().into_inner(),
        expected_root(vec![(key_b, vec![22]), (key_c, vec![44]),]),
    )
}

#[test]
fn overlay_earlier_deletions_respected_in_seek() {
    let mut test = Test::new("overlay_earlier_deletions_respected_in_seek");

    // key_a will be introduced in one overlay and deleted in the next
    let key_a = key_path![0, 0, 1, 0, 1];

    // key_b will have a leaf on disk, deleted in an overlay. after key_a
    let key_b = key_path![0, 0, 1, 0, 1, 1];

    // key_c will have a leaf on disk. after key_b
    let key_c = key_path![0, 0, 1, 1, 1, 1, 1];

    // key_d will have a leaf on disk with the goal of constraining the leaf node to the path
    // [0, 0, 1]
    let key_d = key_path![0, 0, 0];

    // populate. [0, 0, 1] is an internal node with children for key_b and key_c
    test.write(key_b, Some(vec![42]));
    test.write(key_c, Some(vec![43]));
    test.write(key_d, Some(vec![44]));
    let _ = test.commit();

    // First overlay: introduce key_a and delete key_b. [0, 0, 1] is
    // now internal with children for key_a and key_c
    test.start_overlay_session(None);
    test.write(key_a, Some(vec![24]));
    test.write(key_b, None);
    let overlay_a = test.update().0;

    // Second overlay: delete key_a. [0, 0, 1] is now a leaf for key_c
    test.start_overlay_session([&overlay_a]);
    test.write(key_a, None);
    let overlay_b = test.update().0;

    // Third update: update key_c. Seek should skip over deleted key_a to find key_b.
    // At this point, the ground truth beatree iterator contains key_b and key_c.
    // Seek will:
    //   1. encounter the leaf for key_c and initiate a leaf fetch
    //   2. gather overlay deletions [key_a, key_b]
    //   3. get first item from btree: key_b > key_a.
    //   4. ignore the deleted key_a and continue to next deletion, key_b.
    //   5. find that key_b is deleted, continue to next item.
    //   6. get next item from btree: key_c == key_c, and return it as the leaf.
    test.start_overlay_session([&overlay_b, &overlay_a]);
    test.write(key_c, Some(vec![22]));
    let overlay_c = test.update().0;

    // Now ensure that the trie root is accurate for overlay_c.
    assert_eq!(
        overlay_c.root().into_inner(),
        expected_root(vec![(key_c, vec![22]), (key_d, vec![44]),]),
    )
}
