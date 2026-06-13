use std::{
    cell::RefCell,
    ops::{Deref, DerefMut, Index, IndexMut},
    sync::Arc,
};

use crate::{
    nibble::Nibble, trie_node::TrieNode, trie_node_ext::TrieNodeExt, NodePtr, NodePtrWeak,
};
use ethereum_types::H256;
use kvdb::KeyValueDB;
use rlp::{Decodable, Encodable};

type Bytes = Vec<u8>;

#[derive(Clone, Default)]
pub enum ChildRef {
    #[default]
    Null,
    Ref(H256),
    Owned(NodePtr),
}

impl PartialEq for ChildRef {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Ref(l0), Self::Ref(r0)) => l0 == r0,
            (Self::Owned(l0), Self::Owned(r0)) => NodePtr::ptr_eq(l0, r0),
            _ => false,
        }
    }
}

impl Eq for ChildRef {}

pub type ChildRefCell = RefCell<ChildRef>;

impl ChildRef {
    pub fn is_null(&self) -> bool {
        *self == ChildRef::Null
    }

    pub fn owned_mut(&mut self) -> Option<&mut NodePtr> {
        if let ChildRef::Owned(node) = self {
            Some(node)
        } else {
            None
        }
    }

    pub fn loaded_mut(&mut self, db: &Arc<dyn KeyValueDB>) -> Option<&mut NodePtr> {
        if let ChildRef::Ref(digest) = self {
            let node = TrieNodeExt::load(db, digest.clone()).seal();
            *self = ChildRef::Owned(node);
        }
        match self {
            ChildRef::Null => None,
            ChildRef::Ref(_) => unreachable!(),
            ChildRef::Owned(node) => Some(node),
        }
    }

    pub fn owned_or_load(
        me: &RefCell<ChildRef>,
        db: &Arc<dyn KeyValueDB>,
    ) -> Option<(NodePtr, bool)> {
        let borrowed_ref = me.borrow();
        match &*borrowed_ref {
            ChildRef::Null => None,
            ChildRef::Ref(digest) => {
                let node = TrieNodeExt::load(db, digest.clone()).seal();
                std::mem::drop(borrowed_ref);

                me.replace(ChildRef::Owned(node.clone()));
                Some((node, true))
            }
            ChildRef::Owned(node) => Some((node.clone(), false)),
        }
    }

    #[inline]
    pub fn truncate(me: &RefCell<ChildRef>) {
        let mut replaced_hash = None;
        if let Self::Owned(node) = &*me.borrow() {
            if node.as_ref().is_small_node() {
                return;
            }
            replaced_hash = Some(node.as_ref().hash());
        }
        if let Some(hash) = replaced_hash {
            *me.borrow_mut() = Self::Ref(hash)
        }
    }

    #[inline]
    pub fn exile<const N: usize>(&self, depth: usize, exile_nodes: &mut Vec<NodePtrWeak>) {
        if let Self::Owned(node) = self {
            TrieNodeExt::exile::<N>(node, depth, exile_nodes);
        }
    }

    #[inline]
    pub fn commit<const N: usize>(
        me: &RefCell<Self>,
        depth: usize,
        put_ops: &mut Vec<(H256, Vec<u8>)>,
        top_layer: bool,
    ) {
        if let Self::Owned(node) = &*me.borrow() {
            node.as_ref().commit::<N>(depth, put_ops, top_layer)
        }
    }

    #[inline]
    #[cfg(test)]
    pub fn loaded_nodes_count(me: &RefCell<Self>) -> usize {
        if let Self::Owned(node) = &*me.borrow() {
            if node.as_ref().get_rlp_encode().len() >= 32 {
                node.as_ref().loaded_nodes_count()
            } else {
                0
            }
        } else {
            0
        }
    }

    #[inline]
    #[cfg(test)]
    pub fn print_node(me: &RefCell<Self>, ident: usize) {
        match &*me.borrow() {
            ChildRef::Null => {
                println!("null")
            }
            ChildRef::Ref(digest) => {
                println!("digest {digest:x?}")
            }
            ChildRef::Owned(node) => node.as_ref().print_node(ident),
        }
    }
}

impl Encodable for ChildRef {
    fn rlp_append(&self, s: &mut rlp::RlpStream) {
        match self {
            ChildRef::Null => Bytes::new().rlp_append(s),
            ChildRef::Ref(digest) => digest.rlp_append(s),
            ChildRef::Owned(node) => {
                if node.as_ref().is_small_node() {
                    let rlp_encoded = node.as_ref().get_rlp_encode();
                    s.append_raw(&rlp_encoded, 0);
                } else {
                    node.as_ref().hash().rlp_append(s)
                }
            }
        }
    }
}
impl Decodable for ChildRef {
    fn decode(rlp: &rlp::Rlp) -> Result<Self, rlp::DecoderError> {
        Ok(if rlp.is_empty() {
            ChildRef::Null
        } else if rlp.is_list() {
            let rlp_encode = rlp.as_raw().to_vec();
            let trie_node = TrieNode::decode(rlp)?;
            ChildRef::Owned(TrieNodeExt::from_child_ref(trie_node, rlp_encode).seal())
        } else {
            ChildRef::Ref(H256::decode(rlp)?)
        })
    }
}

#[derive(Default, Clone, Eq, PartialEq)]
pub struct ChildRefGroup([ChildRefCell; 16]);

impl ChildRefGroup {
    pub fn enumerate_mut(&mut self) -> impl Iterator<Item = (Nibble, &mut ChildRef)> {
        self.0
            .iter_mut()
            .enumerate()
            .map(|(idx, child_ref)| (Nibble::from_lo(idx as u8), child_ref.get_mut()))
    }

    pub fn no_child(&mut self) -> bool {
        self.enumerate_mut().all(|(_, child)| child.is_null())
    }

    pub fn child_cnt(&self) -> usize {
        self.0.iter().filter(|cell| !cell.borrow().is_null()).count()
    }

    pub fn only_child_mut(&mut self) -> Option<(Nibble, &mut ChildRef)> {
        let mut non_null_child = None;
        for (idx, _) in self.enumerate_mut().filter(|(_, child)| !child.is_null()) {
            if non_null_child.is_some() {
                non_null_child = None;
                break;
            } else {
                non_null_child = Some(idx)
            }
        }
        non_null_child.map(|idx| (idx, self[idx].get_mut()))
    }
}

impl Deref for ChildRefGroup {
    type Target = [ChildRefCell; 16];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ChildRefGroup {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Index<Nibble> for ChildRefGroup {
    type Output = ChildRefCell;

    fn index(&self, index: Nibble) -> &Self::Output {
        // SAFETY: a nibble must belongs to [0, 16)
        unsafe { self.0.get_unchecked(index.inner() as usize) }
    }
}

impl IndexMut<Nibble> for ChildRefGroup {
    fn index_mut(&mut self, index: Nibble) -> &mut Self::Output {
        // SAFETY: a nibble must belongs to [0, 16)
        unsafe { self.0.get_unchecked_mut(index.inner() as usize) }
    }
}
