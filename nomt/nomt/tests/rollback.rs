use hex_literal::hex;
use nomt::{
    hasher::Blake3Hasher, trie::KeyPath, KeyReadWrite, Nomt, Options, SessionParams, Value,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

/// Setup a NOMT with the given path, rollback enabled, and the given commit concurrency.
///
/// It's important that tests that run in parallel don't use the same path.
fn setup_nomt(
    path: &str,
    rollback_enabled: bool,
    commit_concurrency: usize,
    should_clean_up: bool,
) -> Nomt<Blake3Hasher> {
    let path = {
        let mut p = PathBuf::from("test");
        p.push(path);
        p
    };
    if should_clean_up && path.exists() {
        std::fs::remove_dir_all(&path).unwrap();
    }
    let mut o = Options::new();
    o.path(path);
    o.commit_concurrency(commit_concurrency);
    o.bitbox_seed([0; 16]);
    o.rollback(rollback_enabled);
    Nomt::open(o).unwrap()
}

#[test]
fn test_rollback_disabled() {
    let nomt = setup_nomt(
        "test_rollback_disabled",
        /* enable_rollback */ false,
        /* commit_concurrency */ 1,
        /* should_clean_up */ true,
    );

    let session = nomt.begin_session(SessionParams::default());
    let finished = session
        .finish(vec![(
            hex!("0000000000000000000000000000000000000000000000000000000000000001"),
            KeyReadWrite::Write(Some(vec![1])),
        )])
        .unwrap();
    finished.commit(&nomt).unwrap();

    let result = nomt.rollback(1);
    // we expect this to fail, because rollback is disabled
    assert!(result.is_err());
}

#[test]
fn test_rollback_to_initial() {
    let nomt = setup_nomt(
        "test_rollback_to_initial",
        /* enable_rollback */ true,
        /* commit_concurrency */ 1,
        /* should_clean_up */ true,
    );

    let session = nomt.begin_session(SessionParams::default());
    let finished = session
        .finish(vec![(
            hex!("0000000000000000000000000000000000000000000000000000000000000001"),
            KeyReadWrite::Write(Some(vec![1])),
        )])
        .unwrap();
    finished.commit(&nomt).unwrap();

    assert_eq!(
        nomt.root().into_inner(),
        hex!("c6e25744545ddabdaf0a95201f8285e670ee9b3e0c1ced4a3006baafd1ac2fdf")
    );

    let result = nomt.rollback(1);
    assert!(result.is_ok());
    assert_eq!(
        nomt.root().into_inner(),
        hex!("0000000000000000000000000000000000000000000000000000000000000000")
    );
}

struct TestPlan {
    /// Every key that will be inserted at some point.
    ///
    /// This is needed for exhaustive verification that no unexpected data is left in the tree.
    every_key: BTreeSet<KeyPath>,
    /// Keys to insert at the i-th commit.
    to_insert: Vec<BTreeMap<KeyPath, Value>>,
    /// Keys to remove at the i-th commit.
    to_remove: Vec<BTreeSet<KeyPath>>,
    /// Expected root before and after applying the i-th commit.
    expected_roots: Vec<[u8; 32]>,
    /// Expected values after applying the i-th commit.
    expected_values: Vec<BTreeMap<KeyPath, Value>>,
}

impl TestPlan {
    /// Generate a test plan for a NOMT with `n` commits. The zero-th commit always corresponds
    /// to the initial empty tree.
    fn generate(name: &'static str, n: usize, overflow: bool) -> Self {
        let mut every_key = BTreeSet::new();
        let mut to_insert = Vec::new();
        let mut to_remove = Vec::new();
        let mut expected_roots = Vec::new();
        let mut expected_values = Vec::new();

        // Zero-th commit, or initial state: empty
        expected_values.push(BTreeMap::new());
        expected_roots.push([0; 32]);

        let mut state_emu = BTreeMap::new();

        for commit_ix in 1..(n + 1) {
            let mut per_commit_insert = BTreeMap::new();
            let mut per_commit_remove = BTreeSet::new();

            // Add 3 new keys each iteration
            for j in 0..3 {
                let mut key = [0u8; 32];
                key[30] = commit_ix as u8;
                key[31] = j as u8;
                let key = *blake3::hash(&key).as_bytes();

                let value: Vec<u8> = if overflow {
                    // 32KB
                    std::iter::repeat(blake3::hash(&key).as_bytes())
                        .take(1024)
                        .flatten()
                        .copied()
                        .collect()
                } else {
                    blake3::hash(&key).as_bytes().to_vec()
                };
                // vec![commit_ix as u8, j as u8];

                per_commit_insert.insert(key, value.clone());
                every_key.insert(key);
                state_emu.insert(key.clone(), value.clone());
            }

            // Remove 1 key (if possible) each iteration
            if commit_ix > 1 {
                let mut key = [0u8; 32];
                key[30] = (commit_ix - 1) as u8;
                key[31] = 0;

                let key = *blake3::hash(&key).as_bytes();
                per_commit_remove.insert(key);
                every_key.insert(key);
                state_emu.remove(&key);
            }

            expected_values.push(state_emu.clone());
            to_insert.push(per_commit_insert);
            to_remove.push(per_commit_remove);
        }

        // Compute expected roots for each commit. We do that using the NOMT with a temporary
        // directory.
        let nomt = setup_nomt(
            &format!("tmp_{name}"),
            /* enable_rollback */ false,
            /* commit_concurrency */ 1,
            /* should_clean_up */ true,
        );

        for commit_no in 0..n {
            println!("commit_no: {}", commit_no);
            println!(
                "adding keys: {}",
                display_keys_and_values(to_insert[commit_no].iter())
            );
            println!(
                "removing keys: {}",
                display_keys(to_remove[commit_no].iter())
            );
            let session = nomt.begin_session(SessionParams::default());
            let mut operations = Vec::new();
            for (key, value) in to_insert[commit_no].iter() {
                operations.push((key.clone(), KeyReadWrite::Write(Some(value.clone()))));
            }
            for key in to_remove[commit_no].iter() {
                operations.push((key.clone(), KeyReadWrite::Write(None)));
            }
            operations.sort_by_key(|(key, _)| key.clone());
            let finished = session.finish(operations).unwrap();
            finished.commit(&nomt).unwrap();
            let post_root = nomt.root();
            expected_roots.push(post_root.into_inner());
        }

        Self {
            every_key,
            to_insert,
            to_remove,
            expected_roots,
            expected_values,
        }
    }

    fn apply_forward(&self, nomt: &mut Nomt<Blake3Hasher>) {
        for commit_no in 0..self.to_insert.len() {
            let session = nomt.begin_session(SessionParams::default());
            let mut operations = Vec::new();
            for (key, value) in self.to_insert[commit_no].iter() {
                operations.push((key.clone(), KeyReadWrite::Write(Some(value.clone()))));
            }
            for key in self.to_remove[commit_no].iter() {
                operations.push((key.clone(), KeyReadWrite::Write(None)));
            }
            operations.sort_by_key(|(key, _)| key.clone());
            let finished = session.finish(operations).unwrap();
            finished.commit(&nomt).unwrap();
        }
    }

    fn verify_restored_state(&self, nomt: &mut Nomt<Blake3Hasher>, commit_ix: usize) {
        let mut errors: Vec<String> = vec![];

        let expected: BTreeMap<KeyPath, Value> = self.expected_values[commit_ix].clone();
        let unexpected: BTreeSet<KeyPath> = self
            .every_key
            .difference(&expected.keys().copied().collect())
            .copied()
            .collect();

        // Verify that all expected keys are in place and have the correct value.
        for (key, expected_value) in expected {
            let actual_value = nomt.read(key).unwrap();
            if actual_value.is_none() {
                errors.push(format!(
                    "missing: the key {:x?} is not present",
                    hex::encode(key)
                ));
            } else if actual_value.as_ref() != Some(&expected_value) {
                errors.push(format!(
                    "wrong value: the key {:x?} has the wrong value. Expected {:x?}, found {:x?}",
                    key,
                    hex::encode(expected_value),
                    hex::encode(actual_value.unwrap())
                ));
            }
        }

        // Verify that no unexpected keys are in the tree.
        for key in unexpected {
            let actual_value = nomt.read(key).unwrap();
            if let Some(value) = actual_value {
                errors.push(format!(
                    "unexpected: the key {:x?} is present in the tree with the value {:x?}",
                    hex::encode(key),
                    hex::encode(value)
                ));
            }
        }

        if nomt.root().into_inner() != self.expected_roots[commit_ix] {
            errors.push(format!(
                "wrong root: expected {:x?}, found {}",
                hex::encode(self.expected_roots[commit_ix]),
                hex::encode(nomt.root().into_inner())
            ));
        }

        if !errors.is_empty() {
            panic!(
                "verification failed of commit {}: \n{}",
                commit_ix,
                errors.join("\n")
            );
        }
    }
}

fn display_keys<'a>(keys: impl IntoIterator<Item = &'a KeyPath>) -> String {
    let mut keys: Vec<_> = keys.into_iter().collect();
    keys.sort();
    keys.iter()
        .map(|k| hex::encode(k))
        .collect::<Vec<_>>()
        .join(", ")
}

