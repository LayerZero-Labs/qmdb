use crate::utils::shortlist::ShortList;
use aes_gcm::Aes256Gcm;
use byteorder::{BigEndian, ByteOrder};
use hex;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;

use super::def::{get_key_pos, to_key_pos, L, PAGE_BYTES};
use super::file_reader::{decrypt_page, FileReader};
use super::file_writer::FileWriter;
use super::index_cache::IndexCacheMap;
use super::overlay::Overlay;
use super::ref_unit::RefUnit;
use super::tempfile::TempFile;
use qmdb_common::def::TAG_SIZE;

pub struct Unit {
    pub overlay: Overlay,
    cache_map: IndexCacheMap,
    pub start_pos: usize,                   // this unit starts here in 'ifof'
    pub end_pos: usize,                     // this unit ends here in 'ifof'
    pub first_k_list: (Vec<u64>, Vec<u32>), // an index for 'ifof'
    pub ifof: Arc<RwLock<TempFile>>,        // Immutable Flash Offload File
    pub ref_u: Option<Box<RefUnit>>,        // only used in test
    cipher: Arc<Option<Aes256Gcm>>,
}

impl Unit {
    pub fn new(ifof: Arc<RwLock<TempFile>>, cipher: Arc<Option<Aes256Gcm>>) -> Self {
        let mut res = Self {
            overlay: Overlay::new(),
            cache_map: IndexCacheMap::default(),
            start_pos: 0,
            end_pos: 0,
            first_k_list: (Vec::new(), Vec::new()),
            ifof,
            ref_u: None,
            cipher,
        };
        if cfg!(feature = "check_hybridindexer") {
            res.ref_u = Some(Box::new(RefUnit::new()));
        }
        res
    }

    pub fn merge(&mut self, f_rd: &mut FileReader, f_wr: &mut FileWriter, _idx: usize) {
        self.first_k_list.0.clear();
        self.first_k_list.1.clear();
        f_rd.load_file(
            self.ifof.clone(),
            self.start_pos,
            self.end_pos,
            &self.overlay,
        );
        //if idx==0x44d1 || idx==0x0193 || idx==0x00ab || idx==0x004b {
        //    let guard = self.ifof.read();
        //    println!("NOTE idx={:#08x} load_file {} start={} end={}", idx, guard.get_name(), self.start_pos, self.end_pos);
        //}
        self.start_pos = f_wr.get_file_size();
        //if idx==0x44d1 || idx==0x0193 || idx==0x00ab || idx==0x004b {
        //    println!("NOTE idx={:#08x} write_file {} start={}", idx, f_wr.get_name(), self.start_pos);
        //}
        for &(k, pos) in self.overlay.new_kv.iter() {
            let key_pos = to_key_pos(k, pos);
            //if idx==0x44d1 || k==0x6983c9d87fe60000 {
            //    println!("HERE idx={:#08x} k={:#016x} ov:{}", idx, k, hex::encode(&key_pos));
            //}
            // get all the records less than key_pos
            loop {
                if let Some(a) = f_rd.cur_record() {
                    let (k_rd, pos_rd) = get_key_pos(&a[..]);
                    if (k_rd, pos_rd) < (k, pos) {
                        //if idx==0x44d1 || k_rd==0x6983c9d87fe60000 {
                        //    println!("HERE-rd idx={:#08x} {}", idx, hex::encode(&a));
                        //}
                        f_wr.write(a, &mut self.first_k_list);
                        f_rd.next(&self.overlay);
                        continue;
                    }
                }
                break;
            }
            //if idx==0x44d1 || k==0x6983c9d87fe60000 {
            //    println!("HERE-ov idx={:#08x} {}", idx, hex::encode(&key_pos));
            //}
            f_wr.write(&key_pos, &mut self.first_k_list);
        }
        // flush the remaining records in f_rd
        loop {
            if let Some(a) = f_rd.cur_record() {
                //let (k_rd, pos_rd) = get_key_pos(&a[..]);
                //if idx==0x44d1 || k_rd==0x6983c9d87fe60000 {
                //    println!("HERE-rd idx={:#08x} {} k_rd={:#016x} pos_rd={:#016x}", idx, hex::encode(&a), k_rd, pos_rd);
                //}
                f_wr.write(a, &mut self.first_k_list);
                f_rd.next(&self.overlay);
                continue;
            }
            break;
        }
        f_wr.flush();
        self.end_pos = f_wr.get_file_size();
        //if idx==0x44d1 || idx==0x0193 || idx==0x00ab || idx==0x004b {
        //    println!("NOTE idx={:#08x} write_file {} end={}", idx, f_wr.get_name(), self.end_pos);
        //}
        self.overlay.clear();
        self.cache_map.clear();
        self.ifof = f_wr.get_tmp_file();
        self.first_k_list.0.shrink_to_fit();
        self.first_k_list.1.shrink_to_fit();
    }

