//! Utility types and functions for the QMDB database.
//!
//! This module provides various utilities including:
//! - Buffer management
//! - Shard mapping
//! - Operation recording
//! - Active bit tracking
//! - Cache management
//! - Channel implementations
//! - Data encoding/decoding
//! - Hashing functions

pub mod activebits;
pub mod bitmap;
pub mod bytescache;
pub mod intset;
pub mod lfsr;
pub mod ringchannel;
pub mod shortlist;
pub mod writebatch;
