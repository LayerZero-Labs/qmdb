#[cfg(feature = "light-hash")]
pub use blake2_hasher::Blake2bHasher as DBHasher;
#[cfg(not(feature = "light-hash"))]
pub use keccak_hasher::KeccakHasher as DBHasher;
