use asb_options::Options;
use asb_profile::{Profiler, Reporter};
use asb_tasks::{Event, Events, TaskTrait};
use authdb::AuthDB;
use fs_extra::dir::CopyOptions;
use kvdb::IoStatsKind;
use std::fs;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

fn warmup(db: &mut dyn AuthDB, tasks: Box<dyn Iterator<Item = Events> + '_>, opts: &Options) {
    let time = Instant::now();

    for (epoch, events) in tasks.enumerate() {
        // Collect all writes for this epoch, sort by key, then write in sorted order
        let mut writes: Vec<(Vec<u8>, Vec<u8>)> = events.0.into_iter()
            .filter_map(|e| if let Event::Write(k, v) = e { Some((k, v)) } else { None })
            .collect();
        writes.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));

        for (key, value) in writes {
            db.set(key, value);
        }
        db.commit(epoch);
        if (epoch + 1) % opts.report_epoch == 0 {
            println!(
                "Time {:>7.3?}s, Warming up epoch: {:>5}",
                time.elapsed().as_secs_f64(),
                epoch + 1
            );
        }
    }

    db.flush_all();
    if let Some(backend) = db.backend() {
        backend.io_stats(IoStatsKind::SincePrevious);
    }
}

pub fn run_tasks(
    mut db: Box<dyn AuthDB>,
    tasks: Arc<dyn TaskTrait>,
    mut reporter: Reporter,
    opts: &Options,
) {
    println!("Start warming up");
    if opts.warmup_from.is_none() && !opts.no_warmup {
        warmup(&mut *db, tasks.warmup(), opts);
        if let Some(ref warmup_dir) = opts.warmup_to() {
            println!("Waiting for post ops");

            sleep(Duration::from_secs_f64(f64::max(
                1.0,
                opts.total_keys as f64 / 1e6,
            )));

            let _ = fs::remove_dir_all(warmup_dir);
            fs::create_dir_all(warmup_dir).unwrap();

            let mut copy_options = CopyOptions::new();
            copy_options.overwrite = true;
            copy_options.copy_inside = true;
            copy_options.content_only = true;
            println!("Writing warmup to {}", warmup_dir);
            let mut retry_cnt = 0usize;
            while retry_cnt < 10 {
                if let Err(e) = fs_extra::dir::copy(&opts.db_dir, warmup_dir, &copy_options) {
                    println!("Fail to save warmup file {:?}. Retry...", e);
                    retry_cnt += 1;
                } else {
                    println!("Writing done");
                    return;
                }
            }

            panic!("Retry limit exceeds!");
        }
    }
    println!("Warm up done");

    let frequency = if opts.report_dir.is_none() { -1 } else { 250 };
    let mut profiler = Profiler::new(frequency);
    reporter.start();

    // Per-epoch timing (replaces reporter.notify_epoch)
    const TIMING_PRINT_INTERVAL: usize = 10;
    let mut acc_get_us: u64 = 0;
    let mut acc_set_us: u64 = 0;
    let mut acc_commit_us: u64 = 0;
    let mut acc_epochs: usize = 0;
    let start = Instant::now();

    for (epoch, events) in tasks.tasks().enumerate() {
        if reporter.start_time.elapsed().as_secs() >= opts.max_time.unwrap_or(u64::MAX)
            || epoch + 1 >= opts.max_epoch.unwrap_or(usize::MAX)
        {
            profiler.tick();
            break;
        }

        if (epoch + 1) % opts.profile_epoch == 0 {
            profiler.tick();
        }

        let mut read_count = 0;
        let mut write_count = 0;
        let mut t_get_us: u64 = 0;
        let mut t_set_us: u64 = 0;

        for event in events.0.into_iter() {
            match event {
                Event::Read(key) => {
                    read_count += 1;
                    let t = Instant::now();
                    let _ = db.get(key);
                    t_get_us += t.elapsed().as_micros() as u64;
                }
                Event::Write(key, value) => {
                    write_count += 1;
                    let t = Instant::now();
                    db.set(key, value);
                    t_set_us += t.elapsed().as_micros() as u64;
                }
            }
        }

        let t = Instant::now();
        db.commit(epoch);
        let t_commit_us = t.elapsed().as_micros() as u64;

        acc_get_us += t_get_us;
        acc_set_us += t_set_us;
        acc_commit_us += t_commit_us;
        acc_epochs += 1;

        if acc_epochs >= TIMING_PRINT_INTERVAL {
            println!(
                "[TIMING epoch={}] avg per epoch: get={}ms set={}ms commit={}ms | \
                 this_epoch: get={:.1}us/op set={:.1}us/op elapsed={:.1}s",
                epoch,
                acc_get_us / acc_epochs as u64 / 1000,
                acc_set_us / acc_epochs as u64 / 1000,
                acc_commit_us / acc_epochs as u64 / 1000,
                if read_count > 0 { t_get_us as f64 / read_count as f64 } else { 0.0 },
                if write_count > 0 { t_set_us as f64 / write_count as f64 } else { 0.0 },
                start.elapsed().as_secs_f64(),
            );
            acc_get_us = 0;
            acc_set_us = 0;
            acc_commit_us = 0;
            acc_epochs = 0;
        }
    }

    reporter.collect_profiling(profiler);
}
