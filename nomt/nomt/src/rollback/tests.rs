use std::{collections::BTreeSet, fs::OpenOptions, sync::Arc};

use super::{
    reverse_delta_worker::AsyncPending, BTreeMap, KeyPath, KeyReadWrite, LoadValueAsync, Rollback,
};
use crossbeam::channel::{Receiver, Sender};
use hex_literal::hex;

const MAX_ROLLBACK_LOG_LEN: u32 = 100;

/// A mock implementation of `LoadValue` for testing. Describes the "current" state of the
/// database.
#[derive(Clone)]
struct MockStore {
    values: BTreeMap<KeyPath, Option<Vec<u8>>>,
    traps: BTreeSet<KeyPath>,
}

impl MockStore {
    fn new() -> Self {
        Self {
            values: BTreeMap::new(),
            traps: BTreeSet::new(),
        }
    }

    /// Insert a value into the store that will be served by `load_value`.
    fn insert(&mut self, key_path: KeyPath, value: Option<Vec<u8>>) {
        assert!(
            !self.traps.contains(&key_path),
            "the caller is trying to insert a value that was trapped by the test"
        );
        self.values.insert(key_path, value);
    }

    /// Mark a key path as being trapped. If `load_value` is called with a trapped key path, the
    /// test will fail.
    fn trap(&mut self, key_path: KeyPath) {
        assert!(
            !self.values.contains_key(&key_path),
            "the caller is trying to trap a value that was already inserted by the test"
        );
        self.traps.insert(key_path);
    }

    fn async_reader(&self) -> MockStoreAsync {
        let (completion_tx, completion_rx) = crossbeam::channel::unbounded();
        MockStoreAsync {
            inner: self.clone(),
            completion_tx,
            completion_rx,
        }
    }
}

struct MockStoreAsync {
    inner: MockStore,
    completion_tx: Sender<(u64, Option<Vec<u8>>)>,
    completion_rx: Receiver<(u64, Option<Vec<u8>>)>,
}

struct MockPending;

impl AsyncPending for MockPending {
    type Store = MockStoreAsync;
    type OverflowPageInfo = ();

    fn submit(&mut self, _: &Self::Store, _: u64) -> Option<Self::OverflowPageInfo> {
        None
    }

    fn try_complete(
        &mut self,
        completion: Option<Vec<u8>>,
        _: Option<()>,
    ) -> Option<Option<Vec<u8>>> {
        Some(completion)
    }
}

impl LoadValueAsync for MockStoreAsync {
    type Completion = Option<Vec<u8>>;
    type Pending = MockPending;
    type RawCompletion = (u64, Option<Vec<u8>>);

    fn start_load(
        &self,
        key_path: KeyPath,
        user_data: u64,
    ) -> Result<Option<Vec<u8>>, Self::Pending> {
        if self.inner.traps.contains(&key_path) {
            panic!("the caller requested a value that was trapped by the test");
        }

        let res = match self.inner.values.get(&key_path) {
            Some(value) => value.clone(),
            None => panic!("the caller requested a value that was not inserted by the test"),
        };

        self.completion_tx.send((user_data, res)).unwrap();

        Err(MockPending)
    }

    fn raw_completion_stream(&self) -> Receiver<Self::RawCompletion> {
        self.completion_rx.clone()
    }

    fn process_raw_completion(
        complete_io: (u64, Option<Vec<u8>>),
    ) -> (u64, anyhow::Result<Self::Completion>) {
        let (user_data, completion) = complete_io;
        (user_data, Ok(completion))
    }
}