    pub fn for_each_value<F>(
        &mut self,
        height: i64,
        warmup: bool,
        k: u64,
        k80: &[u8],
        mut access: F,
    ) where
        F: FnMut(i64) -> bool,
    {
        let mut rv = if cfg!(feature = "check_hybridindexer") {
            Some(self.ref_u.as_ref().unwrap().debug_get_values(0, k))
        } else {
            None
        };
        //if rv.is_some() {
        //    let rset = rv.as_ref().unwrap();
        //    //println!("RSET BEGIN");
        //    for v in rset.iter() {
        //        //println!("RSET k64={} v={:#016x}", hex::encode(k64arr), v);
        //        //if access(*v) {
        //        //    break;
        //        //}
        //    }
        //    //println!("RSET END");
        //}
        let mut list = match self.cache_map.lookup_without_prev(height, warmup, k) {
            Some(l) => l, //cache hit
            None => self.get_from_file(height, k, false).1,
        };
        // add values in overlay.new_kv to list
        self.overlay.collect_values(k, &mut list);
        let mut stop_accessing = false;
        for (_, v) in list.enumerate() {
            //println!("NOW k={:#016x} v={:#016x}", k, v);
            if self.overlay.is_erased_value(v) {
                continue;
            }
            if let Some(rset) = rv.as_mut() {
                if !rset.remove(&v) {
                    panic!(
                        "NOT_IN_REF_FE k80={} k={:#016x} v={:#016x}",
                        hex::encode(k80),
                        k,
                        v
                    );
                }
            }
            if !stop_accessing {
                let stop = access(v);
                if stop {
                    stop_accessing = true;
                }
            }
        }
        if let Some(rset) = rv {
            if !rset.is_empty() {
                for v in rset.iter() {
                    println!("missed k80={} v={:#016x}", hex::encode(k80), v);
                }
                panic!(
                    "missed some postions k80={} k={:#016x}",
                    hex::encode(k80),
                    k
                );
            }
        }
    }

    fn access_list<F>(
        &self,
        list: &ShortList,
        k: u64,
        prev_k: u64,
        k80: &mut [u8],
        mut access: F,
        rv: &mut Option<HashSet<([u8; 10], i64)>>,
    ) -> bool
    where
        F: FnMut(&[u8], i64) -> bool,
    {
        let mut not_in_ref_fea = false;
        let mut stop_accessing = false;
        for (_, v) in list.enumerate() {
            if (v >> 48) == 0 {
                // bit#49 == 0 means this key's value
                BigEndian::write_u64(&mut k80[2..], k);
            } else {
                // bit#49 == 1 means prev key's value
                BigEndian::write_u64(&mut k80[2..], prev_k);
            }
            //if k80[0]==0x63 && k80[1]==0xd4 {
            //    println!("ACCESS k={:#016x} prev_k={:#016x} k80={} v={:#016x}", k, prev_k, hex::encode(&k80), v);
            //}
            let only_bit49_zero = !(1i64 << 48);
            let v = v & only_bit49_zero; //clear bit#49
            if self.overlay.is_erased_value(v) {
                continue;
            }
            if let Some(rset) = rv.as_mut() {
                let mut k80arr = [0u8; 10];
                k80arr[..].copy_from_slice(&k80[..10]);
                if !rset.remove(&(k80arr, v)) {
                    not_in_ref_fea = true;
                    println!(
                        "NOT_IN_REF_FEA k80={} k={:#016x} v={:#016x}",
                        hex::encode(&k80[..]),
                        k,
                        v
                    );
                }
            }
            if !stop_accessing {
                let stop = access(k80, v);
                if stop {
                    stop_accessing = true;
                }
            }
        }
        not_in_ref_fea
    }

