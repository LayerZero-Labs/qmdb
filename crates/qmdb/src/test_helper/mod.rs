pub mod cipher;
pub mod randsrc;
pub mod testgen;
use crate::config::Config;
use crate::metadb::MetaInfo;
use crate::tasks::TasksManager;
use crate::{tasks, AdsCore, AdsWrap, ADS};
use byteorder::{BigEndian, ByteOrder};
use parking_lot::RwLock;
use qmdb_common::def::{SHARD_COUNT, TAG_SIZE};
use qmdb_common::task::taskid::join_task_id;
use qmdb_common::utils::changeset::ChangeSet;
use std::fs::File;
use std::io::Read;
use std::sync::Arc;

pub use randsrc::RandSrc;
pub use tasks::helpers::SimpleTask;

pub fn to_k80(k64: u64) -> [u8; 10] {
    let mut k80 = [0u8; 10];
    BigEndian::write_u64(&mut k80, k64);
    k80
}

fn read_metainfo_from_file(path: &str) -> Result<MetaInfo, String> {
    let mut f = File::open(path).map_err(|e| format!("open {}: {}", path, e))?;
    let mut bz = Vec::new();
    f.read_to_end(&mut bz)
        .map_err(|e| format!("read {}: {}", path, e))?;

    // Try plain bincode first
    if let Ok(mi) = bincode::deserialize::<MetaInfo>(&bz[..]) {
        return Ok(mi);
    }

    // If file looks like encrypted envelope (nonce + payload + tag), try inner slice
    if bz.len() >= (8 + TAG_SIZE) {
        let inner = &bz[8..bz.len() - TAG_SIZE];
        if let Ok(mi) = bincode::deserialize::<MetaInfo>(inner) {
            return Ok(mi);
        }
    }

    Err(format!(
        "failed to decode MetaInfo from {} (unsupported format or encrypted)",
        path
    ))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

pub fn print_metainfo_side_by_side(file_a: &str, file_b: &str) {
    let a = match read_metainfo_from_file(file_a) {
        Ok(v) => v,
        Err(e) => {
            println!("{}", e);
            return;
        }
    };
    let b = match read_metainfo_from_file(file_b) {
        Ok(v) => v,
        Err(e) => {
            println!("{}", e);
            return;
        }
    };
    let header_field = "Field";
    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        header_field, file_a, "cmp", file_b
    );
    println!("{}", "-".repeat(28 + 3 + 48 + 3 + 4 + 3 + 48));

    compare_metainfo(&a, &b);
}

