use super::node::{AMTNode, NodeIndex};
use super::write_guard::AMTNodeWriteGuard;
use crate::crypto::export::{PairingEngine, PrimeField, ProjectiveCurve, Zero};
use crate::crypto::{
    export::{Fr, FrInt, G1},
    AMTParams, TypeUInt,
};
use crate::serde::{MyFromBytes, MyToBytes};
use crate::storage::access::PUT_MODE;
use crate::storage::{DBAccess, DBColumn, LayoutTrait};
use std::sync::Arc;

pub trait AMTConfigTrait {
    type PE: PairingEngine<G1Projective = Self::Commitment>;
    type Name: MyToBytes;
    type Data: AMTData<Fr<Self::PE>> + Default + Clone + MyToBytes + MyFromBytes;
    type Commitment: ProjectiveCurve + MyToBytes + MyFromBytes;

    type DataLayout: LayoutTrait<usize>;
    type TreeLayout: LayoutTrait<NodeIndex<Self::Height>>;
    type Height: TypeUInt;

    const DEPTHS: usize = Self::Height::USIZE;
    const LENGTH: usize = 1 << Self::DEPTHS;
    const IDX_MASK: usize = Self::LENGTH - 1;
}

pub trait AMTData<P: PrimeField> {
    fn as_fr_int(&self) -> P::BigInt;
    fn as_fr(&self) -> P {
        self.as_fr_int().into()
    }
}

#[derive(Clone)]
pub struct AMTree<C: AMTConfigTrait> {
    pub name: C::Name,
    data: DBAccess<usize, C::Data, C::DataLayout>,
    subtree_roots: DBAccess<usize, C::Commitment, C::DataLayout>,
    inner_nodes: DBAccess<NodeIndex<C::Height>, AMTNode<C::Commitment>, C::TreeLayout>,
    commitment: Option<G1<C::PE>>,

    dirty: bool,
    shard_root: Option<NodeIndex<C::Height>>,

    pp: Arc<AMTParams<C::PE>>,
}

pub type AMTProof<G> = Vec<AMTNode<G>>;

impl<C: AMTConfigTrait> AMTree<C> {
    pub fn new(
        name: C::Name,
        db: DBColumn,
        pp: Arc<AMTParams<C::PE>>,
        shard_root: Option<NodeIndex<C::Height>>,
    ) -> Self {
        let ser_name = name.to_bytes_consensus();
        let set_prefix = |prefix: u8| {
            let mut prefix = vec![prefix];
            prefix.extend_from_slice(&ser_name);
            prefix
        };
        Self {
            name,

            data: DBAccess::new(set_prefix(1), db.clone()),
            inner_nodes: DBAccess::new(set_prefix(2), db.clone()),
            subtree_roots: DBAccess::new(set_prefix(3), db.clone()),

            commitment: None,
            dirty: false,
            shard_root,
            pp,
        }
    }

    pub fn set_commitment(&mut self, commitment: &G1<C::PE>) {
        if self.commitment.is_none() {
            self.commitment = Some(commitment.clone())
        }
    }

    // Because the underlying data has a cache, so most read operation requires a mutable ref.
    pub fn get(&mut self, index: usize) -> &C::Data {
        assert!(index < C::LENGTH);
        self.data.get(&index)
    }

