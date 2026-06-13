use std::{
    ops::{Deref, DerefMut},
    rc::{Rc, Weak},
};

use crate::{trie_node::TrieNode, trie_node_ext::TrieNodeExt};

#[derive(Clone)]
pub struct Node(pub TrieNodeExt);

impl Node {
    pub fn as_ref(&self) -> impl Deref<Target = TrieNodeExt> + '_ {
        &self.0
    }

    pub fn as_mut(&mut self) -> impl DerefMut<Target = TrieNodeExt> + '_ {
        &mut self.0
    }

    pub fn as_mut_inner(&mut self) -> impl DerefMut<Target = TrieNode> + '_ {
        &mut *self.0
    }
}

#[derive(Clone)]
pub struct NodePtr(pub Rc<Node>);

impl NodePtr {
    pub fn ptr_eq(me: &Self, other: &Self) -> bool {
        Rc::ptr_eq(&me.0, &other.0)
    }

    pub fn downgrade(me: &Self) -> NodePtrWeak {
        NodePtrWeak(Rc::downgrade(&me.0))
    }

    pub fn make_mut(me: &mut Self) -> &mut Node {
        Rc::make_mut(&mut me.0)
    }

    pub fn as_ref(&self) -> impl Deref<Target = TrieNodeExt> + '_ {
        &self.0.deref().0
    }
}

pub struct NodePtrWeak(pub Weak<Node>);

impl NodePtrWeak {
    pub fn upgrade(&self) -> Option<NodePtr> {
        self.0.upgrade().map(|x| NodePtr(x))
    }
}
