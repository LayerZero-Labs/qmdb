//! Tests the directory lock behavior.

use std::path::PathBuf;

use nomt::{hasher::Blake3Hasher, Nomt, Options};

fn setup_nomt(path: &str, should_clean_up: bool) -> anyhow::Result<Nomt<Blake3Hasher>> {
    let path = {
        let mut p = PathBuf::from("test");
        p.push(path);
        p
    };
    if should_clean_up && path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    let mut o = Options::new();
    o.path(path);
    o.bitbox_seed([0; 16]);
    Nomt::open(o)
}

#[test]
fn smoke() {
    let _nomt = setup_nomt("smoke", true).unwrap();
}

#[test]
fn dir_lock() {
    let _nomt_1 = setup_nomt("dir_lock", true).unwrap();
    let nomt_2 = setup_nomt("dir_lock", false);
    assert!(matches!(nomt_2, Err(e) if e.to_string().contains("Resource temporarily unavailable")));
}

#[test]
fn dir_unlock() {
    let nomt_1 = setup_nomt("dir_unlock", true).unwrap();
    drop(nomt_1);
    let _nomt_2 = setup_nomt("dir_unlock", false).unwrap();
}
