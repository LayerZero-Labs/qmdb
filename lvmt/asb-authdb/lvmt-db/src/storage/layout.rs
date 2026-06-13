use crate::amt::NodeIndex;
use crate::crypto::TypeUInt;
use std::fmt::Debug;
use std::hash::Hash;

pub trait LayoutTrait<I: Copy + Clone + Debug + Eq + Hash> {
    fn position(index: &I) -> usize;
}

#[derive(Clone)]
pub struct FlattenArray;

impl LayoutTrait<usize> for FlattenArray {
    #[inline]
    fn position(index: &usize) -> usize {
        *index
    }
}

#[derive(Clone)]
pub struct FlattenTree;

impl<N: TypeUInt> LayoutTrait<NodeIndex<N>> for FlattenTree {
    #[inline]
    fn position(tree_index: &NodeIndex<N>) -> usize {
        (1 << tree_index.depth()) + tree_index.index()
    }
}
