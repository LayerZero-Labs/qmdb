use parking_lot::RwLock;
use qmdb::test_helper::SimpleTask;
use std::path::Path;

#[cfg(feature = "use_nomt")]
pub const NAME: &str = "NOMT";

#[cfg(feature = "use_reth")]
pub const NAME: &str = "RETH";

#[cfg(feature = "use_lvmt")]
pub const NAME: &str = "LVMT";

#[cfg(all(not(feature = "use_nomt"), not(feature = "use_reth"), not(feature = "use_lvmt")))]
pub const NAME: &str = "QMDB";

#[cfg(feature = "use_nomt")]
pub fn init(db_dir: &str, _: usize, _: bool) {
    let path = Path::new(db_dir).join("NOMT");
    crate::speed::nomt_adapter::init(path.to_str().unwrap());
}

#[cfg(feature = "use_reth")]
pub fn init(db_dir: &str, _: usize, is_eth_mode: bool) {
    let path = Path::new(db_dir).join("RETH");
    crate::speed::reth_adapter::init(path.to_str().unwrap(), is_eth_mode);
}

#[cfg(feature = "use_lvmt")]
pub fn init(db_dir: &str, _: usize, _: bool) {
    let path = Path::new(db_dir).join("LVMT");
    crate::speed::lvmt_adapter::init(path.to_str().unwrap());
}

#[cfg(all(not(feature = "use_nomt"), not(feature = "use_reth"), not(feature = "use_lvmt")))]
pub fn init(db_dir: &str, table_id: usize, _: bool) {
    let path = Path::new(db_dir).join("ADS");
    crate::speed::qmdb::init(path.to_str().unwrap(), table_id);
}

#[cfg(feature = "use_nomt")]
pub fn create_kv(_: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::nomt_adapter::create_kv(height, task_list);
}

#[cfg(feature = "use_reth")]
pub fn create_kv(_: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::reth_adapter::create_kv(height, task_list);
}

#[cfg(feature = "use_lvmt")]
pub fn create_kv(_: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::lvmt_adapter::create_kv(height, task_list);
}

#[cfg(all(not(feature = "use_nomt"), not(feature = "use_reth"), not(feature = "use_lvmt")))]
pub fn create_kv(table_id: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::qmdb::create_kv(table_id, height, task_list);
}

#[cfg(feature = "use_nomt")]
pub fn update_kv(_: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    // NOMT handles update and create the same way, as writes
    crate::speed::nomt_adapter::update_kv(height, task_list);
}

#[cfg(feature = "use_reth")]
pub fn update_kv(_: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::reth_adapter::update_kv(height, task_list);
}

#[cfg(feature = "use_lvmt")]
pub fn update_kv(_: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::lvmt_adapter::update_kv(height, task_list);
}

#[cfg(all(not(feature = "use_nomt"), not(feature = "use_reth"), not(feature = "use_lvmt")))]
pub fn update_kv(table_id: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::qmdb::update_kv(table_id, height, task_list);
}

#[cfg(feature = "use_nomt")]
pub fn delete_kv(_: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::nomt_adapter::delete_kv(height, task_list);
}

#[cfg(feature = "use_reth")]
pub fn delete_kv(_: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::reth_adapter::delete_kv(height, task_list);
}

#[cfg(feature = "use_lvmt")]
pub fn delete_kv(_: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::lvmt_adapter::delete_kv(height, task_list);
}

#[cfg(all(not(feature = "use_nomt"), not(feature = "use_reth"), not(feature = "use_lvmt")))]
pub fn delete_kv(table_id: usize, height: i64, task_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::qmdb::delete_kv(table_id, height, task_list);
}

#[cfg(feature = "use_nomt")]
pub fn read_kv(_: usize, key_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::nomt_adapter::read_kv(key_list);
}

#[cfg(feature = "use_reth")]
pub fn read_kv(_: usize, key_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::reth_adapter::read_kv(key_list);
}

#[cfg(feature = "use_lvmt")]
pub fn read_kv(_: usize, key_list: Vec<RwLock<Option<SimpleTask>>>) {
    crate::speed::lvmt_adapter::read_kv(key_list);
}

#[cfg(all(not(feature = "use_nomt"), not(feature = "use_reth"), not(feature = "use_lvmt")))]
pub fn read_kv(_: usize, _: &Vec<[u8; 20]>) {
}

#[cfg(any(feature = "use_nomt", feature = "use_reth", feature = "use_lvmt"))]
pub fn flush(_: usize) {}

#[cfg(all(not(feature = "use_nomt"), not(feature = "use_reth"), not(feature = "use_lvmt")))]
pub fn flush(table_id: usize) {
    crate::speed::qmdb::flush(table_id);
}

// Prints the shard Merkle roots and their digest after flush (QMDB only)
#[cfg(any(feature = "use_nomt", feature = "use_reth", feature = "use_lvmt"))]
pub fn print_root_hashes(_: usize) {}

#[cfg(all(not(feature = "use_nomt"), not(feature = "use_reth"), not(feature = "use_lvmt")))]
pub fn print_root_hashes(table_id: usize) {
    let (h, roots) = crate::speed::qmdb::root_hashes(table_id);
    let hex = |r: &[u8; 32]| r.iter().map(|b| format!("{b:02x}")).collect::<String>();
    for (i, r) in roots.iter().enumerate() { println!("root shard={i} {}", hex(r)); }
    let mut all = Vec::new();
    for r in &roots { all.extend_from_slice(r); }
    let mut d = [0u8; 32];
    qmdb_common::utils::hasher::blake3_hash_inplace(&mut d, &all);
    println!("state_root_digest height={h} {}", hex(&d));
}
