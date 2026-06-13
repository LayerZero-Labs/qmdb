use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;

use global::Global;
use hashbrown::HashMap;
use kvdb::{DBOp, DBTransaction};

use crate::serde::{MyFromBytes, MyToBytes};

use super::layout::LayoutTrait;
use super::DBColumn;

pub static PUT_COUNT: Global<[u64; 4]> = Global::INIT;
pub static PUT_MODE: Global<usize> = Global::INIT;

#[derive(Clone)]
pub struct DBAccess<
    K: Copy + Clone + Debug + Eq + Hash,
    V: Default + Clone + MyFromBytes + MyToBytes,
    L: LayoutTrait<K>,
> {
    prefix: Vec<u8>,
    db: DBColumn,
    cache: HashMap<K, (V, bool)>,
    _phantom: PhantomData<(K, V, L)>,
}

impl<
        K: Copy + Clone + Debug + Eq + Hash,
        V: Default + Clone + MyFromBytes + MyToBytes,
        L: LayoutTrait<K>,
    > DBAccess<K, V, L>
{
    pub fn new(prefix: Vec<u8>, db: DBColumn) -> Self {
        Self {
            prefix,
            db,
            cache: Default::default(),
            _phantom: PhantomData,
        }
    }

    pub fn get(&mut self, node_index: &K) -> &V {
        let (value, _dirty) = self.ensure_cached(node_index);
        value
    }

    pub fn get_mut(&mut self, node_index: &K) -> &mut V {
        let (value, dirty) = self.ensure_cached(node_index);
        *dirty = true;
        value
    }

    fn ensure_cached(&mut self, node_index: &K) -> &mut (V, bool) {
        let (prefix, db) = (&self.prefix, &self.db);

        self.cache.entry(*node_index).or_insert_with(|| {
            let db_key = Self::compute_key(&prefix, node_index);

            let value = match db.get(&db_key).unwrap() {
                Some(x) => V::from_bytes_local(&*x).unwrap(),
                None => V::default(),
            };
            (value, false)
        })
    }

    pub fn set(&mut self, node_index: &K, value: V) {
        self.cache.insert(*node_index, (value, true));
    }

    pub fn flush_cache(&mut self) {
        let prefix = &self.prefix;
        let ops: Vec<DBOp> = self
            .cache
            .iter_mut()
            .filter(|(_k, (_v, dirty))| *dirty)
            .map(|(key, (value, dirty))| {
                *dirty = false;
                let db_key = Self::compute_key(&prefix, key);
                DBOp::Insert {
                    col: 0,
                    key: db_key.into(),
                    value: value.to_bytes_local(),
                }
            })
            .collect();

        (*PUT_COUNT.lock_mut().unwrap())[*PUT_MODE.lock().unwrap()] += ops.len() as u64;

        self.db.write_buffered(DBTransaction { ops });
        self.cache.clear();
    }

    fn compute_key(name: &[u8], node_index: &K) -> Vec<u8> {
        let layout_index = <L as LayoutTrait<K>>::position(node_index) as u32;

        let mut key = name.to_vec();
        key.extend_from_slice(&layout_index.to_be_bytes());
        key
    }
}

#[cfg(test)]
mod test {
    use crate::crypto::TypeUInt;
    use crate::type_uint;

    use super::*;

    type_uint! {
        struct TestDepths(6);
    }

    #[test]
    fn test_backend() {
        type NodeIndex = crate::amt::NodeIndex<TestDepths>;
        type FlattenTree = super::super::FlattenTree;

        const DEPTHS: usize = TestDepths::USIZE;
        const TMP_RATIO: usize = 719323;

        let db = crate::storage::test_db_col();
        let mut tree =
            DBAccess::<NodeIndex, u64, FlattenTree>::new("test".to_string().into_bytes(), db);

        for depth in 0..DEPTHS {
            for index in 0..(1 << depth) {
                let node_index = &NodeIndex::new(depth, index);
                *tree.get_mut(node_index) = (TMP_RATIO * depth) as u64;
                *tree.get_mut(node_index) += index as u64;
            }
        }

        tree.flush_cache();

        for depth in 0..DEPTHS {
            for index in 0..(1 << depth) {
                let node_index = &NodeIndex::new(depth, index);
                assert_eq!(
                    (TMP_RATIO * depth + index) as u64,
                    *tree.get_mut(node_index)
                )
            }
        }

        drop(tree);
    }
}
