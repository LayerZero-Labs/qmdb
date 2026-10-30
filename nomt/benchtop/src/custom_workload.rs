use crate::{
    backend::Transaction,
    cli::StateItemDistribution,
    workload::{Distribution, Workload},
};
use rand::Rng;

#[derive(Clone)]
pub struct RwInit {
    cur_val: u64,
    num_vals: u64,
}

impl Workload for RwInit {
    fn run_step(&mut self, transaction: &mut dyn Transaction) {
        const MAX_INIT_PER_ITERATION: u64 = 64 * 1024 * 1024;

        if self.num_vals == 0 {
            return;
        }

        let count = std::cmp::min(self.num_vals - self.cur_val, MAX_INIT_PER_ITERATION);
        for _ in 0..count {
            transaction.write(&encode_id(self.cur_val), Some(&[64u8; 32]));
            self.cur_val += 1;
        }
        println!(
            "populating {:.1}%",
            100.0 * (self.cur_val as f64) / (self.num_vals as f64)
        );
    }

    fn is_done(&self) -> bool {
        self.num_vals == self.cur_val
    }
}

/// Greate a workload for initializing a database with the given amount of key-value pairs.
pub fn init(db_size: u64) -> RwInit {
    RwInit {
        cur_val: 0,
        num_vals: db_size,
    }
}

fn encode_id(id: u64) -> [u8; 8] {
    id.to_be_bytes()
}

/// Build N `RwWorkload`s, one for each thread.
pub fn build(
    reads: u8,
    writes: u8,
    workload_size: u64,
    fresh: u8,
    db_size: u64,
    op_limit: u64,
    threads: usize,
    distribution: StateItemDistribution,
) -> Vec<RwWorkload> {
    let thread_workload_size = workload_size / threads as u64;
    let db_step = db_size / threads as u64;

    (0..threads)
        .map(|i| {
            let db_start = db_step * i as u64;

            RwWorkload {
                reads,
                writes,
                fresh,
                workload_size: if i == threads - 1 {
                    thread_workload_size + workload_size % threads as u64
                } else {
                    thread_workload_size
                },
                ops_remaining: op_limit / threads as u64,
                distribution: Distribution::new(distribution, db_start, db_start + db_step),
            }
        })
        .collect()
}

// The read-write workload will follow these rules:
// 1. Reads and writes are randomly and uniformly distributed across the key space.
// 2. The DB size indicates the number of entries in the database.
// 3. The workload size represents the total number of operations, where reads and writes
//     are numbers that need to sum to 100 and represent a percentage of the total size.
// 4. Fresh indicates the percentage of reads and writes that will be performed on
//     non-existing keys
pub struct RwWorkload {
    pub reads: u8,
    pub writes: u8,
    pub workload_size: u64,
    pub fresh: u8,
    pub ops_remaining: u64,
    pub distribution: Distribution,
}

impl Workload for RwWorkload {
    fn run_step(&mut self, transaction: &mut dyn Transaction) {
        let from_percentage = |p: u8| (self.workload_size as f64 * p as f64 / 100.0) as u64;
        let fresh = |size: u64| (size as f64 * self.fresh as f64 / 100.0) as u64;

        // total reads and writes
        let n_reads = from_percentage(self.reads);
        let n_writes = from_percentage(self.writes);
        // fresh reads and writes
        let n_reads_fresh = fresh(n_reads);
        let n_writes_fresh = fresh(n_writes);

        let mut rng = rand::thread_rng();

        for i in 0..n_reads {
            let _ = if i < n_reads_fresh {
                // fresh read, technically there is a chance to generate
                // a random key that is already present in the database,
                // but it is very unlikely
                transaction.read(&rand_key(&mut rng))
            } else {
                // read already existing key
                let key = self.distribution.sample(&mut rng);
                transaction.read(&encode_id(key))
            };
        }

        for i in 0..n_writes {
            let value = rand_key(&mut rng);
            if i < n_writes_fresh {
                // fresh write
                transaction.write(&rand_key(&mut rng), Some(&value));
            } else {
                // substitute key
                let key = self.distribution.sample(&mut rng);
                transaction.write(&encode_id(key), Some(&value));
            };
        }

        self.ops_remaining = self.ops_remaining.saturating_sub(self.workload_size);
    }

    fn is_done(&self) -> bool {
        self.ops_remaining == 0
    }
}

fn rand_key(rng: &mut impl Rng) -> [u8; 32] {
    // keys must be uniformly distributed
    let mut key = [0; 32];
    rng.fill(&mut key[..16]);
    key
}
