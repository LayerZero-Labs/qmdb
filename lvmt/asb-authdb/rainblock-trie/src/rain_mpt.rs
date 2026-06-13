use std::{io, sync::Arc};

use ethereum_types::H256;
use kvdb::{DBOp, DBTransaction, KeyValueDB};

use crate::{
    add_prefix,
    child_ref::{ChildRef, ChildRefGroup},
    common_prefix_iter,
    nibble::{bytes_to_nibble_list, Nibble},
    trie_node::{NextResult, TrieNode},
    trie_node_ext::TrieNodeExt,
    NodePtrWeak,
};

use crate::NodePtr;

pub struct MerklePatriciaTree<const TOP_LAYER_DEPTH: usize> {
    pub db: Arc<dyn KeyValueDB>,
    root: Option<NodePtr>,
    del_ops: Vec<H256>,
    loaded_node: Vec<NodePtrWeak>,
    exile_nodes: Vec<NodePtrWeak>,
}

struct SearchResult {
    stack: Vec<(NodePtr, Option<Nibble>)>,
    matched: bool,
    remainder: Vec<Nibble>,
}

impl SearchResult {
    fn matched(&self) -> Option<&NodePtr> {
        if self.matched {
            Some(&self.stack.last().unwrap().0)
        } else {
            None
        }
    }

    fn matched_mut(&mut self) -> Option<&mut NodePtr> {
        if self.matched {
            Some(&mut self.stack.last_mut().unwrap().0)
        } else {
            None
        }
    }
}

const ROOT_KEY: [u8; 1] = [0x80];
pub const EMPTY_ROOT: H256 = H256::zero();

impl<const N: usize> MerklePatriciaTree<N> {
    pub fn new(db: Arc<dyn KeyValueDB>) -> MerklePatriciaTree<N> {
        let root = if let Some(digest) = db.get(0, &ROOT_KEY).expect("Cannot load db") {
            let digest = H256::from_slice(&digest);
            let node = TrieNodeExt::load(&db, digest).seal();
            node.as_ref().load_children::<N>(&db, 0);
            Some(node)
        } else {
            None
        };

        MerklePatriciaTree {
            root,
            db,
            del_ops: vec![],
            loaded_node: vec![],
            exile_nodes: vec![],
        }
    }

    pub fn root(&self) -> Option<H256> {
        self.root.as_ref().map(|x| x.as_ref().hash())
    }

    pub fn get(&mut self, key: Vec<u8>) -> Option<Vec<u8>> {
        if key.is_empty() {
            panic!("Empty key is not supported")
        }
        if self.root.is_none() {
            return None;
        }
        let mut result = self.search(key);
        if let Some(matched_node) = result.matched_mut() {
            if let Some(value) = matched_node.as_ref().value() {
                return Some(value.clone());
            }
        }
        return None;
    }

    pub fn put(&mut self, key: Vec<u8>, val: Vec<u8>) {
        if key.is_empty() {
            panic!("Empty key is not supported")
        }
        if val.is_empty() {
            return self.del(key);
        }
        if self.root.is_none() {
            let trie_node = TrieNode::new_leaf(bytes_to_nibble_list(key), val);
            self.root = Some(trie_node.seal());
            return;
        }
        let mut result = self.search(key);
        if let Some(matched_node) = result.matched() {
            if matched_node.as_ref().value().unwrap() == &val {
                // Value not changed
                return;
            }
        }

        self.reset_pointers(&mut result.stack);
        if let Some(matched_node) = result.matched_mut() {
            let trie_node = &mut *TrieNodeExt::make_mut(matched_node, &mut self.del_ops);
            *trie_node.value_mut().unwrap() = val;
        } else {
            let last_depth = result.stack.len() - 1;
            let last_node = &mut *TrieNodeExt::make_mut(
                &mut result.stack.last_mut().unwrap().0,
                &mut self.del_ops,
            );
            self.insert(last_node, result.remainder, val, last_depth);
        }

        self.recover_pointers(result.stack);
    }

