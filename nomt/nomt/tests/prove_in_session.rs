mod common;

use bitvec::prelude::*;
use common::Test;
use nomt_core::trie::LeafData;

#[test]
fn prove_in_session() {
    let mut accounts = 0;
    let mut t = Test::new("prove_in_session");

    let _ = t.read_id(0);
    for _ in 0..100 {
        common::set_balance(&mut t, accounts, 1000);
        accounts += 1;
    }
    let root = t.commit().0.into_inner();

    for i in 0..100 {
        let k = common::account_path(i);

        let proof = t.prove_id(i);
        let expected_leaf = LeafData {
            key_path: k,
            value_hash: proof.terminal.as_leaf_option().unwrap().value_hash,
        };
        assert!(proof
            .verify::<nomt::hasher::Blake3Hasher>(k.view_bits::<Msb0>(), root)
            .expect("verification failed")
            .confirm_value(&expected_leaf)
            .unwrap());
    }

    for i in 100..150 {
        let k = common::account_path(i);
        let proof = t.prove_id(i);
        assert!(proof
            .verify::<nomt::hasher::Blake3Hasher>(k.view_bits::<Msb0>(), root)
            .expect("verification failed")
            .confirm_nonexistence(&k)
            .unwrap());
    }
}

#[test]
fn prove_in_session_against_overlay() {
    let mut accounts = 0;
    let mut t = Test::new("prove_in_session_against_overlay");

    let _ = t.read_id(0);
    for _ in 0..100 {
        common::set_balance(&mut t, accounts, 1000);
        accounts += 1;
    }
    let (overlay_a, _) = t.update();
    let root = overlay_a.root().into_inner();
    t.start_overlay_session(&[overlay_a]);

    for i in 0..100 {
        let k = common::account_path(i);

        let proof = t.prove_id(i);
        let expected_leaf = LeafData {
            key_path: k,
            value_hash: proof.terminal.as_leaf_option().unwrap().value_hash,
        };
        assert!(proof
            .verify::<nomt::hasher::Blake3Hasher>(k.view_bits::<Msb0>(), root)
            .expect("verification failed")
            .confirm_value(&expected_leaf)
            .unwrap());
    }

    for i in 100..150 {
        let k = common::account_path(i);
        let proof = t.prove_id(i);
        assert!(proof
            .verify::<nomt::hasher::Blake3Hasher>(k.view_bits::<Msb0>(), root)
            .expect("verification failed")
            .confirm_nonexistence(&k)
            .unwrap());
    }
}

#[test]
fn prove_in_session_no_cache() {
    let mut accounts = 0;

    {
        let mut t = Test::new("prove_in_session_no_cache");

        let _ = t.read_id(0);

        // Write 5000 accounts to ensure a few I/Os will be needed to read the proofs.
        for _ in 0..5000 {
            common::set_balance(&mut t, accounts, 1000);
            accounts += 1;
        }
        t.commit().0.into_inner();
    }

    // Reopen the DB to clear the cache.
    let t = Test::new_with_params(
        "prove_in_session_no_cache",
        1,      // commit concurrency
        10_000, // hashtable buckets
        None,   // panic on sync
        false,  // cleanup dir
    );

    let root = t.root().into_inner();

    for i in 0..100 {
        let k = common::account_path(i);

        let proof = t.prove_id(i);
        let expected_leaf = LeafData {
            key_path: k,
            value_hash: proof.terminal.as_leaf_option().unwrap().value_hash,
        };
        assert!(proof
            .verify::<nomt::hasher::Blake3Hasher>(k.view_bits::<Msb0>(), root)
            .expect("verification failed")
            .confirm_value(&expected_leaf)
            .unwrap());
    }

    for i in 10000..10050 {
        let k = common::account_path(i);
        let proof = t.prove_id(i);
        assert!(proof
            .verify::<nomt::hasher::Blake3Hasher>(k.view_bits::<Msb0>(), root)
            .expect("verification failed")
            .confirm_nonexistence(&k)
            .unwrap());
    }
}