#[test]
fn truncate_works() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_dir_path = temp_dir.path().join("db");
    std::fs::create_dir_all(&db_dir_path).unwrap();
    let db_dir_fd = OpenOptions::new()
        .read(true)
        .open(db_dir_path.clone())
        .unwrap();

    let mut store = MockStore::new();
    store.insert(
        hex!("0101010101010101010101010101010101010101010101010101010101010101"),
        Some(b"old_value1".to_vec()),
    );
    store.insert(
        hex!("0202020202020202020202020202020202020202020202020202020202020202"),
        Some(b"old_value2".to_vec()),
    );
    store.insert(
        hex!("0303030303030303030303030303030303030303030303030303030303030303"),
        Some(b"old_value3".to_vec()),
    );

    let rollback =
        Rollback::read(MAX_ROLLBACK_LOG_LEN, db_dir_path, Arc::new(db_dir_fd), 0, 0).unwrap();
    let builder = rollback.delta_builder_inner(store.async_reader());
    builder.tentative_preserve_prior([1; 32]);
    builder.tentative_preserve_prior([2; 32]);
    builder.tentative_preserve_prior([3; 32]);
    let delta = builder.finalize(&[
        (
            hex!("0101010101010101010101010101010101010101010101010101010101010101"),
            KeyReadWrite::Write(Some(b"new_value1".to_vec())),
        ),
        (
            hex!("0202020202020202020202020202020202020202020202020202020202020202"),
            KeyReadWrite::Write(Some(b"new_value2".to_vec())),
        ),
    ]);
    rollback.commit(delta).unwrap();

    // We want to see the old values for all the keys that have been changed during the commit.
    let traceback = rollback.truncate(1).unwrap().unwrap();
    assert_eq!(traceback.len(), 2);
    assert_eq!(
        traceback
            .get(&hex!(
                "0101010101010101010101010101010101010101010101010101010101010101"
            ))
            .unwrap()
            .clone(),
        Some(b"old_value1".to_vec())
    );
    assert_eq!(
        traceback
            .get(&hex!(
                "0202020202020202020202020202020202020202020202020202020202020202"
            ))
            .unwrap()
            .clone(),
        Some(b"old_value2".to_vec())
    );
}

#[test]
fn without_tentative_preserve_prior() {
    // A test where we don't call tentative_preserve_prior and expect that the commit will
    // fetch the values itself.
    let temp_dir = tempfile::tempdir().unwrap();
    let db_dir_path = temp_dir.path().join("db");
    std::fs::create_dir_all(&db_dir_path).unwrap();
    let db_dir_fd = OpenOptions::new()
        .read(true)
        .open(db_dir_path.clone())
        .unwrap();

    let mut store = MockStore::new();
    store.insert(
        hex!("0101010101010101010101010101010101010101010101010101010101010101"),
        Some(b"old_value1".to_vec()),
    );
    store.insert(
        hex!("0202020202020202020202020202020202020202020202020202020202020202"),
        Some(b"old_value2".to_vec()),
    );
    store.insert(
        hex!("0303030303030303030303030303030303030303030303030303030303030303"),
        Some(b"old_value3".to_vec()),
    );

    let rollback =
        Rollback::read(MAX_ROLLBACK_LOG_LEN, db_dir_path, Arc::new(db_dir_fd), 0, 0).unwrap();
    let builder = rollback.delta_builder_inner(store.async_reader());
    let delta = builder.finalize(&[
        (
            hex!("0101010101010101010101010101010101010101010101010101010101010101"),
            KeyReadWrite::Write(Some(b"new_value1".to_vec())),
        ),
        (
            hex!("0202020202020202020202020202020202020202020202020202020202020202"),
            KeyReadWrite::Write(Some(b"new_value2".to_vec())),
        ),
    ]);
    rollback.commit(delta).unwrap();

    // We want to see the old values for all the keys that have been changed during the commit.
    let traceback = rollback.truncate(1).unwrap().unwrap();
    assert_eq!(traceback.len(), 2);
    assert_eq!(
        traceback
            .get(&hex!(
                "0101010101010101010101010101010101010101010101010101010101010101"
            ))
            .unwrap()
            .clone(),
        Some(b"old_value1".to_vec())
    );
    assert_eq!(
        traceback
            .get(&hex!(
                "0202020202020202020202020202020202020202020202020202020202020202"
            ))
            .unwrap()
            .clone(),
        Some(b"old_value2".to_vec())
    );
}

