use fs_extra::dir::CopyOptions;
use std::fs;

mod run;

use asb_options::{AuthAlgo, Backend, Options, StructOpt};
use run::run_tasks;

fn main() {
    let options: Options = Options::from_args();
    if options.stat_mem && !options.no_stat {
        panic!("Stat will introduce memory cost")
    }
    if options.algorithm == AuthAlgo::LMPTS && options.backend != Backend::RocksDB {
        panic!("LMPTs can not change backend")
    }
    println!(
        "Testing {:?} with {}",
        options.algorithm,
        if options.real_trace {
            "real trace".into()
        } else {
            format!("{:e} addresses", options.total_keys)
        }
    );

    let db_dir = &options.db_dir;
    let _ = fs::remove_dir_all(db_dir);
    fs::create_dir_all(db_dir).unwrap();

    if let Some(ref warmup_dir) = options.warmup_from() {
        println!("warmup from {}", warmup_dir);
        let mut options = CopyOptions::new();
        options.content_only = true;
        fs_extra::dir::copy(warmup_dir, db_dir, &options).unwrap();
    }

    if let Some(ref dir) = options.report_dir {
        fs::create_dir_all(dir).unwrap()
    }

    let tasks = asb_tasks::tasks(&options);
    let backend = asb_backend::backend(&options);
    let (db, reporter) = asb_authdb::new(backend, &options);
    run_tasks(db, tasks, reporter, &options);
}
