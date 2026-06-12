use qmdb::indexer::memidx::{Unit4Test, UnitTrait};
use qmdb::utils::lfsr::GaloisLfsr;

const STEP_COUNT: usize = 1000;
const OP_COUNT: usize = 1000;

const FEV: u64 = 0;
const FEA: u64 = 1; //a=adjacent
const FEB: u64 = 2; //b=both
const DEL: u64 = 3;
const CHG: u64 = 4;
const ADD: u64 = 5;

fn main() {
    for seed in 1000..1100 {
        println!("=== seed:{seed}");
        run(seed);
    }
}

fn run(seed: u64) {
    let mut dut = Unit4Test::new();
    let mut lfsr = GaloisLfsr::new(seed, 63);
    dut.add_kv(0, 0); //sentry
    for _i in 0..STEP_COUNT {
        //println!("step {} size {}", i, dut.u.len());
        for _ in 0..OP_COUNT {
            step(&mut dut, &mut lfsr);
        }
        dut.compact();
    }
}

fn num_to_k80(num: u64) -> [u8; 10] {
    let mut res = [0u8; 10];
    let bz = num.to_be_bytes();
    res[2..].copy_from_slice(&bz[..]);
    res
}

fn step(dut: &mut Unit4Test, lfsr: &mut GaloisLfsr) {
    let rand_op = lfsr.rand_n(32) % 7; // 6 means equal add/del
    let k_in = lfsr.rand_n(62) << 2;
    //println!("rand op{} k_in {:#016x} leadzero={}", rand_op, k_in, k_in.leading_zeros());
    let v = lfsr.rand_n(48) as i64;
    match rand_op {
        DEL => {
            let (k, v) = dut.debug_get_kv(k_in);
            if k != 0 {
                //println!("DEL k={:#016x} v={:#016x}", k, v);
                dut.erase_kv(k, v);
            }
        }
        CHG => {
            let (k, v_old) = dut.debug_get_kv(k_in);
            //println!("CHG k={:#016x} v={:#016x}->{:#016x}", k, v_old, v);
            dut.change_kv(k, v_old, v);
        }
        FEV => {
            let (k, _v) = dut.debug_get_kv(k_in);
            //println!("FEV k={:#016x}", k);
            dut.for_each_value(k, |_| false);
        }
        FEA => {
            if k_in != 0 {
                let k80 = num_to_k80(k_in);
                //println!("FEA k={:#016x}", k_in);
                dut.for_each_adjacent_value(k80, k_in, |_| false);
            }
        }
        FEB => {
            let (k, _v) = dut.debug_get_kv(k_in);
            if k != 0 {
                let k80 = num_to_k80(k);
                //println!("FEB k={:#016x}", k);
                dut.for_each_adjacent_value(k80, k, |_| false);
            }
        }
        // ADD is default
        _ => {
            let (k, _v) = dut.debug_get_kv(k_in);
            if k_in != k {
                //println!("ADD k={:#016x} v={:#016x}", k_in, v);
                dut.add_kv(k_in, v);
            }
        }
    }
}
