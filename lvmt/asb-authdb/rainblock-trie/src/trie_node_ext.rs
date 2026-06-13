use std::{
    cell::{Cell, RefCell},
    ops::{Deref, DerefMut},
    sync::Arc,
};

use ethereum_types::H256;
use hash_db::Hasher;
use kvdb::KeyValueDB;
use rlp::Encodable;

use crate::trie_node::TrieNode;
use crate::Node;
use crate::RlpHasher;
use crate::{child_ref::ChildRef, NodePtr, NodePtrWeak};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Commit {
    /// The node has been committed to db
    Committed,
    /// The node has not been committed to db, but we ensure it needs to be committed at some time.
    Pending,
    /// The node has a short rlp encoding (< 32 bytes), not need to be committed
    Light,
    /// The node has been changed, we haven't compute its rlp encoding.
    Working,
}
use Commit::*;

#[derive(Clone)]
pub struct TrieNodeExt {
    node: TrieNode,
    hash: Cell<Option<H256>>,
    rlp_encode: RefCell<Option<Vec<u8>>>,
    commited: Cell<Commit>,
}

impl TrieNodeExt {
    pub fn new(node: TrieNode) -> Self {
        Self {
            node,
            hash: Cell::new(None),
            rlp_encode: RefCell::new(None),
            commited: Cell::new(Working),
        }
    }

    pub fn from_child_ref(node: TrieNode, rlp_encode: Vec<u8>) -> Self {
        // I have not find a case with a long rlp_encode, since this function will only be called in loading nodes.
        assert!(rlp_encode.len() < 32);
        Self {
            node,
            hash: Cell::new(None),
            rlp_encode: RefCell::new(Some(rlp_encode)),
            commited: Cell::new(Light),
        }
    }

    pub fn load(db: &Arc<dyn KeyValueDB>, digest: H256) -> Self {
        let loaded = match db.get(0, &digest.0) {
            Ok(Some(loaded)) => loaded,
            Ok(None) => panic!("Hash {:?} not found", &digest.0),
            Err(e) => panic!("Hash {:?} meet error {}", &digest.0, e),
        };

        let node = TrieNode::new(loaded.clone());
        TrieNodeExt {
            node,
            hash: Cell::new(Some(digest)),
            rlp_encode: RefCell::new(Some(loaded)),
            commited: Cell::new(Committed),
        }
    }

    #[cfg(feature = "thread-safe")]
    #[inline]
    pub fn seal(self) -> NodePtr {
        use std::sync::Mutex;
        NodePtr(Arc::new(Node(Mutex::new(self))))
    }

    #[cfg(not(feature = "thread-safe"))]
    #[inline]
    pub fn seal(self) -> NodePtr {
        use std::rc::Rc;

        NodePtr(Rc::new(Node(self)))
    }

