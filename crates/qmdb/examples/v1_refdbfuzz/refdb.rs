use byteorder::{ByteOrder, LittleEndian};
use qmdb::config::Config;
use qmdb_common::def::{
    DEFAULT_ENTRY_SIZE, OP_CREATE, OP_DELETE, OP_WRITE, SENTRY_COUNT, SHARD_COUNT,
};
use qmdb_common::entryfile::entry::{entry_key_hash, entry_to_vec, sentry_entry, Entry, EntryBz};
use qmdb_common::task::taskid::{join_task_id, split_task_id};
use qmdb_common::utils::{byte0_to_shard_id, OpRecord};
use rocksdb::{Direction, IteratorMode, WriteBatch, WriteBatchWithTransaction, WriteOptions, DB};
use std::path::Path;
use std::sync::mpsc::SyncSender;

const KH2E: u8 = 1; // key_hash to entry
const SN2KH: u8 = 2; // serial_number to key_hash
const META: u8 = 3; //meta info (update per block)
const H2C: u8 = 4; // hash to code

pub struct RefDB {
    curr_version: i64,
    oldest_active_sn_arr: [u64; SHARD_COUNT],
    // the youngest active sn is: next_sn_arr[shard_id] - 1
    next_sn_arr: [u64; SHARD_COUNT],
    // number of active entries
    num_active_arr: [usize; SHARD_COUNT],
    db: DB,
    pub utilization_div: i64,
    pub utilization_ratio: i64,
    pub compact_thres: i64,
}

impl RefDB {
    pub fn new(name: &str, config: &Config) -> Self {
        let path = Path::new(&config.dir).join(name.to_owned() + ".db");
        let mut res = RefDB {
            curr_version: join_task_id(1, 0, false),
            oldest_active_sn_arr: [0; SHARD_COUNT],
            next_sn_arr: [SENTRY_COUNT as u64; SHARD_COUNT],
            num_active_arr: [SENTRY_COUNT; SHARD_COUNT],
            db: DB::open_default(path).unwrap(),
            utilization_div: config.utilization_div,
            utilization_ratio: config.utilization_ratio,
            compact_thres: config.compact_thres,
        };
        if let Some(meta_bz) = res.raw_get(&[META]) {
            res.load_meta_info(meta_bz);
        } else {
            res.init_db();
        }
        res
    }

    fn init_db(&self) {
        // initialize an empty DB with sentry entries
        let mut batch = WriteBatch::default();
        let mut bz = [0u8; DEFAULT_ENTRY_SIZE];
        for shard_id in 0..SHARD_COUNT {
            for sn in 0..SENTRY_COUNT {
                let e = sentry_entry(shard_id, sn as u64, &mut bz[..]);
                add_entry(&mut batch, &e.key_hash(), &e);
            }
        }
        self.write_batch_sync(batch);
    }

    fn write_batch_sync(&self, batch: WriteBatch) {
        let mut write_options = WriteOptions::default();
        write_options.set_sync(true);
        self.db.write_opt(batch, &write_options).unwrap();
    }

    fn load_meta_info(&mut self, meta_bz: Vec<u8>) {
        // if we can find meta info, this DB is not empty
        if meta_bz.len() != 8 + 8 * 3 * SHARD_COUNT {
            panic!("invalid meta info");
        }
        let mut start = 0;
        self.curr_version = LittleEndian::read_i64(&meta_bz[start..start + 8]) + 1;
        start += 8;
        for i in 0..SHARD_COUNT {
            self.oldest_active_sn_arr[i] = LittleEndian::read_u64(&meta_bz[start..start + 8]);
            start += 8;
            self.next_sn_arr[i] = LittleEndian::read_u64(&meta_bz[start..start + 8]);
            start += 8;
            self.num_active_arr[i] = LittleEndian::read_u64(&meta_bz[start..start + 8]) as usize;
            start += 8;
        }
    }

    // get code from a H2C record
    pub fn get_code(&self, prefix: &[u8]) -> Option<(i64, Vec<u8>, Vec<u8>)> {
        let mut k = [0u8; 33];
        k[0] = H2C;
        k[1..prefix.len()].copy_from_slice(prefix);
        let iter = self
            .db
            .iterator(IteratorMode::From(&k[..], Direction::Forward));
        for item in iter {
            let (key, value) = item.unwrap();
            if key[0] == H2C {
                let version = LittleEndian::read_i64(&value[..8]);
                let bytecode = value[8..].to_vec();
                return Some((version, key.to_vec(), bytecode));
            } else {
                break;
            }
        }
        None
    }