fn display_keys_and_values<'a>(kv: impl IntoIterator<Item = (&'a KeyPath, &'a Value)>) -> String {
    let mut kv: Vec<_> = kv.into_iter().collect();
    kv.sort();
    kv.iter()
        .map(|(k, v)| format!("{} -> {}", hex::encode(k), hex::encode(v)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn test_rollback_all() {
    let n = 10;
    let plan = TestPlan::generate("all", n, false);
    let mut nomt = setup_nomt(
        "test_rollback_all",
        /* enable_rollback */ true,
        /* commit_concurrency */ 10,
        /* should_clean_up */ true,
    );

    plan.apply_forward(&mut nomt);
    nomt.rollback(n).unwrap();
    plan.verify_restored_state(&mut nomt, 0);
}

#[test]
fn test_rollback_one_by_one() {
    let n = 10;
    let plan = TestPlan::generate("one_by_one", n, false);
    let mut nomt = setup_nomt(
        "one_by_one",
        /* enable_rollback */ true,
        /* commit_concurrency */ 10,
        /* should_clean_up */ true,
    );

    plan.apply_forward(&mut nomt);

    for i in (0..n).rev() {
        nomt.rollback(1).unwrap();
        plan.verify_restored_state(&mut nomt, i);
    }
}

#[test]
fn test_rollback_one_by_one_with_overflow() {
    let n = 10;
    let plan = TestPlan::generate("one_by_one_overflow", n, true);
    let mut nomt = setup_nomt(
        "one_by_one_overflow",
        /* enable_rollback */ true,
        /* commit_concurrency */ 10,
        /* should_clean_up */ true,
    );

    plan.apply_forward(&mut nomt);

    for i in (0..n).rev() {
        nomt.rollback(1).unwrap();
        plan.verify_restored_state(&mut nomt, i);
    }
}

#[test]
fn test_rollback_multiple_equivalence() {
    let n = 10;
    let plan = TestPlan::generate("multiple_equivalence", n, false);
    let mut nomt = setup_nomt(
        "multiple_equivalence",
        /* rollback_enabled */ true,
        /* commit_concurrency */ 10,
        /* should_clean_up */ true,
    );
    plan.apply_forward(&mut nomt);

    // Rollback 2 commits at a time
    nomt.rollback(2).unwrap();
    plan.verify_restored_state(&mut nomt, 8);

    nomt.rollback(2).unwrap();
    plan.verify_restored_state(&mut nomt, 6);

    nomt.rollback(2).unwrap();
    plan.verify_restored_state(&mut nomt, 4);

    nomt.rollback(2).unwrap();
    plan.verify_restored_state(&mut nomt, 2);

    nomt.rollback(2).unwrap();
    plan.verify_restored_state(&mut nomt, 0);
}

#[test]
fn test_rollback_reopen() {
    // This test ensures that we can still rollback after restarting of NOMT.
    let n = 10;
    let plan = TestPlan::generate("rollback_reopen", n, false);
    let mut nomt = setup_nomt(
        "rollback_reopen",
        /* rollback_enabled */ true,
        /* commit_concurrency */ 10,
        /* should_clean_up */ true,
    );

    plan.apply_forward(&mut nomt);
    nomt.rollback(2).unwrap();
    plan.verify_restored_state(&mut nomt, 8);
    // Drop the NOMT to release the lock.
    drop(nomt);

    // Reopen the NOMT and rollback again.
    let mut nomt = setup_nomt(
        "rollback_reopen",
        /* rollback_enabled */ true,
        /* commit_concurrency */ 10,
        /* should_clean_up */ false, // <<<<<
    );
    nomt.rollback(2).unwrap();
    plan.verify_restored_state(&mut nomt, 6);
}

#[test]
fn test_rollback_change_history() {
    let n = 10;
    let plan = TestPlan::generate("rollback_change_history", n, false);
    let mut nomt = setup_nomt(
        "rollback_change_history",
        /* rollback_enabled */ true,
        /* commit_concurrency */ 10,
        /* should_clean_up */ true,
    );

    // 1. Do some commits
    plan.apply_forward(&mut nomt);
    plan.verify_restored_state(&mut nomt, n);

    // 2. Rollback some commits
    nomt.rollback(3).unwrap();
    plan.verify_restored_state(&mut nomt, 7);

    // 3. Create new commits
    let session = nomt.begin_session(SessionParams::default());
    let new_key = KeyPath::from([0xAA; 32]);
    let new_value = vec![0xBB; 32];
    let finished = session
        .finish(vec![(
            new_key,
            KeyReadWrite::Write(Some(new_value.clone())),
        )])
        .unwrap();
    finished.commit(&nomt).unwrap();

    // Verify the new state
    assert_eq!(nomt.read(new_key).unwrap(), Some(new_value));

    // 4. Rollback to the original history
    nomt.rollback(1).unwrap();
    plan.verify_restored_state(&mut nomt, 7);

    // Verify that the new key is gone
    assert_eq!(nomt.read(new_key).unwrap(), None);
}

#[test]
fn test_rollback_read_then_write() {
    // This test ensures that we correctly handle the case where the prior value is obtained from
    // the actual ReadThenWrite operations passed to Nomt::commit.

    let nomt = setup_nomt(
        "rollback_read_then_write",
        /* rollback_enabled */ true,
        /* commit_concurrency */ 10,
        /* should_clean_up */ true,
    );

    // Create a new key and write a value to it
    let session = nomt.begin_session(SessionParams::default());
    let key = KeyPath::from([0xAA; 32]);
    let original_value = vec![0xBB; 32];
    let finished = session
        .finish(vec![(
            key,
            KeyReadWrite::Write(Some(original_value.clone())),
        )])
        .unwrap();
    finished.commit(&nomt).unwrap();

    // Then, create a new commit with a read-then-write actual specifying the **wrong** prior value.
    //
    // The expected behavior is that the value from the ReadThenWrite operation takes precedence
    // over the original value.
    let session = nomt.begin_session(SessionParams::default());
    assert_eq!(session.read(key).unwrap(), Some(original_value.clone()));
    let new_value = vec![0xCC; 32];
    let finished = session
        .finish(vec![(
            key,
            KeyReadWrite::ReadThenWrite(None, Some(new_value.clone())),
        )])
        .unwrap();
    finished.commit(&nomt).unwrap();

    // Rollback and expect the value from the ReadThenWrite operation to be restored.
    nomt.rollback(1).unwrap();
    assert_eq!(nomt.read(key).unwrap(), None);
}