pub fn compare_metainfo(a: &MetaInfo, b: &MetaInfo) {
    // Helpers for arrays
    let fmt_u64_arr = |arr: &[u64; SHARD_COUNT]| format!("{:?}", arr);
    let fmt_i64_arr = |arr: &[i64; SHARD_COUNT]| format!("{:?}", arr);
    let fmt_tuple_arr = |arr: &[(u64, i64); SHARD_COUNT]| format!("{:?}", arr);
    let fmt_root_hash = |arr: &[[u8; 32]; SHARD_COUNT]| {
        let v: Vec<String> = arr.iter().map(|h| to_hex(h)).collect();
        format!("[{}]", v.join(", "))
    };
    let fmt_root_hash_hist = |v: &Vec<[u8; 32]>| {
        if v.is_empty() {
            "len=0".to_owned()
        } else {
            let last_hex = to_hex(&v[v.len() - 1]);
            format!("len={}, last={}", v.len(), last_hex)
        }
    };
    let fmt_edge_nodes = |arr: &[Vec<u8>; SHARD_COUNT]| {
        let parts: Vec<String> = arr
            .iter()
            .map(|v| {
                if v.is_empty() {
                    "len=0".to_owned()
                } else {
                    let preview_len = v.len().min(16);
                    let head_hex = to_hex(&v[..preview_len]);
                    if v.len() > preview_len {
                        format!("len={}, head={}...", v.len(), head_hex)
                    } else {
                        format!("len={}, head={}", v.len(), head_hex)
                    }
                }
            })
            .collect();
        format!("[{}]", parts.join(", "))
    };

    let marker = |eq: bool| if eq { "==" } else { "!=" };

    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        "curr_height",
        a.curr_height,
        marker(a.curr_height == b.curr_height),
        b.curr_height
    );
    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        "last_pruned_twig",
        fmt_tuple_arr(&a.last_pruned_twig),
        marker(a.last_pruned_twig == b.last_pruned_twig),
        fmt_tuple_arr(&b.last_pruned_twig)
    );
    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        "next_serial_num",
        fmt_u64_arr(&a.next_serial_num),
        marker(a.next_serial_num == b.next_serial_num),
        fmt_u64_arr(&b.next_serial_num)
    );
    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        "oldest_active_sn",
        fmt_u64_arr(&a.oldest_active_sn),
        marker(a.oldest_active_sn == b.oldest_active_sn),
        fmt_u64_arr(&b.oldest_active_sn)
    );
    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        "oldest_active_file_pos",
        fmt_i64_arr(&a.oldest_active_file_pos),
        marker(a.oldest_active_file_pos == b.oldest_active_file_pos),
        fmt_i64_arr(&b.oldest_active_file_pos)
    );
    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        "twig_file_sizes",
        fmt_i64_arr(&a.twig_file_sizes),
        marker(a.twig_file_sizes == b.twig_file_sizes),
        fmt_i64_arr(&b.twig_file_sizes)
    );
    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        "entry_file_sizes",
        fmt_i64_arr(&a.entry_file_sizes),
        marker(a.entry_file_sizes == b.entry_file_sizes),
        fmt_i64_arr(&b.entry_file_sizes)
    );
    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        "first_twig_at_height",
        fmt_tuple_arr(&a.first_twig_at_height),
        marker(a.first_twig_at_height == b.first_twig_at_height),
        fmt_tuple_arr(&b.first_twig_at_height)
    );
    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        "root_hash",
        fmt_root_hash(&a.root_hash),
        marker(a.root_hash == b.root_hash),
        fmt_root_hash(&b.root_hash)
    );
    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        "root_hash_by_height",
        fmt_root_hash_hist(&a.root_hash_by_height),
        marker(a.root_hash_by_height == b.root_hash_by_height),
        fmt_root_hash_hist(&b.root_hash_by_height)
    );
    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        "edge_nodes",
        fmt_edge_nodes(&a.edge_nodes),
        marker(a.edge_nodes == b.edge_nodes),
        fmt_edge_nodes(&b.edge_nodes)
    );
    println!(
        "{:<28} | {:<48} | {:^4} | {}",
        "extra_data",
        a.extra_data,
        marker(a.extra_data == b.extra_data),
        b.extra_data
    );
}

pub fn setup_qmdb(path: &str, init_changesets: Vec<ChangeSet>) -> AdsWrap<SimpleTask> {
    let mut config = Config::from_dir(path);
    config.with_twig_file = true;
    AdsCore::init_dir(&config);

    let mut ads = AdsWrap::<SimpleTask>::new(&config);

    write_changesets_to_qmdb(&mut ads, init_changesets);
    ads
}

pub fn write_changesets_to_qmdb(ads: &mut AdsWrap<SimpleTask>, changesets: Vec<ChangeSet>) {
    let task = SimpleTask::new(changesets);
    let task_list = vec![RwLock::new(Some(task))];
    let tasks_manager = Arc::new(TasksManager::new(task_list));
    let height = ads.get_metadb().read().get_curr_height() + 1;

    let (success, _meta) = ads.start_block(height, tasks_manager);
    assert!(success);

    let shared_ads = ads.get_shared();
    shared_ads.insert_extra_data(height, "".to_owned());
    let task_id = join_task_id(height, 0, true);
    shared_ads.add_task(task_id);
    ads.flush();
}