    // get an entry from a KH2E record
    pub fn get_entry(&self, key_hash: &[u8; 32]) -> Option<Vec<u8>> {
        let mut k = [0u8; 33];
        k[0] = KH2E;
        k[1..].copy_from_slice(&key_hash[..]);
        self.raw_get(&k)
    }

    // remove an entry that may or may not be existent
    pub fn remove_entry<K: AsRef<[u8]>>(&mut self, key: K) -> Option<OpRecord> {
        let key_hash = entry_key_hash(key.as_ref());
        if let Some(old) = self.get_entry(&key_hash) {
            let mut op_rec = OpRecord::new(OP_DELETE);
            op_rec.key.extend_from_slice(key.as_ref());
            self.delete_kv(&key_hash, old, &mut op_rec);
            return Some(op_rec);
        }
        None // do nothing when removing a non-existent entry
    }

    // create a new entry or update an existent entry
    pub fn set_entry<K: AsRef<[u8]>>(&mut self, key: K, value: &[u8]) -> OpRecord {
        let key_hash = entry_key_hash(key.as_ref());
        let mut op_rec;
        if let Some(old) = self.get_entry(&key_hash) {
            op_rec = OpRecord::new(OP_WRITE);
            self.write_kv(&key_hash, value, old, &mut op_rec);
        } else {
            op_rec = OpRecord::new(OP_CREATE);
            self.create_kv(key.as_ref(), &key_hash, value, &mut op_rec);
        }
        op_rec
    }

    pub fn read_all_entries(&self, out: SyncSender<Vec<u8>>) {
        let end_hash = [0xFFu8; 32];
        let iter = self
            .db
            .iterator(IteratorMode::From(&[KH2E], Direction::Forward));
        for item in iter {
            let (key, value) = item.unwrap();
            if key.len() < 32 || key[3..].iter().all(|&x| x == 0) {
                continue;
            }
            let curr_entry = EntryBz { bz: &value[..] };
            let mut curr_hash = curr_entry.key_hash();
            assert_eq!(curr_hash, &key[1..], "incorrect key_hash");
            self.check_entry(&curr_entry);
            assert_eq!(curr_hash, curr_entry.key_hash(), "incorrect key_hash");
            curr_hash[..].copy_from_slice(curr_entry.next_key_hash());
            out.send(value.to_vec()).unwrap();
            if curr_hash == end_hash {
                break;
            }
        }
    }

    // flush the meta info to DB when ending a block
    pub fn end_block(&mut self) {
        let meta_info = self.encode_meta_info();
        let mut batch = WriteBatch::default();
        batch.put(vec![META], &meta_info);
        self.write_batch_sync(batch);

        let height = split_task_id(self.curr_version).0;
        self.curr_version = join_task_id(height + 1, 0, false);
    }

    fn encode_meta_info(&self) -> Vec<u8> {
        let mut bz = [0u8; 8 + 8 * 3 * SHARD_COUNT];
        let mut start = 0;
        LittleEndian::write_i64(&mut bz[start..start + 8], self.curr_version);
        start += 8;
        for i in 0..SHARD_COUNT {
            LittleEndian::write_u64(&mut bz[start..start + 8], self.oldest_active_sn_arr[i]);
            start += 8;
            LittleEndian::write_u64(&mut bz[start..start + 8], self.next_sn_arr[i]);
            start += 8;
            LittleEndian::write_u64(&mut bz[start..start + 8], self.num_active_arr[i] as u64);
            start += 8;
        }

        Vec::from(bz)
    }

    pub fn end_tx(&mut self) {
        self.curr_version += 1;
    }

    pub fn get_num_active(&self, shard_id: usize) -> usize {
        self.num_active_arr[shard_id]
    }

    pub fn total_num_active(&self) -> usize {
        let mut count = 0;
        for shard_id in 0..SHARD_COUNT {
            count += self.num_active_arr[shard_id];
        }
        count
    }

    // =============== the private methods ===========

    fn raw_get(&self, key: &[u8]) -> Option<Vec<u8>> {
        if key.is_empty() {
            panic!("Empty Key")
        }
        self.db.get(key).unwrap()
    }

