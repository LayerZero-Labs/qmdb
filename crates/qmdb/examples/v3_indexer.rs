use byteorder::{BigEndian, ByteOrder, LittleEndian};
use qmdb::indexer::hybrid::ref_unit::RefUnit;
use qmdb::indexer::hybrid::HybridIndexer;
use qmdb::indexer::memidx::InMemIndexer;
use qmdb::test_helper::RandSrc;
use qmdb_common::{
    def::{SHARD_COUNT, SHARD_DIV},
    indexer::indexer_trait::IndexerTrait,
};
use std::sync::Arc;

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const SALT: u64 = 12345;
const TOTAL: u64 = 1u64 << 26;
const HOVER_COUNT: u64 = 10;
const HOVER_RANGE: u64 = 500;
const ONLY_ADD_UNTIL: usize = 1 << 19;
const DONT_ADD_AFTER: usize = 1 << 24;
const HISTORY_SIZE: usize = 1000;

const FEV: u8 = 0;
const FEA: u8 = 1; //a=adjacent
const FEB: u8 = 2; //b=both
const DEL: u8 = 3;
const CHG: u8 = 4;
const ADD: u8 = 5;

type IndexerToTest = HybridIndexer;
//type IndexerToTest = InMemIndexer4Test;

#[allow(unreachable_code)]
fn main() {
    let file_name = "randsrc.dat";
    let mut randsrc = RandSrc::new(file_name, "qmdb-1");
    allocate_indexer_entries(&mut randsrc, 1u64 << 24);
    panic!("allocate_indexer_entries finished");
    let mut rfu = RefUnit::new();
    let hi_dir = "HI";
    let mut dut = IndexerToTest::with_dir(hi_dir.to_owned());
    init(&mut rfu, &mut dut);
    let hi_arc = Arc::new(dut);
    run(&mut randsrc, &mut rfu, &hi_arc);
}

fn init(rfu: &mut RefUnit, dut: &mut IndexerToTest) {
    let mut k80 = [0u8; 10];
    // simulate the effect of adding sentry entries
    for shard_id in 0..SHARD_COUNT {
        let sn_start = (shard_id * SHARD_DIV) as u64;
        let sn_end = (shard_id * SHARD_DIV + SHARD_DIV) as u64;
        for sn in sn_start..sn_end {
            BigEndian::write_u16(&mut k80[..2], sn as u16);
            dut.add_kv(&k80[..], sn as i64 * 8, sn);
            rfu.add_kv(sn, 0, sn as i64 * 8);
        }
        //dut.dump_mem_to_file(shard_id);
    }
}

fn get_rand_op(randsrc: &mut RandSrc, rfu: &RefUnit) -> u8 {
    let count = rfu.len();
    if count < ONLY_ADD_UNTIL {
        // only add
        return ADD;
    }
    if count > DONT_ADD_AFTER {
        // no more ADD
        return (randsrc.get_uint32() % 5) as u8;
    }
    (randsrc.get_uint32() % 6) as u8
}

struct HistoryEntry {
    fe_vec: Vec<[u8; 10]>,
    diff_vec: Vec<[u8; 10]>,
}

fn run(randsrc: &mut RandSrc, rfu: &mut RefUnit, dut: &Arc<IndexerToTest>) {
    let mut sn = 1u64 << 16;
    let mut history = Vec::with_capacity(HISTORY_SIZE);
    for _ in 0..HISTORY_SIZE {
        history.push(HistoryEntry {
            fe_vec: Vec::<[u8; 10]>::new(),
            diff_vec: Vec::<[u8; 10]>::new(),
        });
    }
    for base in 0..TOTAL {
        if base % 4096 == 0 {
            println!("base={:#08x} total_count={:#08x}", base, rfu.len());
        }
        let height = base as i64;
        let idx = base as usize % history.len();
        let his_entry = history.get_mut(idx).unwrap();
        // run historical for-each-adjacent access again
        for k80 in his_entry.fe_vec.iter() {
            let _ = dut.for_each_adjacent_value(height, &k80[..]);
        }
        for k80 in his_entry.diff_vec.iter() {
            let _ = dut.for_each_value(height, &k80[..]);
            if BigEndian::read_u64(&k80[2..]) != 0 {
                let _ = dut.for_each_adjacent_value(height, &k80[..]);
            }
        }
        his_entry.fe_vec.clear();
        his_entry.diff_vec.clear();
        for _ in 0..HOVER_COUNT {
            let num = base + randsrc.get_uint32() as u64 % HOVER_RANGE;
            run_at(randsrc, his_entry, rfu, dut, num, &mut sn, height);
            for i in 0..SHARD_COUNT {
                dut.len(i);
            }
        }
    }
}