    pub fn for_each_adjacent_value<F>(
        &mut self,
        height: i64,
        warmup: bool,
        k: u64,
        k80: &[u8; 10],
        mut access: F,
    ) where
        F: FnMut(&[u8], i64) -> bool,
    {
        let mut rv = if cfg!(feature = "check_hybridindexer") {
            Some(
                self.ref_u
                    .as_ref()
                    .unwrap()
                    .debug_get_adjacent_values(k80, 0, k),
            )
        } else {
            None
        };
        let (mut prev, mut list) = match self.cache_map.lookup_with_prev(height, warmup, k) {
            Some(pair) => pair,
            None => self.get_from_file(height, k, true),
        };
        let mut prev_all_erased = true;
        for (_, mut v) in list.enumerate() {
            if v >> 48 == 0 {
                continue; //not prev
            }
            v = (v << 16) >> 16;
            if !self.overlay.is_erased_value(v) {
                prev_all_erased = false;
            }
        }
        //if k80[0]==0x63 && k80[1]==0xd4 {
        //    println!("OVL k={:#016x}", k);
        //}
        //let old_prev = prev;
        //let mut _new_prev = u64::MAX;
        //let list0 = list.clone();
        if let Some((new_prev, mut new_list)) = self.overlay.get_prev_values(k) {
            //_new_prev = new_prev;
            //if k==0x163f6beca3200000 {
            //    println!("OVL new-prev={:#016x}", new_prev);
            //    for (_, v) in new_list.enumerate() {
            //        println!("OVL v={:#016x}", v);
            //    }
            //}
            if prev <= new_prev || prev_all_erased {
                // new_prev is closer to k, so we use 'new_list' to replace 'list'
                for (_, pos) in list.enumerate() {
                    if pos >> 48 == 0 {
                        //this bucket
                        new_list.append(pos);
                    } else if prev == new_prev && !prev_all_erased {
                        //prev bucket
                        new_list.append(pos);
                    }
                }
                prev = new_prev;
                list = new_list;
            } else {
                // new_prev is farther from k, so we ignore it
            }
        }
        //let list1 = list.clone();
        // add current bucket in overlay.new_kv to list
        self.overlay.collect_values(k, &mut list);
        //let list2 = list.clone();
        let mut buf = *k80;
        let _ = self.access_list(
            &list,
            k,
            prev,
            &mut buf[..],
            |k, pos| access(k, pos),
            &mut rv,
        );
        //if not_in {
        //    println!("NOT_IN old_prev={:#016x} new_prev={:#016x}", old_prev, _new_prev);
        //    for (_, v) in list0.enumerate() {
        //        println!("0: v={:#016x}", v);
        //    }
        //    for (_, v) in list1.enumerate() {
        //        println!("1: v={:#016x}", v);
        //    }
        //    for (_, v) in list2.enumerate() {
        //        println!("2: v={:#016x}", v);
        //    }
        //}

        if let Some(rset) = rv {
            if !rset.is_empty() {
                for (k, v) in rset.iter() {
                    println!(
                        "missed k80={} k={} v={:#016x}",
                        hex::encode(k80),
                        hex::encode(k),
                        v
                    );
                }
                panic!("missed some postions");
            }
        }
    }