    fn check_entry(&self, entry_bz: &EntryBz) {
        let shard_id = byte0_to_shard_id(entry_bz.key_hash()[0]);
        let kh = self
            .get_key_hash(entry_bz.serial_number(), shard_id)
            .unwrap();
        assert_eq!(entry_bz.key_hash(), kh, "incorrect sn->key_hash");
        for (_, sn) in entry_bz.dsn_iter() {
            let option = self.get_key_hash(sn, shard_id);
            assert!(option.is_none(), "sn is not deactived");
        }
    }

    // get key_hash from a SN2KH record
    fn get_key_hash(&self, sn: u64, shard_id: usize) -> Option<[u8; 32]> {
        let mut k = [0u8; 10];
        k[0] = SN2KH;
        k[1] = shard_id as u8;
        LittleEndian::write_u64(&mut k[2..], sn);
        match self.raw_get(&k) {
            None => None,
            Some(v) => {
                let mut arr = [0u8; 32];
                arr[..].copy_from_slice(&v[..]);
                Some(arr)
            }
        }
    }

    // get the previous entry from the previous KH2E record
    fn get_prev_entry(&self, key_hash: &[u8; 32]) -> ([u8; 32], Vec<u8>) {
        let mut k = [0u8; 33];
        k[0] = KH2E;
        k[1..].copy_from_slice(&key_hash[..]);
        let iter = self
            .db
            .iterator(IteratorMode::From(&k[..], Direction::Reverse));
        let mut prev_key_hash = [0u8; 32];
        for item in iter {
            let (key, value) = item.unwrap();
            prev_key_hash.copy_from_slice(&key[1..]);
            if prev_key_hash == *key_hash {
                continue; //ignore myself
            }
            return (prev_key_hash, value.to_vec());
        }
        panic!("prev_entry not found");
    }

    // remove the entry 'old'
    fn delete_kv(&mut self, key_hash: &[u8; 32], old: Vec<u8>, op_rec: &mut OpRecord) {
        let old_entry = EntryBz { bz: &old[..] };
        let shard_id = byte0_to_shard_id(key_hash[0]);
        op_rec.num_active = self.num_active_arr[shard_id];
        op_rec.oldest_active_sn = self.oldest_active_sn_arr[shard_id];
        let (prev_key_hash, prev_e_bz) = self.get_prev_entry(key_hash);
        let prev_entry = EntryBz { bz: &prev_e_bz[..] };
        let prev_changed = Entry {
            key: prev_entry.key(),
            value: prev_entry.value(),
            // the old next_key_hash is old_entry.key_hash()
            next_key_hash: old_entry.next_key_hash(),
            version: self.curr_version,
            serial_number: self.next_sn_arr[shard_id],
        };
        self.next_sn_arr[shard_id] += 1;
        let dsn_list = &mut vec![old_entry.serial_number(), prev_entry.serial_number()];
        //if dsn_list[0] > dsn_list[1] {
        //    (dsn_list[0], dsn_list[1]) = (dsn_list[1], dsn_list[0]); //sort it
        //}
        #[allow(clippy::needless_borrow)]
        let prev_changed_bz = entry_to_vec(&prev_changed, &dsn_list);
        let e = EntryBz {
            bz: &prev_changed_bz[..],
        };
        let mut batch = WriteBatch::default();
        self.add_entry(&mut batch, &prev_key_hash, shard_id, &e);

        op_rec.shard_id = shard_id;
        op_rec.next_sn = self.next_sn_arr[shard_id];
        op_rec.rd_list.push(prev_e_bz);
        op_rec.rd_list.push(old);
        op_rec.wr_list.push(prev_changed_bz);

        remove_kh2e_record(&mut batch, key_hash);

        //println!("BB s{} remove next_sn={} num_active={}", op_rec.shard_id, op_rec.next_sn, self.num_active_arr[shard_id]);
        self.num_active_arr[shard_id] -= 1;
        self.try_twice_compact(&mut batch, shard_id, op_rec);
        self.write_batch_sync(batch);
    }

