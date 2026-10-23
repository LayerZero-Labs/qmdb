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
pub fn init(db_dir: &str, _: usize, start_from_existing: bool, _: bool, _: u64) {
    let path = Path::new(db_dir).join("NOMT");
    crate::speed::nomt_adapter::init(path.to_str().unwrap(), start_from_existing);
}

#[cfg(feature = "use_reth")]
pub fn init(db_dir: &str, _: usize, start_from_existing: bool, is_eth_mode: bool, entry_count: u64) {
    let path = Path::new(db_dir).join("RETH");
    crate::speed::reth_adapter::init(path.to_str().unwrap(), start_from_existing, is_eth_mode, entry_count);
}

#[cfg(feature = "use_lvmt")]
pub fn init(db_dir: &str, _: usize, _start_from_existing: bool, _: bool, _: u64) {
    let path = Path::new(db_dir).join("LVMT");
    crate::speed::lvmt_adapter::init(path.to_str().unwrap(), false);
}

#[cfg(all(not(feature = "use_nomt"), not(feature = "use_reth"), not(feature = "use_lvmt")))]
pub fn init(db_dir: &str, table_id: usize, _: bool, _: bool, _: u64) {
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
    // update 和 create 在 NOMT 中通常都是通过 Session.write 处理
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

#[cfg(feature = "use_reth")]
pub fn warmup_trie() {
    crate::speed::reth_adapter::warmup_trie();
}