    fn get_from_file(&mut self, height: i64, k: u64, get_prev: bool) -> (u64, ShortList) {
        let mut page = [0u8; PAGE_BYTES];
        let mask = 1i64 << 48;
        let mut page_id = self.find_page_id(k);
        let mut rd_count = self.read_page(page_id, &mut page[..]);
        let (mut at_prev, mut prev, mut list) = (false, 0, ShortList::new());
        'outer: loop {
            for i in (0..rd_count).rev() {
                let start = i * L;
                let (k_rd, file_off) = get_key_pos(&page[start..start + L]);
                if k_rd == k {
                    list.append(file_off);
                }
                if k_rd < k && !at_prev {
                    if !get_prev {
                        break 'outer;
                    }
                    at_prev = true;
                    prev = k_rd;
                }
                if at_prev && k_rd == prev {
                    list.append(mask | file_off);
                }
                if at_prev && k_rd < prev {
                    break 'outer;
                }
            }
            if page_id == 0 {
                break;
            }
            page_id -= 1;
            rd_count = self.read_page(page_id, &mut page[..]);
        }
        self.cache_map.add(height, k, prev, &list);
        (prev, list)
    }

    fn find_page_id(&self, k: u64) -> usize {
        let id64 = self.find_page_id_k64(k);
        let mut page_id = id64 * 4;
        let base = id64 * 3;
        let list32 = &self.first_k_list.1;
        let end = usize::min(3, list32.len() - base);
        for sub_idx in 0..end {
            if (k >> 32) as u32 >= list32[base + sub_idx] {
                page_id += 1;
            }
        }
        page_id
    }

    fn find_page_id_k64(&self, k: u64) -> usize {
        let list64 = &self.first_k_list.0;
        match list64.binary_search(&k) {
            Ok(match_idx) => {
                let mut id = match_idx;
                loop {
                    if id + 1 < list64.len() && list64[id + 1] == k {
                        id += 1;
                        continue;
                    }
                    return id;
                }
            }
            Err(next_idx) => {
                if next_idx == 0 {
                    panic!("Missing sentry entry for this unit");
                }
                next_idx - 1
            }
        }
    }

    fn read_page(&self, page_id: usize, page: &mut [u8]) -> usize {
        let f = self.ifof.read();
        let start = self.start_pos + page_id * PAGE_BYTES;
        let length = usize::min(PAGE_BYTES, self.end_pos - start);
        f.read_at(&mut page[..length], start as u64);
        decrypt_page(&mut page[..length], start, &self.cipher);
        if cfg!(feature = "tee_cipher") {
            return (length - TAG_SIZE) / L;
        }
        length / L
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "tee_cipher"))]
    #[test]
    fn test_unit() {
        let file_r = Arc::new(RwLock::new(TempFile::new("test_unit_read.tmp".to_string())));
        let file_w = Arc::new(RwLock::new(TempFile::new(
            "test_unit_write.tmp".to_string(),
        )));

        let mut writer = FileWriter::new(file_r.clone(), Arc::new(None));
        let mut first_k_list = (Vec::new(), Vec::new());

        let e1 = [1u8; L];
        let e2 = [2u8; L];

        writer.write(&e1, &mut first_k_list);
        writer.write(&e2, &mut first_k_list);
        writer.flush();

        let mut reader = FileReader::new(Arc::new(None));
        let mut writer = FileWriter::new(file_w.clone(), Arc::new(None));
        // build unit
        let mut unit = Unit::new(file_r.clone(), Arc::new(None));
        unit.ifof = file_r.clone();
        unit.start_pos = 0;
        unit.end_pos = 28;
        unit.overlay.add_kv(0, 10);
        unit.overlay.add_kv(1, 11);

        unit.merge(&mut reader, &mut writer, 0);
        assert_eq!(unit.end_pos, 4 * L);
        assert_eq!(unit.start_pos, 0);

        let overlay = Overlay::new();
        reader.load_file(file_w.clone(), 0, 4 * L, &overlay);
        let overlay = Overlay::new();
        reader.next(&overlay);
        reader.next(&overlay);
        reader.next(&overlay);
        assert_eq!(reader.cur_record().unwrap(), &e2);
        let mut found = false;
        unit.for_each_value(1, false, 1, &[], |i| {
            if i == 11 {
                found = true;
                return true;
            }
            false
        });
        assert!(found);

        let mut found = false;
        let mut k80 = [0u8; 10];
        unit.for_each_adjacent_value(1, false, 1, &mut k80, |k, pos| {
            if pos == 10 {
                found = true;
                println!("k:{k:?}");
                return true;
            }
            false
        });
        assert!(found);
    }
}
