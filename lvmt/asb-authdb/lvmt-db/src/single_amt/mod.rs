use crate::amt::{AMTConfigTrait, AMTData, AMTree, NodeIndex};
use crate::crypto::export::{
    instances::{Fr, FrInt, G1Aff, G1},
    Pairing, ProjectiveCurve, Zero,
};
use crate::crypto::{AMTParams, TypeUInt};
use crate::serde::{MyFromBytes, MyToBytes, SerdeType};
use crate::storage::{DBColumn, FlattenArray, FlattenTree};
use keccak_hash::{keccak, H256};
use kvdb::{DBKey, DBOp, DBTransaction, KeyValueDB};
use lvmt_serde_derive::{MyFromBytes, MyToBytes};
use std::io::Write;
use std::sync::{Arc, RwLock};

const ROOT_KEY: [u8; 2] = [0, 0];

#[derive(Copy, Clone)]
struct TreeName;

impl MyToBytes for TreeName {
    fn write<W: Write>(&self, mut writer: W, _ty: SerdeType) -> std::io::Result<()> {
        writer.write(&[])?;
        Ok(())
    }
}

#[derive(Default, Clone, Debug, MyFromBytes, MyToBytes)]
struct Node {
    data: Vec<u8>,
    hash: H256,
}

impl AMTData<Fr> for Node {
    #[cfg(target_endian = "little")]
    fn as_fr_int(&self) -> FrInt {
        let mut result = unsafe { std::mem::transmute::<[u8; 32], [u64; 4]>(self.hash.0.clone()) };
        result[3] &= 0x3fffffff;
        FrInt::new(result)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Height<const N: usize>;

impl<const N: usize> TypeUInt for Height<N> {
    const USIZE: usize = N;
}

#[derive(Copy, Clone)]
struct AMTConfig<const N: usize>;

impl<const N: usize> AMTConfigTrait for AMTConfig<N> {
    type PE = Pairing;
    type Name = TreeName;
    type Data = Node;
    type Commitment = G1;
    type DataLayout = FlattenArray;
    type TreeLayout = FlattenTree;
    type Height = Height<N>;
}

#[derive(Clone)]
pub struct AmtDB<const N: usize> {
    root: G1,
    amt: Arc<RwLock<AMTree<AMTConfig<N>>>>,
    pub db: Arc<dyn KeyValueDB>,
}

impl<const N: usize> AmtDB<N> {
    pub fn new(
        db: Arc<dyn KeyValueDB>,
        pp: Arc<AMTParams<Pairing>>,
        shard_node: Option<(usize, usize)>,
    ) -> Self {
        let db_col = DBColumn::from_kvdb(db.clone(), 0);
        let root = db_col
            .get(ROOT_KEY.as_ref())
            .unwrap()
            .map_or(G1::zero(), |x| G1::from_bytes_local(&x).unwrap());

        let shard_root = shard_node.map(|(depth, index)| NodeIndex::<Height<N>>::new(depth, index));

        let mut amt = AMTree::<AMTConfig<N>>::new(TreeName, db_col, pp, shard_root);
        amt.set_commitment(&root);

        Self {
            root,
            amt: Arc::new(RwLock::new(amt)),
            db,
        }
    }

    fn index(key: &[u8]) -> usize {
        let bytes = (N + 7) / 8;
        assert!(key.len() >= bytes);
        let mut index: [u8; 8] = [0u8; 8];
        index.copy_from_slice(&key[..bytes]);
        let mut index = u64::from_le_bytes(index);
        index &= (1 << N) - 1;
        return index as usize;
    }

    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let mut amt = self.amt.write().unwrap();
        let node = amt.get(Self::index(key));
        return if node.hash == H256::zero() {
            None
        } else {
            Some(node.data.clone())
        };
    }

    pub fn set(&mut self, key: &[u8], value: Vec<u8>) {
        let new_node = Node {
            hash: keccak(value.as_slice()),
            data: value,
        };
        let mut amt = self.amt.write().unwrap();
        *amt.write_versions(Self::index(key)) = new_node;
    }

    pub fn commit(&mut self) -> G1Aff {
        self.root = self.amt.write().unwrap().flush();
        self.db.write_buffered(DBTransaction {
            ops: vec![DBOp::Insert {
                col: 0,
                key: DBKey::from(ROOT_KEY.as_ref()),
                value: self.root.clone().to_bytes_local(),
            }],
        });
        self.db.flush().unwrap();
        return self.root.into_affine();
    }
}
