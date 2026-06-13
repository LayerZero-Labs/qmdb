mod common;

use common::Test;

#[test]
fn last_layer_trie() {
    let mut t = Test::new_with_params(
        "last_layer_trie", // name
        1,                 // commit_concurrency
        10_000,            // hashtable_buckets
        None,              // panic_on_sync
        true,              // cleanup_dir
    );

    let key1 = [170; 32];
    let mut key2 = key1.clone();
    key2[31] = 171;

    // write two leaf nodes at the last layer of the trie
    t.write(key1, Some(vec![1; 128]));
    t.write(key2, Some(vec![2; 128]));
    t.commit();
    assert_eq!(t.read(key1), Some(vec![1; 128]));
    assert_eq!(t.read(key2), Some(vec![2; 128]));

    // modify two leaf nodes at the last layer of the trie
    t.write(key1, Some(vec![3; 100]));
    t.write(key2, Some(vec![4; 100]));
    t.commit();
    assert_eq!(t.read(key1), Some(vec![3; 100]));
    assert_eq!(t.read(key2), Some(vec![4; 100]));

    // delete two leaf nodes at the last layer of the trie
    t.write(key1, None);
    t.write(key2, None);
    t.commit();
    assert_eq!(t.read(key1), None);
    assert_eq!(t.read(key2), None);
}
