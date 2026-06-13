mod key;
mod name;
mod node;
mod tree;

pub use self::{
    key::Key,
    name::TreeName,
    node::{EpochPosition, Node, MAX_VERSION_NUMBER},
    tree::{AMTNodeIndex, VerInfo, VersionTree},
};
use crate::{
    amt::{AMTConfigTrait, AMTree},
    crypto::export::{Pairing, G1},
    storage::{FlattenArray, FlattenTree},
};

#[derive(Copy, Clone)]
pub struct AMTConfig;

impl AMTConfigTrait for AMTConfig {
    type PE = Pairing;
    type Name = TreeName;
    type Data = Node;
    type Commitment = G1<Pairing>;
    type DataLayout = FlattenArray;
    type TreeLayout = FlattenTree;
    type Height = crate::crypto::TypeDepths;
}

type Tree = AMTree<AMTConfig>;
pub type Commitment = G1<<AMTConfig as AMTConfigTrait>::PE>;

const DEPTHS: usize = <AMTConfig as AMTConfigTrait>::DEPTHS;

//TODO: Store Key for non-existent proof
