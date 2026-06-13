use std::sync::Arc;

use crate::rain_mpt::EMPTY_ROOT;

use super::*;
use kvdb::KeyValueDB;
use kvdb_memorydb;
use rand::prelude::*;
use rand::rngs::StdRng;

type Bytes = Vec<u8>;

fn new_db() -> Arc<dyn KeyValueDB> {
    Arc::new(kvdb_memorydb::create(1))
}

#[test]
fn test_put_random() {
    let mut rng = StdRng::seed_from_u64(123);

    let mut trie = MerklePatriciaTree::<3>::new(new_db());
    let mut tasks: Vec<(Bytes, Bytes)> = (0..=255u8).map(|x| (vec![x], vec![x])).collect();
    tasks.shuffle(&mut rng);

    for (key, value) in tasks.drain(..) {
        trie.put(key, value);
    }

    for i in 0..=255u8 {
        assert_eq!(
            trie.get(vec![i]),
            Some(vec![i]),
            "Fail on position {} before commit",
            i
        );
    }

    let first_hash = trie.commit().unwrap();

    for i in 0..=255u8 {
        assert_eq!(
            trie.get(vec![i]),
            Some(vec![i]),
            "Fail on position {} after commit",
            i
        );
    }

    let mut another_trie = MerklePatriciaTree::<3>::new(new_db());

    for i in 0..=255 {
        another_trie.put(vec![i], vec![i]);
    }
    let second_hash = another_trie.commit().unwrap();

    assert_eq!(first_hash, second_hash);
}

#[test]
fn comprehensive_test_with_similar_prefix() {
    let mut rng = StdRng::seed_from_u64(124);
    let last_byte = vec![0x00, 0xf0, 0xff];
    let make_key =
        |x: usize| -> Vec<u8> { [&vec![0xff; x / 3][..], &last_byte[x % 3..=x % 3]].concat() };
    let make_value = |x: usize| -> Vec<u8> { vec![x as u8] };
    const SAMPLES: usize = 256;

    let mut trie = MerklePatriciaTree::<1000>::new(new_db());
    let mut tasks: Vec<usize> = (0..SAMPLES).collect();
    tasks.shuffle(&mut rng);

    // Check put consistensy
    for i in tasks.drain(..) {
        trie.put(make_key(i), make_value(i));
    }

    for i in 0..SAMPLES {
        assert_eq!(
            trie.get(make_key(i)),
            Some(make_value(i)),
            "Fail on position {} before commit",
            i
        );
    }

    let first_hash = trie.commit().unwrap();

    for i in 0..SAMPLES {
        assert_eq!(
            trie.get(make_key(i)),
            Some(make_value(i)),
            "Fail on position {} after commit",
            i
        );
    }

    // Check put consistensy
    let db2 = new_db();
    let mut trie2 = MerklePatriciaTree::<3>::new(db2.clone());
    for i in 0..SAMPLES {
        trie2.put(make_key(i), make_value(i));
    }

    let second_hash = trie2.commit().unwrap();
    assert_eq!(first_hash, second_hash);

    // Check the number of cached nodes.
    assert!(trie2.loaded_nodes_count() <= 5);

    // Read all the data and check the number cached nodes.
    (0..SAMPLES).for_each(|x| {
        trie2.get(make_key(x));
    });
    assert!(trie2.loaded_nodes_count() > SAMPLES / 3);
    trie2.commit().unwrap();
    assert!(trie2.loaded_nodes_count() <= 5);

    // Check flush top layers
    let cached_nodes = trie2.loaded_nodes_count();
    let dumped_nodes = db2.iter_from_prefix(0, &[]).count();
    trie2.flush_all().unwrap();
    let new_dumped_nodes = db2.iter_from_prefix(0, &[]).count();
    assert_eq!(cached_nodes + dumped_nodes, new_dumped_nodes);

    // Check reload trie from db
    let mut trie2 = MerklePatriciaTree::<3>::new(db2.clone());
    trie2.commit().unwrap();
    assert!(trie2.loaded_nodes_count() <= 5);

    // Check deletion
    let mut tasks: Vec<usize> = (0..SAMPLES).collect();
    tasks.shuffle(&mut rng);
    for i in tasks.drain(..) {
        assert_eq!(
            trie2.get(make_key(i)),
            Some(make_value(i)),
            "Fail in deletion",
        );
        trie2.put(make_key(i), vec![]);
    }

    let empty_hash = trie2.commit().unwrap();

    // Check no memory leak
    assert!(trie2.loaded_nodes_count() <= 5);
    assert_eq!(empty_hash, EMPTY_ROOT);

    // Check no leak on db
    assert!(db2.iter_from_prefix(0, &vec![]).next().is_none());
}