    pub(super) fn get_mut(&mut self, index: usize) -> &mut C::Data {
        assert!(index < C::LENGTH);
        self.data.get_mut(&index)
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn only_root(&self) -> bool {
        self.shard_root.is_none()
    }

    pub fn can_prove(&self) -> bool {
        self.shard_root == Some(NodeIndex::root())
    }

    pub fn write_versions(&mut self, index: usize) -> AMTNodeWriteGuard<C> {
        let value = std::mem::take(self.data.get_mut(&index));
        AMTNodeWriteGuard::new(index, value, self)
    }

    pub fn subtree_root_mut(&mut self, index: usize) -> &mut G1<C::PE> {
        assert!(index < C::LENGTH);
        self.subtree_roots.get_mut(&index)
    }

    pub fn subtree_root(&mut self, index: usize) -> &G1<C::PE> {
        &*self.subtree_root_mut(index)
    }

    pub fn commitment(&mut self) -> &G1<C::PE> {
        self.commitment.as_ref().unwrap()
    }

    pub fn flush(&mut self) -> G1<C::PE> {
        *PUT_MODE.lock_mut().unwrap() = 0;
        self.data.flush_cache();

        *PUT_MODE.lock_mut().unwrap() = 1;
        self.inner_nodes.flush_cache();

        *PUT_MODE.lock_mut().unwrap() = 2;
        self.subtree_roots.flush_cache();

        self.dirty = false;
        self.commitment.unwrap().clone()
    }

    pub fn update(&mut self, index: usize, update_fr_int: FrInt<C::PE>) {
        assert!(index < C::LENGTH);

        if update_fr_int == FrInt::<C::PE>::from(0) {
            return;
        }

        self.dirty = true;

        let inc_comm = self.pp.get_idents_pow(index, &update_fr_int);

        // Update commitment
        *self.commitment.as_mut().unwrap() += &inc_comm;

        let shard_root = if let Some(v) = self.shard_root {
            v
        } else {
            return;
        };

        let leaf_index = bitreverse(index, C::DEPTHS);
        let node_index = NodeIndex::new(C::DEPTHS, leaf_index);

        for (height, depth) in (0..C::DEPTHS).map(|height| (height, C::DEPTHS - height)) {
            let visit_node_index = node_index.to_ancestor(height);

            if !visit_node_index.needs_maintain(&shard_root) {
                continue;
            }

            // let proof = self.pp.get_quotient(depth, index).mul(update_fr_int);
            let proof = self.pp.get_quotient_pow(depth, index, &update_fr_int);
            let node = self.inner_nodes.get_mut(&visit_node_index);
            node.commitment += &inc_comm;
            node.proof += proof;
        }
    }

    pub fn prove(&mut self, index: usize) -> Option<AMTProof<G1<C::PE>>> {
        if !self.can_prove() {
            return None;
        }
        let leaf_index = bitreverse(index, C::DEPTHS);
        let node_index = NodeIndex::new(C::DEPTHS, leaf_index);

        let mut answers = vec![Default::default(); C::DEPTHS];

        for visit_depth in (1..=C::DEPTHS).rev() {
            let visit_height = C::DEPTHS - visit_depth;
            let sibling_node_index = node_index.to_ancestor(visit_height).to_sibling();

            answers[visit_depth - 1] = self.inner_nodes.get_mut(&sibling_node_index).clone();
        }
        Some(answers)
    }

    pub fn verify(
        index: usize,
        value: Fr<C::PE>,
        commitment: &G1<C::PE>,
        proof: AMTProof<G1<C::PE>>,
        pp: &AMTParams<C::PE>,
    ) -> bool {
        assert!(index < C::LENGTH);
        let self_indent = pp.get_commitments(index).mul(value.into());
        let others: G1<C::PE> = proof.iter().map(|node| node.commitment).sum();

        if *commitment != self_indent + &others {
            println!(
                "Commitment check fail {},{},{}",
                self_indent.is_zero(),
                others.is_zero(),
                commitment.is_zero()
            );
            return false;
        }

        for (idx, node) in proof.iter().copied().enumerate() {
            let height = C::DEPTHS - idx - 1;
            let depth = idx + 1;
            let verification = *pp.get_sibling_verification(depth, index);
            if C::PE::pairing(node.commitment, pp.g2()) != C::PE::pairing(node.proof, verification)
            {
                println!("Pairing check fails at height {}", height);
                return false;
            }
        }
        return true;
    }
}

#[inline]
fn bitreverse(mut n: usize, l: usize) -> usize {
    let mut r = 0;
    for _ in 0..l {
        r = (r << 1) | (n & 1);
        n >>= 1;
    }
    r
}