    // update an existent entry 'old': change its value to 'value'
    fn write_kv(&mut self, key_hash: &[u8; 32], value: &[u8], old: Vec<u8>, op_rec: &mut OpRecord) {
        let old_entry = EntryBz { bz: &old[..] };
        let shard_id = byte0_to_shard_id(key_hash[0]);
        op_rec.num_active = self.num_active_arr[shard_id];
        op_rec.oldest_active_sn = self.oldest_active_sn_arr[shard_id];
        let new_entry = Entry {
            key: old_entry.key(),
            value,
            next_key_hash: old_entry.next_key_hash(),
            version: self.curr_version,
            serial_number: self.next_sn_arr[shard_id],
        };
        self.next_sn_arr[shard_id] += 1;
        let new_e_bz = entry_to_vec(&new_entry, &[old_entry.serial_number()]);
        let new_e = EntryBz { bz: &new_e_bz[..] };
        let mut batch = WriteBatch::default();
        self.add_entry(&mut batch, key_hash, shard_id, &new_e);

        op_rec.shard_id = shard_id;
        op_rec.next_sn = self.next_sn_arr[shard_id];
        op_rec.key.extend_from_slice(new_entry.key);
        op_rec.value.extend_from_slice(new_entry.value);
        op_rec.rd_list.push(old);
        op_rec.wr_list.push(new_e_bz);

        self.write_batch_sync(batch);
        let mut batch = WriteBatch::default();

        self.try_twice_compact(&mut batch, shard_id, op_rec);
        self.write_batch_sync(batch);
    }

    // create a new entry with 'key' and 'value'
    fn create_kv(&mut self, key: &[u8], key_hash: &[u8; 32], value: &[u8], op_rec: &mut OpRecord) {
        let shard_id = byte0_to_shard_id(key_hash[0]);
        op_rec.num_active = self.num_active_arr[shard_id];
        op_rec.oldest_active_sn = self.oldest_active_sn_arr[shard_id];
        let (prev_key_hash, prev_e_bz) = self.get_prev_entry(key_hash);
        let prev_entry = EntryBz { bz: &prev_e_bz[..] };
        let new_entry = Entry {
            key,
            value,
            next_key_hash: prev_entry.next_key_hash(),
            version: self.curr_version,
            serial_number: self.next_sn_arr[shard_id],
        };
        let mut batch = WriteBatch::default();
        let new_entry_bz = entry_to_vec(&new_entry, &[]);
        let new_entry = EntryBz {
            bz: &new_entry_bz[..],
        };
        self.add_entry(&mut batch, key_hash, shard_id, &new_entry);

        let prev_changed = Entry {
            key: prev_entry.key(),
            value: prev_entry.value(),
            next_key_hash: &key_hash[..],
            version: self.curr_version,
            serial_number: self.next_sn_arr[shard_id] + 1,
        };
        self.next_sn_arr[shard_id] += 2;
        let prev_changed_bz = entry_to_vec(&prev_changed, &[prev_entry.serial_number()]);
        let prev_changed = EntryBz {
            bz: &prev_changed_bz[..],
        };
        self.add_entry(&mut batch, &prev_key_hash, shard_id, &prev_changed);

        self.write_batch_sync(batch);
        let mut batch = WriteBatch::default();

        op_rec.shard_id = shard_id;
        op_rec.next_sn = self.next_sn_arr[shard_id];
        op_rec.key.extend_from_slice(key);
        op_rec.value.extend_from_slice(value);
        op_rec.rd_list.push(prev_e_bz);
        op_rec.wr_list.push(prev_changed_bz);
        op_rec.wr_list.push(new_entry_bz);

        //println!("BB s{} create next_sn={} num_active={}", op_rec.shard_id, op_rec.next_sn, self.num_active_arr[shard_id]);
        self.num_active_arr[shard_id] += 1;
        self.try_twice_compact(&mut batch, shard_id, op_rec);
        self.write_batch_sync(batch);
    }

    // if the utilization ratio is too slow, it's compactible
    fn is_compactible(&self, shard_id: usize) -> bool {
        let active_count = self.num_active_arr[shard_id];
        let total_count = self.next_sn_arr[shard_id] - self.oldest_active_sn_arr[shard_id];

        //if total_count % 2000 == 0 {
        //    println!("AA Cmpt tot={} act={} {}-{} thres={}", total_count, active_count,
        //        total_count as i64 / self.utilization_div,
        //        active_count as i64 / self.utilization_ratio, self.compact_thres);
        //}
        if (total_count as i64 / self.utilization_div
            < active_count as i64 / self.utilization_ratio)
            || (active_count as i64) < self.compact_thres
        {
            return false; // no need to compact
        }
        true
    }

    fn try_twice_compact(
        &mut self,
        batch: &mut WriteBatchWithTransaction<false>,
        shard_id: usize,
        op_rec: &mut OpRecord,
    ) {
        if self.is_compactible(shard_id) {
            self.compact(batch, shard_id, op_rec);
            self.compact(batch, shard_id, op_rec);
        }
    }

