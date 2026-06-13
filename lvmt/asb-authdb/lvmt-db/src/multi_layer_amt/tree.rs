use std::collections::VecDeque;
use std::sync::Arc;

use hashbrown::{HashMap, HashSet};
use kvdb::{DBKey, DBOp, DBTransaction};

use crate::amt::AMTConfigTrait;
use lvmt_serde_derive::{MyFromBytes, MyToBytes};

use crate::crypto::export::Zero;
use crate::crypto::{
    export::{
        instances::{FrInt, G1Aff, G1},
        BigInteger, Pairing, ProjectiveCurve,
    },
    AMTParams,
};
use crate::multi_layer_amt::AMTConfig;
use crate::serde::{MyFromBytes, MyToBytes};
use crate::storage::DBColumn;

use super::{Commitment, EpochPosition, Key, Tree, TreeName, MAX_VERSION_NUMBER};

type NodeIndex = u32;
type TreesLayer = HashMap<Vec<NodeIndex>, TreeWithInfo>;
pub type AMTNodeIndex = crate::amt::NodeIndex<<AMTConfig as AMTConfigTrait>::Height>;

const ROOT_KEY: [u8; 2] = [0, 0];

/// The `VersionTree`
#[derive(Clone)]
pub struct VersionTree {
    producer: TreeProducer,
    forest: Vec<TreesLayer>,

    shard_node: Option<AMTNodeIndex>,
}

impl VersionTree {
    pub fn new(
        db: DBColumn,
        pp: Arc<AMTParams<Pairing>>,
        shard_node: Option<AMTNodeIndex>,
    ) -> Self {
        let mut forest = Vec::<TreesLayer>::with_capacity(8);
        forest.push(Default::default());
        let root = db
            .get(ROOT_KEY.as_ref())
            .unwrap()
            .map_or(G1::zero(), |x| G1::from_bytes_local(&x).unwrap());
        let producer = TreeProducer {
            db,
            pp,
            shard_node: shard_node.clone(),
        };
        let mut root_tree = TreeWithInfo {
            tree: producer.new_tree(&TreeName::root()),
            mark_in_parent: true,
            children_marks: Default::default(),
        };

        root_tree.tree.set_commitment(&root);
        forest[0].insert(vec![], root_tree);

        Self {
            forest,
            producer,
            shard_node,
        }
    }

    pub(crate) fn get_tree_mut(&mut self, name: &TreeName) -> &mut Tree {
        let (ancestor_layers, tree_layer) = {
            let level = name.0.len();
            if self.forest.len() < level + 1 {
                self.forest.resize(level + 1, Default::default())
            }
            let (front, end) = self.forest.split_at_mut(level);
            (front, &mut end[0])
        };

        let producer = &self.producer;
        let new_tree = |tree_name: &TreeName| TreeWithInfo {
            tree: producer.new_tree(tree_name),
            mark_in_parent: false,
            children_marks: Default::default(),
        };

        let tree_with_info = tree_layer
            .entry(name.0.clone())
            .or_insert_with(|| new_tree(&name));

        let mut last_tree = &mut tree_with_info.tree;

        if !tree_with_info.mark_in_parent {
            tree_with_info.mark_in_parent = true;

            for (level, layer) in ancestor_layers.iter_mut().enumerate().rev() {
                let anc_tree_name = TreeName(name.0[..level].to_vec());
                let sub_index = name.0[level];
                let tree_with_info = layer
                    .entry(anc_tree_name.clone().0)
                    .or_insert_with(|| new_tree(&anc_tree_name));
                tree_with_info.children_marks.insert(sub_index);
                last_tree.set_commitment(&tree_with_info.tree.subtree_root(sub_index as usize));
                last_tree = &mut tree_with_info.tree;

                if tree_with_info.mark_in_parent {
                    break;
                } else {
                    tree_with_info.mark_in_parent = true;
                }
            }
        }
        &mut tree_with_info.tree
    }

    pub fn inc_key_ver(&mut self, key: &Key, version: Option<VerInfo>) -> VerInfo {
        let VerInfo {
            version,
            level,
            slot_index,
        } = match version {
            None => self.allocate_vacant_slot(key),
            Some(ver_info) => ver_info,
        };

        let shard_node = self.shard_node.clone();
        let visit_amt = self.get_tree_mut(&key.tree_at_level(level));
        let node = key.index_at_level(level);

        let in_proof_shard = if let Some(shard_node) = shard_node {
            AMTNodeIndex::leaf(key.index_at_level(0)).needs_maintain(&shard_node)
        } else {
            false
        };

        if !in_proof_shard {
            visit_amt.update(node, fr_int_pow_2((slot_index as u32 + 1) * 40));
        } else {
            // Maintain necessary data for proof.
            visit_amt.write_versions(node).key_versions[slot_index as usize] += 1;
        }
        assert!(version < MAX_VERSION_NUMBER);
        return VerInfo {
            version: version + 1,
            level,
            slot_index,
        };
    }

    pub fn allocate_vacant_slot(&mut self, key: &Key) -> VerInfo {
        for level in 0..32 {
            if level >= 3 {
                println!("Level {}, allocate slot for {:?}", level, key.0);
            }
            let visit_amt = self.get_tree_mut(&key.tree_at_level(level));
            let node_index = key.index_at_level(level);

            if visit_amt.get(node_index).key_versions.len() < 5 {
                let mut data = visit_amt.write_versions(node_index);
                let slot_index = data.key_versions.len();
                data.key_versions.push(0);
                std::mem::drop(data);

                return VerInfo {
                    version: 0,
                    level,
                    slot_index: slot_index as u8,
                };
            }
        }
        panic!("Exceed maximum support level");
    }