    pub fn commit(&mut self) -> io::Result<H256> {
        let mut put_ops = Vec::new();

        for node in self.exile_nodes.drain(..) {
            if let Some(node) = node.upgrade() {
                let hash = node.as_ref().hash();
                let rlp = node.as_ref().get_rlp_encode();
                put_ops.push((hash, rlp))
            }
        }

        if let Some(root) = &self.root {
            root.as_ref().commit::<N>(0, &mut put_ops, false);
        } else {
            self.db.write_buffered(DBTransaction {
                ops: vec![DBOp::Delete {
                    col: 0,
                    key: ROOT_KEY.to_vec().into(),
                }],
            })
        }

        for node in self.loaded_node.drain(..) {
            if let Some(node) = node.upgrade() {
                node.as_ref().truncate();
            }
        }

        for del_op in self.del_ops.drain(..) {
            self.db.write_buffered(DBTransaction {
                ops: vec![DBOp::Delete {
                    col: 0,
                    key: del_op.0.into(),
                }],
            })
        }
        for (key, value) in put_ops.drain(..) {
            self.db.write_buffered(DBTransaction {
                ops: vec![DBOp::Insert {
                    col: 0,
                    key: key.0.into(),
                    value,
                }],
            })
        }

        let hash = if let Some(root) = &self.root {
            let hash = root.as_ref().hash();
            self.db.write_buffered(DBTransaction {
                ops: vec![DBOp::Insert {
                    col: 0,
                    key: ROOT_KEY.to_vec().into(),
                    value: hash.0.to_vec(),
                }],
            });
            hash
        } else {
            EMPTY_ROOT
        };

        self.db.flush()?;

        Ok(hash)
    }

    pub fn flush_all(&mut self) -> io::Result<()> {
        let mut put_ops = Vec::new();
        if let Some(root) = &self.root {
            root.as_ref().commit::<N>(0, &mut put_ops, true);
        }

        for (key, value) in put_ops.drain(..) {
            self.db.write_buffered(DBTransaction {
                ops: vec![DBOp::Insert {
                    col: 0,
                    key: key.0.into(),
                    value,
                }],
            })
        }
        self.db.flush()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn loaded_nodes_count(&self) -> usize {
        if let Some(root) = &self.root {
            root.as_ref().loaded_nodes_count()
        } else {
            0
        }
    }

    #[cfg(test)]
    pub fn print_trie(&self) {
        if let Some(root) = &self.root {
            print!("Root -> ");
            root.as_ref().print_node(0);
        } else {
            println!("Root -> Null");
        }
    }

    fn del(&mut self, key: Vec<u8>) {
        if self.root.is_none() {
            return;
        }

        let mut result = self.search(key);
        if !result.matched {
            return;
        }

        self.reset_pointers(&mut result.stack);
        self.remove(&mut result.stack);
        self.recover_pointers(result.stack);
    }

    // Reset the pointers to the child which will be changed. This can save memory cost in Arc::make_mut
    fn reset_pointers(&mut self, stack: &mut Vec<(NodePtr, Option<Nibble>)>) {
        self.root = None;
        for (node, child_idx) in stack.iter_mut() {
            if let Some(&child_idx) = child_idx.as_ref() {
                let trie_node = &mut *TrieNodeExt::make_mut(node, &mut self.del_ops);
                *trie_node.child_mut(child_idx).unwrap() = ChildRef::Null;
            }
        }
    }

    fn recover_pointers(&mut self, stack: Vec<(NodePtr, Option<Nibble>)>) {
        let mut last_child: Option<NodePtr> = None;
        for (mut node, child_idx) in stack.into_iter().rev() {
            if let Some(&child_idx) = child_idx.as_ref() {
                let trie_node = &mut *TrieNodeExt::make_mut(&mut node, &mut self.del_ops);
                *trie_node.child_mut(child_idx).unwrap() =
                    ChildRef::Owned(last_child.unwrap().clone());
            }
            last_child = Some(node.clone());
        }
        self.root = last_child;
    }

    fn search(&mut self, key: Vec<u8>) -> SearchResult {
        let mut node = self.root.clone().unwrap();
        let mut stack = vec![(node.clone(), None)];
        let mut remainder = bytes_to_nibble_list(key);
        let mut depth = 1;
        let matched = loop {
            match TrieNode::next::<N>(
                &node,
                &mut remainder,
                &self.db,
                &mut self.loaded_node,
                depth,
            ) {
                NextResult::Matched => {
                    break true;
                }
                NextResult::NotMatched => {
                    break false;
                }
                NextResult::Next((next_node, pos)) => {
                    stack.last_mut().unwrap().1 = Some(pos);
                    stack.push((next_node.clone(), None));
                    node = next_node;
                    depth += 1;
                }
            }
        };

        SearchResult {
            stack,
            matched,
            remainder,
        }
    }
}

impl<const N: usize> MerklePatriciaTree<N> {
    fn insert(
        &mut self,
        last_node: &mut TrieNode,
        remainder: Vec<Nibble>,
        val: Vec<u8>,
        depth: usize,
    ) {
        match last_node {
            TrieNode::Branch { children, .. } => {
                self.insert_on_branch(children, remainder, val, depth)
            }
            TrieNode::Extension { .. } => {
                self.insert_on_extension(last_node, remainder, val, depth)
            }
            TrieNode::Leaf { .. } => self.insert_on_leaf(last_node, remainder, val),
        }
    }

