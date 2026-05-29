//! Merkle Tree implementation for efficient cryptographic verification of data integrity.
//!
//! This module provides a complete implementation of a Merkle Tree data structure,
//! optimized for blockchain use cases. It includes:
//!
//! - [`Tree`]: The main Merkle tree implementation with support for incremental updates
//! - [`Twig`]: A sub-tree component that handles leaf-level operations
//! - [`TwigFile`]: Persistent storage for twig data
//! - [`proof`]: Merkle proof generation and verification
//! - [`check`]: Tree consistency validation utilities
//! - [`recover`]: Recovery mechanisms for tree reconstruction
//! - [`helpers`]: Common utilities for tree operations
//!
//! The implementation is designed for high performance and memory efficiency,
//! with support for concurrent access and incremental updates.

pub mod check;
pub mod helpers;
//pub mod proof;
pub mod multiproof;
pub mod recover;
pub mod singleproof;
pub mod tree;
pub mod twigfile;

pub use tree::{Tree, UpperTree};
pub use twigfile::TwigFile;