    pub fn make_mut<'a>(
        me: &'a mut NodePtr,
        del_ops: &mut Vec<H256>,
    ) -> impl DerefMut<Target = TrieNode> + 'a {
        let cached_node = NodePtr::make_mut(me);
        if cached_node.as_ref().commited.get() == Committed {
            if let Some(digest) = cached_node.as_ref().hash.get() {
                del_ops.push(digest);
            }
        }
        cached_node.as_mut().clear_cache();
        cached_node.as_mut_inner()
    }

    pub fn clear_cache(&mut self) {
        *self.hash.get_mut() = None;
        *self.rlp_encode.get_mut() = None;
        *self.commited.get_mut() = Working;
    }

    #[inline]
    pub fn hash(&self) -> H256 {
        if self.hash.get().is_none() {
            let hash = RlpHasher::hash(&self.get_rlp_encode());
            self.hash.set(Some(hash));
        }
        self.hash.get().unwrap()
    }

    #[inline]
    pub fn is_small_node(&self) -> bool {
        self.get_rlp_encode().len() < 32
    }

    #[inline]
    pub fn get_rlp_encode(&self) -> Vec<u8> {
        if self.rlp_encode.borrow().is_none() {
            let rlp_encode: Vec<u8> = self.node.rlp_bytes();
            self.commited.set(if rlp_encode.len() < 32 {
                Light
            } else {
                Pending
            });
            *self.rlp_encode.borrow_mut() = Some(rlp_encode);
        }
        self.rlp_encode.borrow().as_ref().unwrap().clone()
    }

    pub fn load_children<const N: usize>(&self, db: &Arc<dyn KeyValueDB>, depth: usize) {
        if depth >= N - 1 {
            return;
        }
        assert!(self.commited.get() != Working);
        match &self.node {
            TrieNode::Branch { children, .. } => {
                for child in children.iter() {
                    if let Some((node, _)) = ChildRef::owned_or_load(child, db) {
                        node.as_ref().load_children::<N>(db, depth + 1);
                    }
                }
            }
            TrieNode::Extension { child, .. } => {
                if let Some((node, _)) = ChildRef::owned_or_load(child, db) {
                    node.as_ref().load_children::<N>(db, depth + 1);
                }
            }
            _ => {}
        }
    }

    pub fn commit<const N: usize>(
        &self,
        depth: usize,
        put_ops: &mut Vec<(H256, Vec<u8>)>,
        top_layer: bool,
    ) {
        let bottom_layer = !top_layer;
        if depth >= N && top_layer {
            return;
        }

        let commit = self.commited.get();

        if (commit == Committed || commit == Light) && bottom_layer {
            return;
        }

        match &self.node {
            TrieNode::Branch { children, .. } => {
                for child in children.iter() {
                    ChildRef::commit::<N>(child, depth + 1, put_ops, top_layer);
                }
            }
            TrieNode::Extension { child, .. } => {
                ChildRef::commit::<N>(child, depth + 1, put_ops, top_layer);
            }
            _ => {}
        }

        let rlp_encode = self.get_rlp_encode();
        if rlp_encode.len() >= 32 || depth == 0 {
            let hash = self.hash();
            if depth >= N || top_layer {
                put_ops.push((hash, rlp_encode));
            }
            self.commited.set(Committed);
        } else {
            assert_eq!(self.commited.get(), Light);
        }

        if depth == N - 1 && bottom_layer {
            self.truncate();
        }
    }

    pub fn truncate(&self) {
        match &self.node {
            TrieNode::Branch { children, .. } => {
                for child in children.iter() {
                    ChildRef::truncate(child);
                }
            }
            TrieNode::Extension { child, .. } => {
                ChildRef::truncate(child);
            }
            _ => {}
        }
    }

    pub fn exile<const N: usize>(me: &NodePtr, depth: usize, exile_nodes: &mut Vec<NodePtrWeak>) {
        if depth > N {
            return;
        }

        if depth == N {
            exile_nodes.push(NodePtr::downgrade(me));
            return;
        }
        match &**me.as_ref() {
            TrieNode::Branch { children, .. } => {
                for child in children.iter() {
                    child.borrow().exile::<N>(depth + 1, exile_nodes);
                }
            }
            TrieNode::Extension { child, .. } => {
                child.borrow().exile::<N>(depth + 1, exile_nodes);
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub fn loaded_nodes_count(&self) -> usize {
        1 + match &self.node {
            TrieNode::Branch { children, .. } => {
                children.iter().map(ChildRef::loaded_nodes_count).sum()
            }
            TrieNode::Extension { child, .. } => ChildRef::loaded_nodes_count(child),
            TrieNode::Leaf { .. } => 0,
        }
    }

    #[cfg(test)]
    pub fn print_node(&self, ident: usize) {
        let prefix = String::from_utf8(vec![b' '; (ident + 1) * 4]).unwrap();
        match &self.node {
            TrieNode::Branch { children, value } => {
                println!("Branch");
                for (idx, child) in children.iter().enumerate() {
                    if !child.borrow().is_null() {
                        print!("{prefix}Child {idx} -> ");
                        ChildRef::print_node(child, ident + 1);
                    }
                }
                println!("{prefix}Value -> {:x?}", value);
            }
            TrieNode::Extension { key, child, .. } => {
                println!("Extension");
                println!("{prefix}Key -> {:x?}", key);
                print!("{prefix}Child -> ");
                ChildRef::print_node(child, ident + 1);
            }
            TrieNode::Leaf { key, value } => {
                println!("Leaf");
                println!("{prefix}Key -> {:x?}", key);
                println!("{prefix}Value -> {:x?}", value);
            }
        }
    }
}

impl Deref for TrieNodeExt {
    type Target = TrieNode;

    fn deref(&self) -> &Self::Target {
        &self.node
    }
}

impl DerefMut for TrieNodeExt {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.node
    }
}
