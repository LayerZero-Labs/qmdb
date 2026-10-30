use anyhow::Result;
use nomt_core::{hasher::Blake3Hasher, proof, trie::LeafData};

fn main() -> Result<()> {
    // The witness produced in the example `commit_batch` will be used
    let (prev_root, new_root, witness) = commit_batch::NomtDB::commit_batch().unwrap();

    let mut updates = Vec::new();

    // A witness is composed of multiple WitnessedPath objects,
    // which stores all the necessary information to verify the operations
    // performed on the same path
    for (i, witnessed_path) in witness.path_proofs.iter().enumerate() {
        // Constructing the verified operations
        let verified = witnessed_path
            .inner
            .verify::<Blake3Hasher>(&witnessed_path.path.path(), prev_root.into_inner())
            .unwrap();

        // Among all read operations performed the ones that interact
        // with the current verified path are selected
        //
        // Each witnessed operation contains an index to the path it needs to be verified against
        //
        // This information could already be known if we committed the batch initially,
        // and thus, the witnessed field could be discarded entirely.
        for read in witness
            .operations
            .reads
            .iter()
            .skip_while(|r| r.path_index != i)
            .take_while(|r| r.path_index == i)
        {
            match read.value {
                // Check for non-existence if the return value was None
                None => assert!(verified.confirm_nonexistence(&read.key).unwrap()),
                // Verify the correctness of the returned value when it is Some(_)
                Some(value_hash) => {
                    let leaf = LeafData {
                        key_path: read.key,
                        value_hash,
                    };
                    assert!(verified.confirm_value(&leaf).unwrap());
                }
            }
        }

        // The correctness of write operations cannot be easily verified like reads.
        // Write operations need to be collected.
        // All writes that have worked on shared prefixes,
        // such as the witnessed_path, need to be bundled together.
        // Later, it needs to be verified that all these writes bring
        // the new trie to the expected state
        let mut write_ops = Vec::new();
        for write in witness
            .operations
            .writes
            .iter()
            .skip_while(|r| r.path_index != i)
            .take_while(|r| r.path_index == i)
        {
            write_ops.push((write.key, write.value));
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

    Ok(())
}
