extern crate blake2;
extern crate ethereum_types;
extern crate hash_db;
extern crate plain_hasher;

use blake2::{Blake2b512, Blake2s256, Digest};
use ethereum_types::H256;
use hash_db::Hasher;
use plain_hasher::PlainHasher;

/// Concrete `Hasher` impl for the Keccak-256 hash
#[derive(Default, Debug, Clone, PartialEq)]
pub struct Blake2sHasher;
impl Hasher for Blake2sHasher {
    type Out = H256;
    type StdHasher = PlainHasher;
    const LENGTH: usize = 32;
    fn hash(x: &[u8]) -> Self::Out {
        let mut hasher = Blake2s256::new();
        hasher.update(x);
        let digest = hasher.finalize();
        let mut answer = H256::zero();
        answer.0[..].copy_from_slice(&digest);
        answer
    }
}

pub fn blake2s<T: AsRef<[u8]>>(s: T) -> H256 {
    Blake2sHasher::hash(s.as_ref())
}

/// Concrete `Hasher` impl for the Keccak-256 hash
#[derive(Default, Debug, Clone, PartialEq)]
pub struct Blake2bHasher;
impl Hasher for Blake2bHasher {
    type Out = H256;
    type StdHasher = PlainHasher;
    const LENGTH: usize = 32;
    fn hash(x: &[u8]) -> Self::Out {
        let mut hasher = Blake2b512::new();
        hasher.update(x);
        let digest = hasher.finalize();
        let mut answer = H256::zero();
        answer.0[..].copy_from_slice(&digest[..Self::LENGTH]);
        answer
    }
}

pub fn blake2b<T: AsRef<[u8]>>(s: T) -> H256 {
    Blake2bHasher::hash(s.as_ref())
}
