use nomt::{hasher::Blake3Hasher, KeyReadWrite, Nomt, Options, SessionParams};
use std::path::PathBuf;

/// Setup a NOMT with the given path, rollback enabled, and the given commit concurrency.
///
/// It's important that tests that run in parallel don't use the same path.
fn setup_nomt(path: &str) -> Nomt<Blake3Hasher> {
    let path = {
        let mut p = PathBuf::from("test");
        p.push(path);
        p
    };
    if path.exists() {
        std::fs::remove_dir_all(&path).unwrap();
    }
    let mut o = Options::new();
    o.path(path);
    o.commit_concurrency(1);
    Nomt::open(o).unwrap()
}

#[test]
fn test_prev_root_commits() {
    let nomt = setup_nomt("prev_root_commits");
    let session1 = nomt.begin_session(SessionParams::default());
    let finished1 = session1
        .finish(vec![([1; 32], KeyReadWrite::Write(Some(vec![1, 2, 3])))])
        .unwrap();

    let session2 = nomt.begin_session(SessionParams::default());
    let finished2 = session2
        .finish(vec![([1; 32], KeyReadWrite::Write(Some(vec![1, 2, 3])))])
        .unwrap();

    finished1.commit(&nomt).unwrap();

    finished2.commit(&nomt).unwrap_err();
}

#[test]
fn test_prev_root_overlay_invalidated() {
    let nomt = setup_nomt("prev_root_overlay_invalidated");
    let session1 = nomt.begin_session(SessionParams::default());
    let finished1 = session1
        .finish(vec![([1; 32], KeyReadWrite::Write(Some(vec![1, 2, 3])))])
        .unwrap();
    let overlay1 = finished1.into_overlay();

    let session2 = nomt.begin_session(SessionParams::default());
    let finished2 = session2
        .finish(vec![([1; 32], KeyReadWrite::Write(Some(vec![1, 2, 3])))])
        .unwrap();

    finished2.commit(&nomt).unwrap();

    overlay1.commit(&nomt).unwrap_err();
}

#[test]
fn test_prev_root_overlay_invalidates_session() {
    let nomt = setup_nomt("prev_root_overlays");
    let session1 = nomt.begin_session(SessionParams::default());
    let finished1 = session1
        .finish(vec![([1; 32], KeyReadWrite::Write(Some(vec![1, 2, 3])))])
        .unwrap();
    let overlay1 = finished1.into_overlay();

    let session2 = nomt.begin_session(SessionParams::default());
    let finished2 = session2
        .finish(vec![([1; 32], KeyReadWrite::Write(Some(vec![1, 2, 3])))])
        .unwrap();

    overlay1.commit(&nomt).unwrap();

    finished2.commit(&nomt).unwrap_err();
}