fn run_at(
    randsrc: &mut RandSrc,
    his_entry: &mut HistoryEntry,
    rfu: &mut RefUnit,
    dut: &Arc<IndexerToTest>,
    num: u64,
    sn: &mut u64,
    height: i64,
) {
    let mut k80 = num_to_k80(num);
    let rand_op = get_rand_op(randsrc, rfu);
    let idx = BigEndian::read_u16(&k80[..2]) as u64;
    let k_in = BigEndian::read_u64(&k80[2..]);
    match rand_op {
        ADD => {
            let (k, _v) = rfu.debug_get_kv(idx, k_in);
            if k_in != k {
                //if k80[0]==0xc3 && k80[1]==0xfa {
                //    println!("ADD k80={} k={:#016x} sn={:#016x}", hex::encode(&k80), k, sn);
                //}
                // k80 is a new key to add
                rfu.add_kv(idx, k_in, *sn as i64 * 8);
                dut.add_kv(&k80[..], *sn as i64 * 8, *sn);
                his_entry.diff_vec.push(k80);
                *sn += 1;
            }
        }
        DEL => {
            let (k, v) = rfu.debug_get_kv(idx, k_in);
            // move k80 to an existing non-sentry slot
            if k != 0 {
                BigEndian::write_u64(&mut k80[2..], k);
                //if k80[0]==0xc3 && k80[1]==0xfa {
                //    println!("DEL k80={} k={:#016x}", hex::encode(&k80), k);
                //}
                rfu.erase_kv(idx, k, v);
                dut.erase_kv(&k80[..], v, v as u64 / 8);
                his_entry.diff_vec.push(k80);
            }
        }
        CHG => {
            let (k, v) = rfu.debug_get_kv(idx, k_in);
            //if k80[0]==0xc3 && k80[1]==0xfa {
            //    println!("CHG initk80={}", hex::encode(&k80));
            //}
            // move k80 to an existing slot
            BigEndian::write_u64(&mut k80[2..], k);
            //if k80[0]==0xc3 && k80[1]==0xfa {
            //    println!("CHG k80={} k={:#016x} v_old={:#016x}", hex::encode(&k80), k, v);
            //}
            rfu.change_kv(idx, k, v, *sn as i64 * 8);
            dut.change_kv(&k80[..], v, *sn as i64 * 8, v as u64 / 8, *sn);
            his_entry.diff_vec.push(k80);
            *sn += 1;
        }
        FEV => {
            let (k, _) = rfu.debug_get_kv(idx, k_in);
            // move k80 to an existing slot
            //if k80[0]==0xc3 && k80[1]==0xfa {
            //    println!("FEV k80={} k={:#016x}", hex::encode(&k80), k);
            //}
            BigEndian::write_u64(&mut k80[2..], k);
            let _ = dut.for_each_value(height, &k80[..]);
        }
        FEA => {
            let k = BigEndian::read_u64(&k80[2..]);
            if k != 0 {
                //if k80[0]==0xc3 && k80[1]==0xfa {
                //    println!("FEA k80={} k={:#016x}", hex::encode(&k80), k);
                //}
                let _ = dut.for_each_adjacent_value(height, &k80[..]);
                // record historical for-each-adjacent access
                his_entry.fe_vec.push(k80);
            }
        }
        FEB => {
            let (k, _) = rfu.debug_get_kv(idx, k_in);
            // move k80 to an existing non-sentry slot
            if k != 0 {
                BigEndian::write_u64(&mut k80[2..], k);
                let _ = dut.for_each_adjacent_value(height, &k80[..]);
                // record historical for-each-adjacent access
                his_entry.fe_vec.push(k80);
            }
        }
        _ => {}
    }
}

fn num_to_k80(num: u64) -> [u8; 10] {
    let mut buf = [0u8; 16];
    LittleEndian::write_u64(&mut buf[..8], SALT);
    LittleEndian::write_u64(&mut buf[..8], num);
    let hash = qmdb_common::entryfile::entry::entry_key_hash(&buf[..]);
    let mut res = [0u8; 10];
    res[..].copy_from_slice(&hash[..10]);
    res
}

fn allocate_indexer_entries(_randsrc: &mut RandSrc, count: u64) {
    let mut buf = [0u8; 8];
    let indexer = Arc::new(InMemIndexer::new(256));
    //InMemIndexer::start_compacting(indexer.clone());
    let step = count / 1024;
    for i in 0..count {
        if i % step == 0 {
            println!("{}", i / step);
        }
        BigEndian::write_u64(&mut buf[..], i);
        let mut hash = qmdb_common::entryfile::entry::entry_key_hash(&buf[..]);
        hash[0] = 0; // because only 256 shards, not 65536
        indexer.add_kv(&hash[..], (i * 8) as i64, i);
    }
}