    fn insert_on_branch(
        &mut self,
        children: &mut ChildRefGroup,
        remainder: Vec<Nibble>,
        val: Vec<u8>,
        depth: usize,
    ) {
        assert!(!remainder.is_empty());

        let prev_branch_node = children[*remainder.first().unwrap()].get_mut();
        if prev_branch_node.is_null() {
            let trie_node = TrieNode::new_leaf(remainder[1..].to_vec(), val);
            *prev_branch_node = ChildRef::Owned(trie_node.seal());
            return;
        }

        let mut new_branch_node = TrieNode::new_branch();
        let (new_branch_children, new_branch_value) = new_branch_node.as_branch_mut().unwrap();

        if let Some(idx) = remainder.get(1) {
            let trie_node = TrieNode::new_leaf(remainder[2..].to_vec(), val);
            *new_branch_children[*idx].get_mut() = ChildRef::Owned(trie_node.seal());
        } else {
            *new_branch_value = val.clone();
        }

        let mut trie_node =
            TrieNodeExt::make_mut(prev_branch_node.owned_mut().unwrap(), &mut self.del_ops);
        match &mut *trie_node {
            TrieNode::Leaf { key, value } => {
                // Mark: prev_branch_node should be deleted;
                let value = std::mem::take(value);
                if let Some(&first_key) = key.first() {
                    let trie_node = TrieNode::new_leaf(key[1..].to_vec(), value);
                    *new_branch_children[first_key].get_mut() = ChildRef::Owned(trie_node.seal());
                } else {
                    *new_branch_value = value;
                }
                std::mem::drop(trie_node);
            }
            TrieNode::Extension { key, .. } => {
                let first_key = *key.first().unwrap();
                *key = key[1..].to_vec();
                std::mem::drop(trie_node);
                prev_branch_node.exile::<N>(depth + 2, &mut self.exile_nodes);
                *new_branch_children[first_key].get_mut() = std::mem::take(prev_branch_node);
            }
            TrieNode::Branch { .. } => {
                unreachable!("Search should not end in a node with a branch child")
            }
        }

        *prev_branch_node = ChildRef::Owned(new_branch_node.seal());
    }

    fn insert_on_extension(
        &mut self,
        last: &mut TrieNode,
        remainder: Vec<Nibble>,
        val: Vec<u8>,
        depth: usize,
    ) {
        let (ext_key, child) = last.as_extension_mut().unwrap();
        let intersection: Vec<Nibble> =
            common_prefix_iter(&remainder, &*ext_key).cloned().collect();
        // As the search stopped at the extension node, the ext_key must not be exactly consumed.
        let ext_key_next = ext_key[intersection.len()];
        let ext_key_rest = ext_key[intersection.len() + 1..].to_vec();
        let remainder_next = remainder.get(intersection.len()).cloned();

        let mut new_branch_node = TrieNode::new_branch();
        let (new_branch_children, new_branch_value) = new_branch_node.as_branch_mut().unwrap();

        if !ext_key_rest.is_empty() {
            let child_node = child.loaded_mut(&self.db).unwrap();
            let child_node_borrow = child_node.as_ref();
            if let TrieNode::Branch { .. } = &**child_node_borrow {
                std::mem::drop(child_node_borrow);
                child.exile::<N>(depth + 2, &mut self.exile_nodes);
                *child = ChildRef::Owned(
                    TrieNode::new_extention(ext_key_rest, std::mem::take(child)).seal(),
                );
            } else {
                std::mem::drop(child_node_borrow);
                let mut trie_node = TrieNodeExt::make_mut(child_node, &mut self.del_ops);
                let key = trie_node.key_mut().unwrap();
                add_prefix(key, &ext_key_rest);
            }
        }
        child.exile::<N>(depth + 2, &mut self.exile_nodes);
        *new_branch_children[ext_key_next].get_mut() = std::mem::take(child);

        if let Some(remainder_next) = remainder_next {
            let trie_node = TrieNode::new_leaf(remainder[intersection.len() + 1..].to_vec(), val);
            *new_branch_children[remainder_next].get_mut() = ChildRef::Owned(trie_node.seal());
        } else {
            *new_branch_value = val;
        }

        if !intersection.is_empty() {
            *ext_key = intersection;
            *child = ChildRef::Owned(new_branch_node.seal());
        } else {
            *last = new_branch_node;
        }
    }

