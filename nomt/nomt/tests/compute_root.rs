mod common;

use common::Test;
use nomt::{hasher::Blake3Hasher, trie::NodeKind};

#[test]
fn root_on_empty_db() {
    let t = Test::new("compute_root_empty");
    let root = t.root();
    assert_eq!(
        NodeKind::of::<Blake3Hasher>(&root.into_inner()),
        NodeKind::Terminator
    );
}

#[test]
fn root_on_leaf() {
    {
        let mut t = Test::new("compute_root_leaf");
        t.write([1; 32], Some(vec![1, 2, 3]));
        t.commit();
    }

    let t = Test::new_with_params("compute_root_leaf", 1, 1, None, false);
    let root = t.root();
    assert_eq!(
        NodeKind::of::<Blake3Hasher>(&root.into_inner()),
        NodeKind::Leaf
    );
}

#[test]
fn root_on_internal() {
    {
        let mut t = Test::new("compute_root_internal");
        t.write([0; 32], Some(vec![1, 2, 3]));
        t.write([1; 32], Some(vec![1, 2, 3]));
        t.commit();
    }

    let t = Test::new_with_params("compute_root_internal", 1, 1, None, false);
    let root = t.root();
    assert_eq!(
        NodeKind::of::<Blake3Hasher>(&root.into_inner()),
        NodeKind::Internal
    );
}