    fn commit_tree(
        name: &TreeName,
        epoch: u64,
        start_pos: u64, //TODO: ugly.
        layers: &mut [TreesLayer],
        updates: &mut SubTreeRootRecorder,
    ) -> (bool, Commitment) {
        let (this_layer, rest_layers) = layers.split_first_mut().unwrap();

        let tree_with_info = this_layer.get_mut(&name.0).unwrap();

        let mut indices: Vec<&NodeIndex> = tree_with_info.children_marks.iter().collect();
        indices.sort_unstable();
        for &&index in indices.iter() {
            let child_name = name.child(index);
            let (dirty, commitment) =
                Self::commit_tree(&child_name, epoch, start_pos, rest_layers, updates);
            if dirty {
                *tree_with_info.tree.subtree_root_mut(index as usize) = commitment;
                let mut node = tree_with_info.tree.write_versions(index as usize);
                node.tree_version += 1;
                node.tree_position = EpochPosition {
                    epoch,
                    position: start_pos + updates.len() as u64,
                };
                updates.push(child_name, node.tree_version, commitment);
                std::mem::drop(node);
            }
        }
        tree_with_info.children_marks.clear();
        tree_with_info.mark_in_parent = false;
        let tree = &mut tree_with_info.tree;

        let dirty = tree.dirty();
        // let commitment = tree.flush();
        let commitment = if name.0.len() > 0 {
            tree.flush()
        } else {
            tree.commitment().clone()
        };

        return (dirty, commitment);
    }

    pub fn commit(
        &mut self,
        epoch: u64,
        start_pos: u64,
    ) -> (Commitment, impl IntoIterator<Item = (TreeName, u64, G1Aff)>) {
        let mut updates = SubTreeRootRecorder::with_capacity(1 << 20);
        let (_, commitment) = Self::commit_tree(
            &TreeName::root(),
            epoch,
            start_pos,
            &mut self.forest,
            &mut updates,
        );
        // for layers in self.forest.iter_mut().skip(1) {
        //     layers.clear();
        // }
        updates.to_affine_in_batch();

        return (commitment, updates);
    }

    pub fn flush_all(&mut self) {
        let commitment: G1 = self.get_tree_mut(&TreeName::root()).flush();
        self.producer.db.write_buffered(DBTransaction {
            ops: vec![DBOp::Insert {
                col: 0,
                key: DBKey::from(ROOT_KEY.as_ref()),
                value: commitment.to_bytes_local(),
            }],
        })
    }
}

pub struct SubTreeRootRecorderIter(SubTreeRootRecorder);

impl Iterator for SubTreeRootRecorderIter {
    type Item = (TreeName, u64, G1Aff);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((tree_name, version)) = self.0.tree_version.pop_front() {
            let commitment: G1Aff = self.0.commitments.pop_front().unwrap().into();
            Some((tree_name, version, commitment))
        } else {
            None
        }
    }
}

pub struct SubTreeRootRecorder {
    tree_version: VecDeque<(TreeName, u64)>,
    commitments: VecDeque<G1>,
}

impl SubTreeRootRecorder {
    fn with_capacity(n: usize) -> Self {
        Self {
            tree_version: VecDeque::with_capacity(n),
            commitments: VecDeque::with_capacity(n),
        }
    }

    fn push(&mut self, name: TreeName, version: u64, commitment: G1) {
        self.commitments.push_back(commitment);
        self.tree_version.push_back((name, version));
    }

    fn to_affine_in_batch(&mut self) {
        let (slice1, slice2) = self.commitments.as_mut_slices();
        assert_eq!(slice2.len(), 0);
        ProjectiveCurve::batch_normalization(slice1);
    }

    fn len(&self) -> usize {
        self.tree_version.len()
    }
}

impl IntoIterator for SubTreeRootRecorder {
    type Item = (TreeName, u64, G1Aff);
    type IntoIter = SubTreeRootRecorderIter;

    fn into_iter(self) -> Self::IntoIter {
        SubTreeRootRecorderIter(self)
    }
}

#[derive(Clone)]
struct TreeProducer {
    pub db: DBColumn,
    pub pp: Arc<AMTParams<Pairing>>,
    pub shard_node: Option<AMTNodeIndex>,
}

impl TreeProducer {
    fn new_tree(&self, name: &TreeName) -> Tree {
        let shard_root = if let Some(ref shard_node) = self.shard_node {
            if name.0.len() == 0 {
                Some(shard_node.clone())
            } else {
                let root_tree_leaf = AMTNodeIndex::leaf(name.0[0] as usize);
                if root_tree_leaf.needs_maintain(shard_node) {
                    Some(AMTNodeIndex::root())
                } else {
                    None
                }
            }
        } else {
            None
        };
        Tree::new(name.clone(), self.db.clone(), self.pp.clone(), shard_root)
    }
}

#[derive(Default, Copy, Clone, Debug, MyFromBytes, MyToBytes)]
pub struct VerInfo {
    pub version: u64,
    pub level: u8,
    pub slot_index: u8,
}

#[derive(Clone)]
struct TreeWithInfo {
    tree: Tree,
    mark_in_parent: bool,
    children_marks: HashSet<NodeIndex>,
}

fn fr_int_pow_2(power: u32) -> FrInt {
    let mut fr_int = FrInt::from(1);
    fr_int.muln(power);
    fr_int
}
