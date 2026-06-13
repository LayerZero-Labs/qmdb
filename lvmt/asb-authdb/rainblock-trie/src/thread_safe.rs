use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex, MutexGuard, Weak},
};

use crate::{trie_node::TrieNode, trie_node_ext::TrieNodeExt};

pub struct Node(pub Mutex<TrieNodeExt>);

impl Clone for Node {
    fn clone(&self) -> Self {
        Self(Mutex::new(self.0.try_lock().unwrap().clone()))
    }
}

impl Node {
    pub fn as_ref(&self) -> impl Deref<Target = TrieNodeExt> + '_ {
        self.0.try_lock().unwrap()
    }

    pub fn as_mut(&mut self) -> impl DerefMut<Target = TrieNodeExt> + '_ {
        self.0.try_lock().unwrap()
    }

    pub fn as_mut_inner(&mut self) -> impl DerefMut<Target = TrieNode> + '_ {
        NodeGuard(self.0.try_lock().unwrap())
    }
}

#[derive(Clone)]
pub struct NodePtr(pub Arc<Node>);

impl NodePtr {
    pub fn ptr_eq(me: &Self, other: &Self) -> bool {
        Arc::ptr_eq(&me.0, &other.0)
    }

    pub fn downgrade(me: &Self) -> NodePtrWeak {
        NodePtrWeak(Arc::downgrade(&me.0))
    }

    pub fn make_mut(me: &mut Self) -> &mut Node {
        Arc::make_mut(&mut me.0)
    }

    pub fn as_ref(&self) -> impl Deref<Target = TrieNodeExt> + '_ {
        self.0.deref().0.try_lock().unwrap()
    }
}

pub struct NodePtrWeak(pub Weak<Node>);

impl NodePtrWeak {
    pub fn upgrade(&self) -> Option<NodePtr> {
        self.0.upgrade().map(|x| NodePtr(x))
    }
}

pub struct NodeGuard<'a>(MutexGuard<'a, TrieNodeExt>);

impl Deref for NodeGuard<'_> {
    type Target = TrieNode;

    fn deref(&self) -> &Self::Target {
        self.0.deref().deref()
    }
}

impl DerefMut for NodeGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.deref_mut().deref_mut()
    }
}