    //deactivate and recreate an old active entry
    fn compact(
        &mut self,
        batch: &mut WriteBatchWithTransaction<false>,
        shard_id: usize,
        op_rec: &mut OpRecord,
    ) {
        //if total_count % 2000 == 0 {
        //    println!("AA Cmpt NOW!");
        //}
        let key_hash = self.get_oldest_active(shard_id);
        let old = self.get_entry(&key_hash).unwrap();
        let old_entry = EntryBz { bz: &old[..] };
        if shard_id == 1 {
            let _r = Entry::from_bz(&old_entry);
            //println!("BB s1 Cmpt old_E {:?}", r);
        }
        let new_entry = Entry {
            key: old_entry.key(),
            value: old_entry.value(),
            next_key_hash: old_entry.next_key_hash(),
            version: old_entry.version(),
            serial_number: self.next_sn_arr[shard_id],
        };
        self.next_sn_arr[shard_id] += 1;
        self.oldest_active_sn_arr[shard_id] = old_entry.serial_number() + 1;
        let e_bz = entry_to_vec(&new_entry, &[old_entry.serial_number()]);
        let e = EntryBz { bz: &e_bz[..] };
        self.add_entry(batch, &key_hash, shard_id, &e);

        op_rec.dig_list.push(old);
        op_rec.put_list.push(e_bz);
    }

    fn add_entry(
        &mut self,
        batch: &mut WriteBatchWithTransaction<false>,
        key_hash: &[u8; 32],
        _shard_id: usize,
        e: &EntryBz,
    ) {
        add_entry(batch, key_hash, e);

        //let mut last_sn: u64 = 0;
        //for i in 0..e.dsn_count() {
        //    let sn = e.get_deactived_sn(i);
        //    if i > 0 && last_sn >= sn {
        //        panic!("dsn_list is not sorted!");
        //    }
        //    last_sn = sn;
        //}
    }

    fn get_oldest_active(&mut self, shard_id: usize) -> [u8; 32] {
        let mut sn = self.oldest_active_sn_arr[shard_id];
        loop {
            if let Some(kh) = self.get_key_hash(sn, shard_id) {
                return kh;
            }
            sn += 1;
        }
    }
}

fn add_entry(batch: &mut WriteBatchWithTransaction<false>, key_hash: &[u8; 32], e: &EntryBz) {
    let shard_id = byte0_to_shard_id(key_hash[0]);
    let mut k = [0u8; 10];
    k[0] = SN2KH;
    k[1] = shard_id as u8;
    // delete the deactived SN2KH record
    for (_, sn) in e.dsn_iter() {
        LittleEndian::write_u64(&mut k[2..], sn);
        let mut _k = [0u8; 10];
        _k.copy_from_slice(&k[..]);
        batch.delete(_k);
    }
    // add a new SN2KH record for the new entry
    LittleEndian::write_u64(&mut k[2..], e.serial_number());
    batch.put(&k[..], &key_hash[..]);

    let mut k = [0u8; 33];
    k[0] = KH2E;
    k[1..].copy_from_slice(&key_hash[..]);
    // add a new KH2E record for the new entry
    batch.put(k, e.bz);
}

fn remove_kh2e_record(batch: &mut WriteBatchWithTransaction<false>, key_hash: &[u8; 32]) {
    let mut k = [0u8; 33];
    k[0] = KH2E;
    k[1..].copy_from_slice(&key_hash[..]);
    batch.delete(&k[..]);
}

fn main() {}

