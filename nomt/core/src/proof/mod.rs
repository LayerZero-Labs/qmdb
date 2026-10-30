//! Trie proofs and proof verification.
//!
//! The Merkle Trie defined in NOMT is an authenticated data structure, which means that it permits
//! efficient proving against the root. This module exposes types and functions necessary for
//! handling these kinds of proofs.
//!
//! Using the types and functions exposed from this module, you can verify the value of a single
//! key within the trie ([`PathProof`]), the values of multiple keys ([`MultiProof`]), or the result
//! of updating a trie with a set of changes ([`verify_update`]).

pub use multi_proof::{
    verify as verify_multi_proof, verify_update as verify_multi_proof_update, MultiPathProof,
    MultiProof, MultiProofVerificationError, VerifiedMultiProof,
};
pub use path_proof::{
    verify_update, KeyOutOfScope, PathProof, PathProofTerminal, PathProofVerificationError,
    PathUpdate, VerifiedPathProof, VerifyUpdateError,
};

mod multi_proof;
mod path_proof;
