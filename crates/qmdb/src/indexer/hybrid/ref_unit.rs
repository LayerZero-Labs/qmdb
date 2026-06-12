use byteorder::{BigEndian, ByteOrder};
use std::collections::{BTreeSet, HashSet};
use std::ops::Bound;
use std::ops::Bound::{Excluded, Included};

const MAX_POS: i64 = 0xFFFF_FFFF_FFFF;

pub struct RefUnit {
    bt: BTreeSet<u128>,
}

// 16b-idx, 64b-k, 48b-v
fn get_u128(idx: u64, k_in: u64, v_in: i64) -> u128 {
    let hi = (idx << 48) | (k_in >> 16);
    let lo = (k_in << 48) | (v_in as u64);
    // using hi&lo to build res will not cause rust-nightly's bug
    let res = ((hi as u128) << 64) | (lo as u128);
    res
}

fn get_pos(x: u128) -> i64 {
    (((x as u64) << 16) >> 16) as i64
}

fn get_k(x: u128) -> u64 {
    let hi = (x >> 64) as u64;
    let lo = x as u64;
    // using hi&lo will not cause rust-nightly's bug
    (hi << 16) | (lo >> 48)
}

// fn get_idx(x: u128) -> u64 {
//     (x >> 112) as u64
// }

impl Default for RefUnit {
    fn default() -> Self {
        Self::new()
    }
}

impl RefUnit {
    pub fn new() -> Self {
        Self {
            bt: BTreeSet::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.bt.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bt.is_empty()
    }

    pub fn add_kv(&mut self, idx: u64, k_in: u64, v_in: i64) {
        //if idx == 0xccc8 {
        //    println!("HH.add {:#032x} {:#016x} {:#016x}", get_u128(idx, k_in, v_in), k_in, v_in);
        //}
        let existed = !self.bt.insert(get_u128(idx, k_in, v_in));
        if existed {
            panic!("Add Duplicated KV");
        }
    }

    pub fn erase_kv(&mut self, idx: u64, k_in: u64, v_in: i64) {
        let existed = self.bt.remove(&get_u128(idx, k_in, v_in));
        if !existed {
            panic!("Cannot Erase Non-existent KV {idx:#08x} {k_in:#016x} {v_in:#016x}");
        }
    }

    pub fn change_kv(&mut self, idx: u64, k_in: u64, v_old: i64, v_new: i64) {
        self.erase_kv(idx, k_in, v_old);
        self.add_kv(idx, k_in, v_new);
    }

    pub fn for_each_value<F>(&self, idx: u64, k_in: u64, mut access: F)
    where
        F: FnMut(i64) -> bool,
    {
        let start = get_u128(idx, k_in, 0);
        let end = get_u128(idx, k_in, MAX_POS);
        let range = (Included(&start), Included(&end));
        for &v in self.bt.range::<u128, (Bound<&u128>, Bound<&u128>)>(range) {
            let pos = ((v as u64) << 16) >> 16;
            if access(pos as i64) {
                break;
            }
        }
    }

    pub fn for_each_adjacent_value<F>(&self, idx: u64, k_in: u64, mut access: F)
    where
        F: FnMut(u64, i64) -> bool,
    {
        let tree = &self.bt;
        if tree.is_empty() {
            return;
        }
        let mut start = get_u128(idx, k_in, 0);
        let mut end = get_u128(idx, k_in, MAX_POS);
        //println!("JJ k_in={:#018x} {:#034x} -> {:#034x}", k_in, start, end);
        let mut range = (Included(&start), Included(&end));
        for &v in tree.range::<u128, (Bound<&u128>, Bound<&u128>)>(range) {
            if access(k_in, get_pos(v)) {
                return;
            }
        }
        end = start;
        start = get_u128(idx, 0, 0);
        //println!("JJ2 {:#034x} -> {:#034x}", start, end);
        range = (Included(&start), Excluded(&end));
        let last = tree
            .range::<u128, (Bound<&u128>, Bound<&u128>)>(range)
            .next_back();
        if let Some(last_elem) = last {
            let k = (last_elem >> 48) as u64;
            for &elem in tree
                .range::<u128, (Bound<&u128>, Bound<&u128>)>(range)
                .rev()
            {
                if get_k(elem) == k {
                    if access(k, get_pos(elem)) {
                        return;
                    }
                } else {
                    break;
                }
            }
        }
    }

    pub fn key_exists(&self, idx: u64, k_in: u64, file_pos: i64) -> bool {
        let mut exists = false;
        self.for_each_value(idx, k_in, |offset| -> bool {
            if offset == file_pos {
                exists = true;
            }
            false // do not exit loop
        });
        exists
    }

    pub fn debug_get_kv(&self, idx: u64, k_in: u64) -> (u64, i64) {
        let tree = &self.bt;
        if tree.is_empty() {
            panic!("Empty tree");
        }
        let start = get_u128(idx, 0, 0);
        let end = get_u128(idx, k_in, MAX_POS);
        //println!("JJ k_in={:#018x} {:#034x} -> {:#034x}", k_in, start, end);
        let range = (Included(&start), Included(&end));
        let last = tree
            .range::<u128, (Bound<&u128>, Bound<&u128>)>(range)
            .next_back();
        if let Some(elem) = last {
            //if get_idx(*elem) == 0xccc8 {
            //    println!("HH {:#034x} {:#018x} {:#018x}", *elem, get_k(*elem), get_pos(*elem));
            //}
            (get_k(*elem), get_pos(*elem))
        } else {
            panic!("get nothing in a non-empty tree");
        }
    }

    pub fn debug_get_values(&self, _idx: u64, k_in: u64) -> HashSet<i64> {
        let mut res = HashSet::new();
        self.for_each_value(0, k_in, |v| -> bool {
            res.insert(v);
            false // do not exit loop
        });
        res
    }

    pub fn debug_get_adjacent(&self, k80: &[u8], idx: u64, k_in: u64) -> HashSet<i64> {
        let mut res = HashSet::new();
        self.for_each_adjacent_value(idx, k_in, |k, v| -> bool {
            //println!("JJ2 {:#018x}", v);
            res.insert(v);
            false // do not exit loop
        });
        res
    }

    pub fn debug_get_adjacent_values(
        &self,
        k80: &[u8],
        idx: u64,
        k_in: u64,
    ) -> HashSet<([u8; 10], i64)> {
        let mut res = HashSet::new();
        self.for_each_adjacent_value(idx, k_in, |k, v| -> bool {
            let mut k80arr = [0u8; 10];
            k80arr[..].copy_from_slice(&k80[..10]);
            BigEndian::write_u64(&mut k80arr[2..], k);
            BigEndian::write_u32(&mut k80arr[6..], 0u32); // clear last 32 bits
            res.insert((k80arr, v));
            false // do not exit loop
        });
        res
    }
}