/*
#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::mpsc::sync_channel;

    fn setup_db() -> (RefDB, tempfile::TempDir) {
        let temp_dir = tempfile::Builder::new().prefix("tmp").tempdir().unwrap();
        let config = Config::from_dir(temp_dir.path().to_str().unwrap());
        let db = RefDB::new("testdb", &config);
        (db, temp_dir)
    }

    #[test]
    #[serial]
    fn test_new_db() {
        let (db, _temp_dir) = setup_db();
        assert_eq!(db.curr_version, 16777216);
        for i in 0..SHARD_COUNT {
            assert_eq!(db.oldest_active_sn_arr[i], 0);
            assert_eq!(db.next_sn_arr[i], SENTRY_COUNT as u64);
            assert_eq!(db.num_active_arr[i], SENTRY_COUNT);
        }
    }

    #[test]
    #[serial]
    fn test_meta_db() {
        let (mut db, _temp_dir) = setup_db();

        db.curr_version = 100;
        for i in 0..SHARD_COUNT {
            db.oldest_active_sn_arr[i] = 200 + i as u64;
            db.next_sn_arr[i] = 300 + i as u64;
            db.num_active_arr[i] = 400 + i;
        }
        db.end_block();

        let meta_info = db.raw_get(&[META]).unwrap();
        db.load_meta_info(meta_info);
        assert_eq!(101, db.curr_version);
        for i in 0..SHARD_COUNT {
            assert_eq!(200 + i as u64, db.oldest_active_sn_arr[i]);
            assert_eq!(300 + i as u64, db.next_sn_arr[i]);
            assert_eq!(400 + i, db.num_active_arr[i]);
        }
    }

    #[test]
    #[serial]
    fn test_set_entry() {
        let (mut db, _temp_dir) = setup_db();
        let key: &[u8; 7] = b"testkey";

        let value = b"testvalue";
        let op_record = db.set_entry(key, value.as_ref());
        assert_eq!(op_record.op_type, OP_CREATE);
        let entry = db.get_entry(&hash(key)).unwrap();
        let entry_bz = EntryBz { bz: &entry[..] };
        assert_eq!(entry_bz.value(), value);
        assert_eq!(db.oldest_active_sn_arr[9], 0);
        assert_eq!(db.next_sn_arr[9], SENTRY_COUNT as u64 + 2);
        assert_eq!(db.num_active_arr[9], SENTRY_COUNT + 1);

        // update
        let value = b"testvalue1";
        let op_record = db.set_entry(key, value.as_ref());
        assert_eq!(op_record.op_type, OP_WRITE);
        let entry = db.get_entry(&hash(key)).unwrap();
        let entry_bz = EntryBz { bz: &entry[..] };
        assert_eq!(entry_bz.value(), value);
        assert_eq!(db.oldest_active_sn_arr[9], 0);
        assert_eq!(db.next_sn_arr[9], SENTRY_COUNT as u64 + 2 + 1);
        assert_eq!(db.num_active_arr[9], SENTRY_COUNT + 1);
    }

    #[test]
    #[serial]
    fn test_remove_entry() {
        let (mut db, _temp_dir) = setup_db();
        let key = b"testkey";
        let value = b"testvalue";
        db.set_entry(key, value.as_ref());

        let op_record = db.remove_entry(key).unwrap();
        assert_eq!(op_record.op_type, OP_DELETE);
        assert_eq!(db.oldest_active_sn_arr[9], 0);
        assert_eq!(db.next_sn_arr[9], SENTRY_COUNT as u64 + 2 + 1);
        assert_eq!(db.num_active_arr[9], SENTRY_COUNT);

        let entry = db.get_entry(&hash(key));
        assert!(entry.is_none());
    }

    #[test]
    #[serial]
    fn test_read_all_entries() {
        let (mut db, _temp_dir) = setup_db();
        let key = b"testkey";
        let value = b"testvalue";
        db.set_entry(key, value.as_ref());

        let key = b"testkey1";
        db.set_entry(key, value.as_ref());
        db.set_entry(key, value.as_ref());

        let (sender, receiver) = sync_channel(1000);
        db.read_all_entries(sender);
        let entries: Vec<Vec<u8>> = receiver.iter().collect();
        assert_eq!(entries.len(), 2);

        let key = b"testkey11111";
        db.set_entry(key, value.as_ref());

        let (sender, receiver) = sync_channel(1000);
        db.read_all_entries(sender);
        let entries: Vec<Vec<u8>> = receiver.iter().collect();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    #[serial]
    fn test_end_block() {
        let temp_dir = tempfile::Builder::new().prefix("tmp").tempdir().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap().to_string();
        {
            let config = Config::from_dir(&dir_path);
            let mut db = RefDB::new("testdb", &config);
            assert_eq!(db.curr_version, 16777216);
            db.end_block()
        }
        let config = Config::from_dir(&dir_path);
        let db = RefDB::new("testdb", &config);
        assert_eq!(db.curr_version, 16777217);
    }

    #[test]
    #[serial]
    fn test_compact() {
        let (mut db, _temp_dir) = setup_db();
        let value = b"testvalue";

        for i in 0..3740_usize {
            db.set_entry(i.to_ne_bytes(), value.as_ref());
        }

        assert_eq!(db.oldest_active_sn_arr[10], 0);
    }
}
 */
