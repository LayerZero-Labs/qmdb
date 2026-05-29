use crate::utils::shortlist::ShortList;
use std::collections::{BTreeSet, HashSet};
use std::ops::Bound;
use std::ops::Bound::{Excluded, Included};

type KV = (u64, i64);

pub struct Overlay {
    pub new_kv: BTreeSet<KV>,
    erased: HashSet<i64>,
}

impl Default for Overlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay {
    pub fn new() -> Self {
        Self {
            new_kv: BTreeSet::new(),
            erased: HashSet::new(),
        }
    }

    pub fn is_erased_value(&self, v_in: i64) -> bool {
        self.erased.contains(&v_in)
    }

    pub fn add_kv(&mut self, k_in: u64, v_in: i64) {
        //if k_in==0x6983c9d87fe60000 {
        //    println!("add_kv {:#016x} {:#016x}", k_in, v_in);
        //}
        let existed = !self.new_kv.insert((k_in, v_in));
        if existed {
            panic!("Add Duplicated KV: {k_in:?},{v_in:?}");
        }
    }

    pub fn erase_kv(&mut self, k_in: u64, v_in: i64) {
        //if k_in==0x6983c9d87fe60000 {
        //    println!("erase_kv {:#016x} {:#016x}", k_in, v_in);
        //}
        self.new_kv.remove(&(k_in, v_in));
        self.erased.insert(v_in);
    }

    pub fn change_kv(&mut self, k_in: u64, v_old: i64, v_new: i64) {
        //if k_in==0x6983c9d87fe60000 {
        //    println!("erase_kv {:#016x} {:#016x} {:#016x}", k_in, v_old, v_new);
        //}
        self.erase_kv(k_in, v_old);
        self.add_kv(k_in, v_new);
    }

    pub fn collect_values(&self, k_in: u64, list: &mut ShortList) {
        let start = (k_in, 0i64);
        let end = (k_in, i64::MAX);
        let range = (Included(&start), Included(&end));
        for &(_, v) in self.new_kv.range::<KV, (Bound<&KV>, Bound<&KV>)>(range) {
            list.append(v);
        }
    }

    pub fn get_prev_values(&self, k_in: u64) -> Option<(u64, ShortList)> {
        if self.new_kv.is_empty() {
            //if k_in==0xd7e7ba8a03050000 {
            //    println!("AA zero_len");
            //}
            return None;
        }
        let mut list = ShortList::new();
        let end = (k_in, 0i64);
        let start = (0u64, 0i64);
        let range = (Included(&start), Excluded(&end));
        let last_opt = self
            .new_kv
            .range::<KV, (Bound<&KV>, Bound<&KV>)>(range)
            .next_back();
        last_opt?;
        let (last_k, _) = last_opt.unwrap();
        //if k_in==0xd7e7ba8a03050000 {
        //    println!("AA last_k={:#016x}", *last_k);
        //}
        for &(k, v) in self
            .new_kv
            .range::<KV, (Bound<&KV>, Bound<&KV>)>(range)
            .rev()
        {
            //if k_in==0xd7e7ba8a03050000 {
            //    println!("AA k={:#016x} v={:#016x}", k, v);
            //}
            if *last_k == k {
                let v = v | (1i64 << 48); //bit#49 == 1 means prev key's value
                list.append(v);
            } else {
                break;
            }
        }
        Some((*last_k, list))
    }

    pub fn clear(&mut self) {
        self.new_kv.clear();
        self.erased.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overlay() {
        let mut overlay = Overlay::new();
        overlay.add_kv(0, 10);
        overlay.add_kv(2, 12);
        overlay.add_kv(1, 11);
        // add
        let prev = overlay.get_prev_values(2);
        assert!(prev.is_some());
        let (prev_key, short_list) = prev.unwrap();
        assert_eq!(prev_key, 1);
        assert!(short_list.contains(11 | (1 << 48)));
        // change
        overlay.change_kv(1, 11, 111);
        let prev = overlay.get_prev_values(2);
        assert!(prev.is_some());
        let (prev_key, short_list) = prev.unwrap();
        assert_eq!(prev_key, 1);
        assert!(short_list.contains(111 | (1 << 48)));
        // erase
        overlay.erase_kv(1, 111);
        let prev = overlay.get_prev_values(2);
        assert!(prev.is_some());
        let (prev_key, short_list) = prev.unwrap();
        assert_eq!(prev_key, 0);
        assert!(short_list.contains(10 | (1 << 48)));
        assert!(overlay.is_erased_value(111));
        // collect
        overlay.add_kv(2, 122);
        let mut list = ShortList::new();
        overlay.collect_values(2, &mut list);
        assert_eq!(list.len(), 2);
        assert!(list.contains(12));
        assert!(list.contains(122));
        // clear
        overlay.clear();
        let prev = overlay.get_prev_values(2);
        assert!(prev.is_none());
    }
}
