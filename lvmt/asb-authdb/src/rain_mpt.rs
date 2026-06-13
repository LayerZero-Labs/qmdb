use std::sync::{Arc, RwLock};

use authdb::AuthDB;
use kvdb::KeyValueDB;
use rainblock_trie::MerklePatriciaTree;

const CACHED_LEVEL: usize = 6;
pub struct RainMpt(
    RwLock<MerklePatriciaTree<CACHED_LEVEL>>,
    Arc<dyn KeyValueDB>,
);

pub fn new(backend: Arc<dyn KeyValueDB>) -> RainMpt {
    RainMpt(
        RwLock::new(MerklePatriciaTree::<CACHED_LEVEL>::new(backend.clone())),
        backend,
    )
}

impl AuthDB for RainMpt {
    fn get(&self, key: Vec<u8>) -> Option<Box<[u8]>> {
        self.0.write().unwrap().get(key).map(Vec::into_boxed_slice)
    }

    fn set(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.0.write().unwrap().put(key, value);
    }

    fn commit(&mut self, _index: usize) {
        self.0.write().unwrap().commit().unwrap();
    }

    fn backend(&self) -> Option<&dyn KeyValueDB> {
        Some(&*self.1)
    }

    fn flush_all(&mut self) {
        self.0.write().unwrap().flush_all().unwrap()
    }
}
