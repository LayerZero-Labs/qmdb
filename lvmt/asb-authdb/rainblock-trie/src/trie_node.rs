use std::{cell::RefCell, sync::Arc};

use crate::{
    child_ref::{ChildRef, ChildRefCell, ChildRefGroup},
    nibble::{from_mpt_key, to_mpt_key, Nibble},
    trie_node_ext::TrieNodeExt,
    Node, NodePtr, NodePtrWeak,
};
use kvdb::KeyValueDB;
use rlp::{Decodable, Encodable, Rlp};

type Bytes = Vec<u8>;

#[derive(Clone, Eq, PartialEq)]
pub enum TrieNode {
    Leaf {
        key: Vec<Nibble>,
        value: Bytes,
    },
    Branch {
        children: ChildRefGroup,
        value: Bytes,
    },
    Extension {
        key: Vec<Nibble>,
        child: ChildRefCell,
    },
}

pub enum NextResult {
    Matched,
    NotMatched,
    Next((NodePtr, Nibble)),
}

use TrieNode::*;

impl Default for TrieNode {
    fn default() -> Self {
        Self::Leaf {
            key: vec![],
            value: vec![],
        }
    }
}

impl TrieNode {
    pub fn new(data: Vec<u8>) -> Self {
        let rlp = Rlp::new(&data);
        TrieNode::decode(&rlp).unwrap()
    }

    pub fn new_leaf(key_remainder: Vec<Nibble>, value: Vec<u8>) -> Self {
        Self::Leaf {
            key: key_remainder.into(),
            value,
        }
    }

    pub fn new_branch() -> Self {
        Self::Branch {
            children: Default::default(),
            value: Default::default(),
        }
    }

    pub fn new_extention(new_key: Vec<Nibble>, child: ChildRef) -> Self {
        Self::Extension {
            key: new_key.into(),
            child: RefCell::new(child),
        }
    }

    pub fn key(&self) -> Option<&[Nibble]> {
        match self {
            Leaf { key, .. } | Extension { key, .. } => Some(key),
            Branch { .. } => None,
        }
    }

    pub fn key_mut(&mut self) -> Option<&mut Vec<Nibble>> {
        match self {
            Leaf { key, .. } | Extension { key, .. } => Some(key),
            Branch { .. } => None,
        }
    }

    pub fn child_mut(&mut self, index: Nibble) -> Option<&mut ChildRef> {
        match self {
            Branch { children, .. } => Some(children[index].get_mut()),
            Extension { child, .. } if index.is_zero() => Some(child.get_mut()),
            _ => None,
        }
    }

    pub fn value(&self) -> Option<&Bytes> {
        match self {
            Leaf { value, .. } | Branch { value, .. } => Some(value),
            Extension { .. } => None,
        }
    }

    pub fn value_mut(&mut self) -> Option<&mut Bytes> {
        match self {
            Leaf { value, .. } | Branch { value, .. } => Some(value),
            Extension { .. } => None,
        }
    }

    pub fn as_branch_mut(&mut self) -> Option<(&mut ChildRefGroup, &mut Vec<u8>)> {
        match self {
            Branch { children, value } => Some((children, value)),
            _ => None,
        }
    }

    pub fn as_extension_mut(&mut self) -> Option<(&mut Vec<Nibble>, &mut ChildRef)> {
        match self {
            Extension { key, child } => Some((key, child.get_mut())),
            _ => None,
        }
    }

    pub fn as_leaf_mut(&mut self) -> Option<(&mut Vec<Nibble>, &mut Vec<u8>)> {
        match self {
            Leaf { key, value } => Some((key, value)),
            _ => None,
        }
    }

    pub fn next<const N: usize>(
        me: &NodePtr,
        nibbles: &mut Vec<Nibble>,
        db: &Arc<dyn KeyValueDB>,
        truncate_ops: &mut Vec<NodePtrWeak>,
        depth: usize,
    ) -> NextResult {
        match &**me.as_ref() {
            Branch { children, .. } => {
                if nibbles.len() == 0 {
                    return NextResult::Matched;
                }

                let branch_key = *nibbles.first().unwrap();
                let branch = &children[branch_key];
                if let Some((next, is_loaded)) = ChildRef::owned_or_load(branch, db) {
                    if is_loaded && depth >= N {
                        truncate_ops.push(NodePtr::downgrade(&me));
                    }
                    *nibbles = nibbles[1..].to_vec();
                    NextResult::Next((next, branch_key))
                } else {
                    NextResult::NotMatched
                }
            }
            Extension { key, child } => {
                if nibbles[..].starts_with(&key) {
                    *nibbles = nibbles[key.len()..].to_vec();
                    let (node, is_loaded) = ChildRef::owned_or_load(child, db).unwrap();
                    if is_loaded && depth >= N {
                        truncate_ops.push(NodePtr::downgrade(&me));
                    }
                    NextResult::Next((node, Nibble::zero()))
                } else {
                    NextResult::NotMatched
                }
            }
            Leaf { key, .. } => {
                if key[..] == nibbles[..] {
                    *nibbles = vec![];
                    NextResult::Matched
                } else {
                    NextResult::NotMatched
                }
            }
        }
    }

    #[cfg(feature = "thread-safe")]
    #[inline]
    pub fn seal(self) -> NodePtr {
        use std::sync::Mutex;

        NodePtr(Arc::new(Node(Mutex::new(TrieNodeExt::new(self)))))
    }

    #[cfg(not(feature = "thread-safe"))]
    #[inline]
    pub fn seal(self) -> NodePtr {
        use std::rc::Rc;

        NodePtr(Rc::new(Node(TrieNodeExt::new(self))))
    }

    #[inline]
    pub fn seal_ref(self) -> ChildRef {
        ChildRef::Owned(self.seal())
    }
}

impl Encodable for TrieNode {
    fn rlp_append(&self, s: &mut rlp::RlpStream) {
        match self {
            Leaf { key, value } => {
                s.begin_list(2);
                s.append(&to_mpt_key(&key[..], true));
                s.append(value);
            }
            Branch { children, value } => {
                s.begin_list(17);
                for child in children.iter() {
                    s.append(&*child.borrow());
                }
                s.append(value);
            }
            Extension { key, child } => {
                s.begin_list(2);
                s.append(&to_mpt_key(&key[..], false));
                s.append(&*child.borrow());
            }
        }
    }
}

impl Decodable for TrieNode {
    fn decode(rlp: &rlp::Rlp) -> Result<Self, rlp::DecoderError> {
        Ok(match rlp.item_count()? {
            2 => {
                let (key, leaf) = from_mpt_key(rlp.val_at(0)?);
                if leaf {
                    Leaf {
                        key,
                        value: rlp.val_at(1)?,
                    }
                } else {
                    Extension {
                        key,
                        child: RefCell::new(rlp.val_at(1)?),
                    }
                }
            }
            17 => {
                let mut children: ChildRefGroup = Default::default();
                for i in Nibble::all() {
                    children[i] = RefCell::new(rlp.val_at(i.inner() as usize)?);
                }
                let value = rlp.val_at(16)?;
                Branch { children, value }
            }
            _ => return Err(rlp::DecoderError::RlpInvalidLength),
        })
    }
}
