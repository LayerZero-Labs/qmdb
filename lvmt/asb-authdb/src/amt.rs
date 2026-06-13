use std::sync::Arc;

use kvdb::KeyValueDB;

use lvmt_db::{lvmt_db::cached_pp_with_depth, single_amt::AmtDB};

use asb_options::Options;
use authdb::AuthDB;

pub struct Amt<const N: usize> {
    amt: AmtDB<N>,
    print_root_period: Option<usize>,
}

pub fn new<const N: usize>(backend: Arc<dyn KeyValueDB>, opts: &Options) -> Amt<N> {
    let pp = cached_pp_with_depth("./pp", N);
    pp.warm_quotient();
    let shard_info = opts.shards.map(|size| (size.trailing_zeros() as usize, 0));
    Amt {
        amt: AmtDB::new(backend, pp, shard_info),
        print_root_period: if opts.print_root {
            Some(opts.report_epoch)
        } else {
            None
        },
    }
}

impl<const N: usize> AuthDB for Amt<N> {
    fn get(&self, key: Vec<u8>) -> Option<Box<[u8]>> {
        // println!("read");
        self.amt.get(&key).map(Into::into)
    }

    fn set(&mut self, key: Vec<u8>, value: Vec<u8>) {
        // println!("write");
        self.amt.set(&key, value)
    }

    fn commit(&mut self, index: usize) {
        // println!("commit");
        let root = self.amt.commit();
        if let Some(period) = self.print_root_period {
            if index % period == 0 {
                println!("Commitment {:?}", root);
            }
        }
    }

    fn backend(&self) -> Option<&dyn KeyValueDB> {
        Some(&*self.amt.db)
    }

    fn flush_all(&mut self) {
        let _ = self.amt.commit();
    }
}

#[allow(unused)]
#[derive(Clone)]
pub struct AMTCounter {
    put_count: [u64; 4],
    inc_key_count: u64,
    inc_tree_count: u64,
    inc_key_level_count: u64,
}

impl Default for AMTCounter {
    fn default() -> Self {
        Self {
            put_count: [0; 4],
            inc_key_count: 0,
            inc_tree_count: 0,
            inc_key_level_count: 0,
        }
    }
}
