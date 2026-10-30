mod common;

use common::Test;
use nomt::{hasher::Blake3Hasher, proof, trie::LeafData};

#[test]
fn produced_witness_validity() {
    let mut accounts = 0;
    let mut t = Test::new("witness_validity");

    let (prev_root, _) = {
        for _ in 0..10 {
            common::set_balance(&mut t, accounts, 1000);
            accounts += 1;
        }
        t.commit()
    };

    let (new_root, witness) = {
        // read all existing accounts.
        for i in 0..accounts {
            t.read_id(i);
        }

        // read some nonexistent accounts.
        for i in 100..105 {
            t.read_id(i);
        }

        // kill half the existing ones.
        for i in 0..5 {
            common::kill(&mut t, i);
        }

        // and add 5 more.
        for _ in 0..5 {
            common::set_balance(&mut t, accounts, 1000);
            accounts += 1;
        }
        t.commit()
    };

    assert_eq!(witness.operations.reads.len(), 15); // 10 existing + 5 nonexisting
    assert_eq!(witness.operations.writes.len(), 10); // 5 deletes + 5 inserts

    let mut updates = Vec::new();
    for (i, witnessed_path) in witness.path_proofs.iter().enumerate() {
        let verified = witnessed_path
            .inner
            .verify::<Blake3Hasher>(&witnessed_path.path.path(), prev_root.into_inner())
            .unwrap();
        for read in witness
            .operations
            .reads
            .iter()
            .skip_while(|r| r.path_index != i)
            .take_while(|r| r.path_index == i)
        {
            match read.value {
                None => assert!(verified.confirm_nonexistence(&read.key).unwrap()),
                Some(ref v) => {
                    let leaf = LeafData {
                        key_path: read.key,
                        value_hash: *v,
                    };
                    assert!(verified.confirm_value(&leaf).unwrap());
                }
            }
        }

        let mut write_ops = Vec::new();
        for write in witness
            .operations
            .writes
            .iter()
            .skip_while(|r| r.path_index != i)
            .take_while(|r| r.path_index == i)
        {
            write_ops.push((write.key, write.value.clone()));
        }

        if !write_ops.is_empty() {
            updates.push(proof::PathUpdate {
                inner: verified,
                ops: write_ops,
            });
        }
    }

    assert_eq!(
        proof::verify_update::<Blake3Hasher>(prev_root.into_inner(), &updates).unwrap(),
        new_root.into_inner(),
    );
}

#[test]
fn empty_witness() {
    let mut accounts = 0;
    let mut t = Test::new("empty_witness");

    let (prev_root, _) = {
        for _ in 0..10 {
            common::set_balance(&mut t, accounts, 1000);
            accounts += 1;
        }
        t.commit()
    };

    // Create a commit with no operations performed
    let (new_root, witness) = t.commit();

    // The roots should be identical since no changes were made
    assert_eq!(prev_root, new_root);

    // The witness should be empty
    assert_eq!(witness.operations.reads.len(), 0);
    assert_eq!(witness.operations.writes.len(), 0);
    assert_eq!(witness.path_proofs.len(), 0);

    // Verify that an empty update produces the same root
    let updates: Vec<proof::PathUpdate> = Vec::new();
    assert_eq!(
        proof::verify_update::<Blake3Hasher>(prev_root.into_inner(), &updates).unwrap(),
        new_root.into_inner(),
    );
}

#[test]
fn test_verify_update_with_identical_paths() {
    use nomt::{
        hasher::Blake3Hasher,
        proof::{verify_update, PathUpdate},
        trie::ValueHash,
    };

    let account0 = 0;

    // Create a simple trie, create an update witness.
    let mut t = Test::new("identical_paths_test");
    common::set_balance(&mut t, account0, 1000);
    let (root, _) = t.commit();
    t.read_id(account0);
    let (_, witness) = t.commit();

    // Using that witness extract and verify the proof.
    let witnessed_path = &witness.path_proofs[0];
    let verified_proof = witnessed_path
        .inner
        .verify::<Blake3Hasher>(&witnessed_path.path.path(), root.into_inner())
        .unwrap();

    // Create two identical PathUpdate objects
    let mut updates = Vec::new();

    // First update
    let value1 = ValueHash::default();
    let ops1 = vec![([0; 32], Some(value1))];
    updates.push(PathUpdate {
        inner: verified_proof.clone(),
        ops: ops1,
    });

    // Second update with identical path
    let value2 = ValueHash::default();
    let ops2 = vec![([1; 32], Some(value2))];
    updates.push(PathUpdate {
        inner: verified_proof, // Using the same verified proof
        ops: ops2,
    });

    // Try to verify the update. We expect an error due to identical paths, because that violates
    // the requirement of ascending keys.
    verify_update::<Blake3Hasher>(root.into_inner(), &updates).unwrap_err();
}
