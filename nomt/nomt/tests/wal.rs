mod common;

use common::Test;
use nomt::PanicOnSyncMode;

#[test]
fn wal_recovery_test_post_meta_swap() {
    // Initialize the db with panic on sync equals true.
    let mut t = Test::new_with_params(
        "wal_add_remove_1000",
        1,                               // commit_concurrency,
        1000000,                         // hashtable_buckets,
        Some(PanicOnSyncMode::PostMeta), // panic_on_sync
        true,                            // clean
    );

    common::set_balance(&mut t, 0, 1000);
    common::set_balance(&mut t, 1, 2000);
    common::set_balance(&mut t, 2, 3000);

    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.commit();
    }));
    assert!(r.is_err());
    drop(t);

    // Re-open the db without cleaning the DB dir and without panic on sync.
    let mut t = Test::new_with_params(
        "wal_add_remove_1000",
        1,       // commit_concurrency,
        1000000, // hashtable_buckets,
        None,    // panic_on_sync
        false,   // clean
    );
    assert_eq!(common::read_balance(&mut t, 0), Some(1000));
    assert_eq!(common::read_balance(&mut t, 1), Some(2000));
    assert_eq!(common::read_balance(&mut t, 2), Some(3000));
}

#[test]
fn wal_recovery_test_pre_meta_swap() {
    // Initialize the db with panic on sync equals true.
    let mut t = Test::new_with_params(
        "wal_pre_meta_swap",
        1,                              // commit_concurrency,
        1000000,                        // hashtable_buckets,
        Some(PanicOnSyncMode::PostWal), // panic_on_sync
        true,                           // clean
    );

    for i in 0..1000 {
        common::set_balance(&mut t, i, 1000);
    }

    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.commit();
    }));
    assert!(r.is_err());
    drop(t);

    // Re-open the db without cleaning the DB dir and without panic on sync.
    let mut t = Test::new_with_params(
        "wal_pre_meta_swap",
        1,       // commit_concurrency,
        1000000, // hashtable_buckets,
        None,    // panic_on_sync
        false,   // clean
    );

    // DB should open cleanly and not have any incomplete changes; the WAL is too new and will be
    // discarded.
    for i in 0..1000 {
        assert_eq!(common::read_balance(&mut t, i), None);
    }
}