    fn insert_on_leaf(&mut self, last_leaf: &mut TrieNode, remainder: Vec<Nibble>, val: Vec<u8>) {
        let (leaf_key, leaf_value) = last_leaf.as_leaf_mut().unwrap();
        let intersection: Vec<Nibble> = common_prefix_iter(&remainder, leaf_key).cloned().collect();
        let rest_remainder = &remainder[intersection.len()..];
        let rest_leaf_key = &leaf_key[intersection.len()..];

        let mut new_branch_node = TrieNode::new_branch();
        let (new_branch_children, new_branch_value) = new_branch_node.as_branch_mut().unwrap();

        if let Some(&rest_idx) = rest_remainder.first() {
            *new_branch_children[rest_idx].get_mut() =
                TrieNode::new_leaf(rest_remainder[1..].to_vec(), val).seal_ref();
        } else {
            *new_branch_value = val;
        }

        if let Some(&rest_idx) = rest_leaf_key.first() {
            let new_leaf_key = rest_leaf_key[1..].to_vec();
            *leaf_key = new_leaf_key;
            *new_branch_children[rest_idx].get_mut() = std::mem::take(last_leaf).seal_ref();
        } else {
            *new_branch_value = std::mem::take(leaf_value);
        }

        *last_leaf = if !intersection.is_empty() {
            TrieNode::new_extention(intersection, new_branch_node.seal_ref())
        } else {
            new_branch_node
        };
    }
}

impl<const N: usize> MerklePatriciaTree<N> {
    fn remove(&mut self, stack: &mut Vec<(NodePtr, Option<Nibble>)>) {
        // #[cfg(test)] {
        //     println!("Show stack");
        //     for (node, _) in stack.iter() {
        //         node.print_node(0);
        //     }
        //     println!("Done stack");
        // }
        let (mut last, _) = stack.pop().unwrap();
        let mut last_node = TrieNodeExt::make_mut(&mut last, &mut self.del_ops);
        match &mut *last_node {
            TrieNode::Branch { children, value } => {
                *value = vec![];
                if let Some((idx, child_ref)) = children.only_child_mut() {
                    let connect_node =
                        self.drop_single_child_branch(idx, std::mem::take(child_ref));
                    self.push_non_branch_to_stack(connect_node, stack);
                    std::mem::drop(last_node);
                } else {
                    std::mem::drop(last_node);
                    stack.push((last, None))
                }
            }
            TrieNode::Leaf { .. } => {
                std::mem::drop(last_node);
                if stack.is_empty() {
                    return;
                }
                let (mut last_parent, idx) = stack.pop().unwrap();
                let mut last_parent_mut =
                    TrieNodeExt::make_mut(&mut last_parent, &mut self.del_ops);
                let idx = idx.unwrap();
                // leaf's parent can not be leaf or extension
                let (children, value) = last_parent_mut.as_branch_mut().unwrap();

                *children[idx].get_mut() = ChildRef::Null;
                if value.is_empty() {
                    if let Some((idx, child_ref)) = children.only_child_mut() {
                        let connect_node =
                            self.drop_single_child_branch(idx, std::mem::take(child_ref));
                        self.push_non_branch_to_stack(connect_node, stack);
                    } else {
                        std::mem::drop(last_parent_mut);
                        // No value, multiple child
                        stack.push((last_parent, None))
                    }
                } else {
                    if children.no_child() {
                        let new_node = TrieNode::new_leaf(vec![], std::mem::take(value));
                        self.push_non_branch_to_stack(new_node, stack);
                    } else {
                        std::mem::drop(last_parent_mut);
                        stack.push((last_parent, None))
                    }
                }
            }
            TrieNode::Extension { .. } => unreachable!(),
        }
    }

    fn drop_single_child_branch(&mut self, idx: Nibble, mut child_ref: ChildRef) -> TrieNode {
        let child = child_ref.loaded_mut(&self.db).unwrap();

        if child.as_ref().key().is_some() {
            // leaf or extension
            let child = &mut *TrieNodeExt::make_mut(child, &mut self.del_ops);
            let key = child.key_mut().unwrap();
            add_prefix(key, &[idx]);
            std::mem::take(child)
        } else {
            TrieNode::new_extention(vec![idx], child_ref)
        }
    }

    fn push_non_branch_to_stack(
        &mut self,
        mut node: TrieNode,
        stack: &mut Vec<(NodePtr, Option<Nibble>)>,
    ) {
        if let Some((last_parent, idx)) = stack.pop() {
            let last_parent_borrow = last_parent.as_ref();
            if let TrieNode::Extension { key: ext_key, .. } = &**last_parent_borrow {
                let key = node.key_mut().unwrap();
                add_prefix(key, &ext_key[..]);
            } else {
                // Branch, parent cannot be leaf.
                std::mem::drop(last_parent_borrow);
                stack.push((last_parent, idx));
            }
        }
        stack.push((node.seal(), None));
    }
}
