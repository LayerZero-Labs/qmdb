#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate static_assertions;

extern crate base64;
extern crate core;
#[cfg(test)]
extern crate kvdb_memorydb;

pub mod amt;
pub mod crypto;
mod enable_log;
pub mod lvmt_db;
pub mod merkle;
pub mod multi_layer_amt;
pub mod serde;
pub mod single_amt;
pub mod storage;

pub use crate::lvmt_db::{LvmtDB, LvmtRoot, Proof};
pub use multi_layer_amt::Key;

#[allow(unused)]
use enable_log::*;

#[cfg(not(any(feature = "medium_lvmt", feature = "large_lvmt", feature = "huge_lvmt")))]
const DEPTHS: usize = 8;
#[cfg(feature = "media_lvmt")]
const DEPTHS: usize = 12;
#[cfg(feature = "large_lvmt")]
const DEPTHS: usize = 16;
#[cfg(feature = "huge_lvmt")]
const DEPTHS: usize = 20;
