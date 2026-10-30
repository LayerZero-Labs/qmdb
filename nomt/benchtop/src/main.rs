mod backend;
mod cli;
mod custom_workload;
mod nomt;

#[cfg(feature = "sov-db")]
mod sov_db;
#[cfg(feature = "sp-trie")]
mod sp_trie;

mod timer;
mod transfer_workload;
mod workload;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands, InitParams, RunParams};
use timer::Timer;

pub fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init(params) => init(params),
        Commands::Run(params) => run(params),
    }
}

pub fn init(params: InitParams) -> Result<()> {
    let workload_params = params.workload;
    let (mut init, _) = workload::parse(&workload_params, u64::max_value())?;

    let mut db = params.backend.instantiate(
        true,
        workload_params.commit_concurrency,
        workload_params.io_workers,
        workload_params.hashtable_buckets,
        workload_params.page_cache_size,
        workload_params.leaf_cache_size,
        workload_params.page_cache_upper_levels,
        workload_params.prepopulate_page_cache,
        0,
    );
    db.execute(None, &mut *init, None);

    Ok(())
}

pub fn run(params: RunParams) -> Result<()> {
    let workload_params = params.workload;
    let (mut init, mut workloads) = workload::parse(
        &workload_params,
        params.limits.ops.unwrap_or(u64::max_value()),
    )?;

    let mut db = params.backend.instantiate(
        params.reset,
        workload_params.commit_concurrency,
        workload_params.io_workers,
        workload_params.hashtable_buckets,
        workload_params.page_cache_size,
        workload_params.leaf_cache_size,
        workload_params.page_cache_upper_levels,
        workload_params.prepopulate_page_cache,
        workload_params.overlay_window_length,
    );

    if params.reset {
        db.execute(None, &mut *init, None);
    }

    let mut timer = Timer::new(format!("{}", params.backend));
    let warmup_timeout = params
        .warm_up
        .map(|time_limit| std::time::Instant::now() + time_limit.into());

    let thread_pool = rayon::ThreadPoolBuilder::new()
        .thread_name(|_| "benchtop-workload".into())
        .num_threads(workload_params.workload_concurrency as usize)
        .build()?;

    if let Some(t) = warmup_timeout {
        if workload_params.workload_concurrency == 1 {
            db.execute(Some(&mut timer), &mut *workloads[0], Some(t));
        } else {
            db.parallel_execute(Some(&mut timer), &thread_pool, &mut workloads, Some(t))?;
        };

        timer = Timer::new(format!("{}", params.backend));
    }

    let timeout = params
        .limits
        .time
        .map(|time_limit| std::time::Instant::now() + time_limit.into());

    if workload_params.workload_concurrency == 1 {
        db.execute(Some(&mut timer), &mut *workloads[0], timeout);
    } else {
        db.parallel_execute(Some(&mut timer), &thread_pool, &mut workloads, timeout)?;
    };

    db.print_metrics();
    timer.print(workload_params.size);
    print_max_rss();

    Ok(())
}

fn print_max_rss() {
    let max_rss = get_max_rss().unwrap_or(0);
    println!("max rss: {} MiB", max_rss / 1024);
    fn get_max_rss() -> Option<usize> {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        if ret == 0 {
            Some(usage.ru_maxrss as usize)
        } else {
            None
        }
    }
}
