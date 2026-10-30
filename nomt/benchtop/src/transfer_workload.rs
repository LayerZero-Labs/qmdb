use crate::{
    backend::Transaction,
    cli::StateItemDistribution,
    workload::{Distribution, Workload},
};
use rand::Rng;

#[derive(Clone)]
pub struct TransferInit {
    cur_account: u64,
    num_accounts: u64,
}

impl Workload for TransferInit {
    fn run_step(&mut self, transaction: &mut dyn Transaction) {
        const MAX_INIT_PER_ITERATION: u64 = 64 * 1024;

        if self.num_accounts == 0 {
            return;
        }

        let count = std::cmp::min(self.num_accounts - self.cur_account, MAX_INIT_PER_ITERATION);
        for _ in 0..count {
            transaction.write(&encode_id(self.cur_account), Some(&encode_balance(1000)));
            self.cur_account += 1;
        }
        println!(
            "populating {:.1}%",
            100.0 * (self.cur_account as f64) / (self.num_accounts as f64)
        );
    }

    fn is_done(&self) -> bool {
        self.cur_account == self.num_accounts
    }
}

/// Create an initialization command for a transfer database.
pub fn init(num_accounts: u64) -> TransferInit {
    TransferInit {
        cur_account: 0,
        num_accounts,
    }
}

fn encode_id(id: u64) -> [u8; 8] {
    id.to_be_bytes()
}

fn encode_balance(balance: u64) -> [u8; 8] {
    balance.to_be_bytes()
}

fn decode_balance(encoded: &[u8]) -> u64 {
    let mut buf = [0; 8];
    buf.copy_from_slice(encoded);
    u64::from_be_bytes(buf)
}

/// Build a new workload meant to emulate transfers.
///
/// `num_accounts` refers to the amount of accounts in the database.
///
/// `percentage_cold_transfer` ranges from 0 to 100 and indicates the proportion of transfers
/// which should be sent to a fresh account.
pub fn build(
    num_accounts: u64,
    workload_size: u64,
    percentage_cold_transfer: u8,
    op_limit: u64,
    threads: usize,
    distribution: StateItemDistribution,
) -> Vec<TransferWorkload> {
    let thread_workload_size = workload_size / threads as u64;
    let num_accounts_step = num_accounts / threads as u64;

    (0..threads)
        .map(|i| {
            let start_account = num_accounts_step * i as u64;
            let end_account = if i == threads - 1 {
                num_accounts
            } else {
                num_accounts_step * (i as u64 + 1)
            };
            TransferWorkload {
                num_accounts,
                workload_size: thread_workload_size,
                percentage_cold_transfer,
                ops_remaining: op_limit / threads as u64,
                distribution: Distribution::new(distribution, start_account, end_account),
            }
        })
        .collect()
}

/// A transfer-like workload.
pub struct TransferWorkload {
    /// The number of accounts in the system.
    pub num_accounts: u64,
    /// The size of the workload.
    pub workload_size: u64,
    /// The percentage of transfers to make to fresh accounts.
    pub percentage_cold_transfer: u8,
    /// The number of remaining operations before being considered 'done'.
    pub ops_remaining: u64,
    /// The random distribution to use to sample state items.
    pub distribution: Distribution,
}

impl Workload for TransferWorkload {
    fn run_step(&mut self, transaction: &mut dyn Transaction) {
        let cold_sends =
            (self.workload_size as f64 * (self.percentage_cold_transfer as f64 / 100.0)) as u64;
        let warm_sends = self.workload_size - cold_sends;

        let mut rng = rand::thread_rng();
        for i in 0..self.workload_size {
            let send_account = self.distribution.sample(&mut rng);
            let recv_account = if i < warm_sends {
                let mut r = self.distribution.sample(&mut rng);
                while r == send_account {
                    r = self.distribution.sample(&mut rng);
                }
                r
            } else {
                // odds of two threads generating the same random account here are
                // incredibly low.
                rng.gen_range(self.num_accounts..u64::max_value())
            };

            let send_balance = decode_balance(
                &transaction
                    .read(&encode_id(send_account))
                    .expect("account exists"),
            );
            let recv_balance = transaction
                .read(&encode_id(recv_account))
                .map_or(0, |v| decode_balance(&v));

            let new_send_balance = if send_balance == 0 {
                1000 // yay, free money.
            } else {
                send_balance - 1
            };
            let new_recv_balance = recv_balance + 1;

            transaction.write(
                &encode_id(send_account),
                Some(&encode_balance(new_send_balance)),
            );
            transaction.write(
                &encode_id(recv_account),
                Some(&encode_balance(new_recv_balance)),
            );
        }

        self.ops_remaining = self.ops_remaining.saturating_sub(self.workload_size);
    }

    fn is_done(&self) -> bool {
        self.ops_remaining == 0
    }
}