#[test]
fn delta_builder_doesnt_load_read_then_write_priors() {
    // This test ensures that the delta builder does not attempt to load the prior value for
    // ReadThenWrite operations.

    let temp_dir = tempfile::tempdir().unwrap();
    let db_dir_path = temp_dir.path().join("db");
    std::fs::create_dir_all(&db_dir_path).unwrap();
    let db_dir_fd = OpenOptions::new()
        .read(true)
        .open(db_dir_path.clone())
        .unwrap();

    let key_1 = hex!("0101010101010101010101010101010101010101010101010101010101010101");

    let mut store = MockStore::new();
    store.trap(key_1);

    let rollback =
        Rollback::read(MAX_ROLLBACK_LOG_LEN, db_dir_path, Arc::new(db_dir_fd), 0, 0).unwrap();
    let builder = rollback.delta_builder_inner(store.async_reader());
    let delta = builder.finalize(&[(
        key_1,
        KeyReadWrite::ReadThenWrite(Some(b"prior_value".to_vec()), Some(b"new_value1".to_vec())),
    )]);

    rollback
        .commit(delta)
        // This will panic if the delta builder attempts to load from store the prior value for
        // key_1.
        .unwrap();

    // We expect that the traceback will contain the specified prior value for key_1.
    let traceback = rollback.truncate(1).unwrap().unwrap();
    assert_eq!(
        traceback.get(&key_1).unwrap(),
        &Some(b"prior_value".to_vec())
    );
}

#[test]
fn rollback_writeout_start_and_end() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_dir_path = temp_dir.path().join("db");
    std::fs::create_dir_all(&db_dir_path).unwrap();
    let db_dir_fd = OpenOptions::new()
        .read(true)
        .open(db_dir_path.clone())
        .unwrap();
    let store = MockStore::new();

    let rollback =
        Rollback::read(MAX_ROLLBACK_LOG_LEN, db_dir_path, Arc::new(db_dir_fd), 0, 0).unwrap();

    // fill the rollback with the max amount of deltas + 1
    for _ in 0..MAX_ROLLBACK_LOG_LEN + 1 {
        let builder = rollback.delta_builder_inner(store.async_reader());
        let delta = builder.finalize(&[]);
        rollback.commit(delta).unwrap();
    }

    // expected prune of oldest delta
    let wa = rollback.writeout_start();
    assert_eq!(wa.rollback_start_live, 1);
    assert_eq!(wa.rollback_end_live, 101);
    assert_eq!(wa.prune_to_new_start_live, Some(2));
    assert_eq!(wa.prune_to_new_end_live, None);

    rollback
        .writeout_end(wa.prune_to_new_start_live, wa.prune_to_new_end_live)
        .unwrap();
    let (rollback_start_live, rollback_end_live) = rollback.seglog().live_range();
    assert_eq!(rollback_start_live, 2.into());
    assert_eq!(rollback_end_live, 101.into());

    // expected prune of oldest delta
    let builder = rollback.delta_builder_inner(store.async_reader());
    let delta = builder.finalize(&[]);
    rollback.commit(delta).unwrap();

    let wa = rollback.writeout_start();
    assert_eq!(wa.rollback_start_live, 2);
    assert_eq!(wa.rollback_end_live, 102);
    assert_eq!(wa.prune_to_new_start_live, Some(3));
    assert_eq!(wa.prune_to_new_end_live, None);

    rollback
        .writeout_end(wa.prune_to_new_start_live, wa.prune_to_new_end_live)
        .unwrap();
    let (rollback_start_live, rollback_end_live) = rollback.seglog().live_range();
    assert_eq!(rollback_start_live, 3.into());
    assert_eq!(rollback_end_live, 102.into());

    rollback.truncate(5).unwrap();

    // expected prune of 5 newest deltas
    let wa = rollback.writeout_start();
    assert_eq!(wa.rollback_start_live, 3);
    assert_eq!(wa.rollback_end_live, 97);
    assert_eq!(wa.prune_to_new_start_live, None);
    assert_eq!(wa.prune_to_new_end_live, Some(97));

    rollback
        .writeout_end(Some(wa.rollback_start_live), Some(wa.rollback_end_live))
        .unwrap();
    let (rollback_start_live, rollback_end_live) = rollback.seglog().live_range();
    assert_eq!(rollback_start_live, 3.into());
    assert_eq!(rollback_end_live, 97.into());
}
