//! Core operations and types within the Nearly Optimal Merkle Trie.
//!
//! This crate defines the schema and basic operations over the merkle trie in a backend-agnostic
//! manner.
//!
//! The core types and proof verification routines of this crate do not require the
//! standard library, but do require Rust's alloc crate.

#![cfg_attr(all(not(feature = "std"), not(test)), no_std)]

extern crate alloc;

pub mod hasher;
pub mod page;
pub mod page_id;
pub mod proof;
pub mod trie;
pub mod trie_pos;
pub mod update;
pub mod witness;
