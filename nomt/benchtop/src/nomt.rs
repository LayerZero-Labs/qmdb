use crate::{backend::Transaction, timer::Timer, workload::Workload};
use fxhash::FxHashMap;
use nomt::{
    hasher::Blake3Hasher, trie::KeyPath, KeyReadWrite, Nomt, Options, Overlay, Session,
    SessionParams, WitnessMode,
};
use sha2::Digest;
use std::{
    collections::{hash_map::Entry, VecDeque},
    sync::Mutex,
};

const NOMT_DB_FOLDER: &str = "nomt_db";

pub struct NomtDB {
    nomt: Nomt<Blake3Hasher>,
    overlay_window_capacity: usize,
    overlay_window: Mutex<VecDeque<Overlay>>,
}

impl NomtDB {
    pub fn open(
        reset: bool,
        commit_concurrency: usize,
        io_workers: usize,
        hashtable_buckets: Option<u32>,
        page_cache_size: Option<usize>,
        leaf_cache_size: Option<usize>,
        page_cache_upper_levels: usize,
        prepopulate_page_cache: bool,
        overlay_window_capacity: usize,
    ) -> Self {
        let nomt_db_folder =
            std::env::var("NOMT_DB_FOLDER").unwrap_or_else(|_| NOMT_DB_FOLDER.to_string());

        if reset {
            // Delete previously existing db
            let _ = std::fs::remove_dir_all(&nomt_db_folder);
        }

        let mut opts = Options::new();
        opts.path(nomt_db_folder);
        opts.commit_concurrency(commit_concurrency);
        opts.io_workers(io_workers);
        opts.metrics(true);
        if let Some(size) = page_cache_size {
            opts.page_cache_size(size);
        }
        if let Some(size) = leaf_cache_size {
            opts.leaf_cache_size(size);
        }
        if let Some(buckets) = hashtable_buckets {
            opts.hashtable_buckets(buckets);
        }
        opts.page_cache_upper_levels(page_cache_upper_levels);
        opts.prepopulate_page_cache(prepopulate_page_cache);

        let nomt = Nomt::open(opts).unwrap();
        Self {
            nomt,
            overlay_window_capacity,
            overlay_window: Mutex::new(VecDeque::new()),
        }
    }

    fn commit_overlay(
        &self,
        overlay_window: &mut VecDeque<Overlay>,
        mut timer: Option<&mut Timer>,
    ) {
        if self.overlay_window_capacity == 0 {
            return;
        }

        if overlay_window.len() == self.overlay_window_capacity {
            let _ = timer.as_mut().map(|t| t.record_span("commit_overlay"));
            let overlay = overlay_window.pop_back().unwrap();
            overlay.commit(&self.nomt).unwrap();
        }
    }

    pub fn execute(&self, mut timer: Option<&mut Timer>, workload: &mut dyn Workload) {
        let mut overlay_window = self.overlay_window.lock().unwrap();
        if overlay_window.len() < self.overlay_window_capacity {
            timer = None;
        }
        let _timer_guard_total = timer.as_mut().map(|t| t.record_span("workload"));

        self.commit_overlay(&mut overlay_window, timer.as_mut().map(|t| &mut **t));

        let session_params = SessionParams::default().witness_mode(WitnessMode::read_write());

        let session_params = if self.overlay_window_capacity == 0 {
            session_params
        } else {
            session_params.overlay(overlay_window.iter()).unwrap()
        };
        let session = self.nomt.begin_session(session_params);

        let mut transaction = Tx {
            session: &session,
            access: FxHashMap::default(),
            timer,
        };

        workload.run_step(&mut transaction);

        let Tx {
            access, mut timer, ..
        } = transaction;

        let _timer_guard_commit = timer.as_mut().map(|t| t.record_span("commit_and_prove"));
        let mut actual_access: Vec<_> = access.into_iter().collect();
        actual_access.sort_by_key(|(k, _)| *k);

        let finished = session.finish(actual_access).unwrap();
        if self.overlay_window_capacity == 0 {
            finished.commit(&self.nomt).unwrap();
        } else {
            let new_overlay = finished.into_overlay();
            overlay_window.push_front(new_overlay);
        }
    }

    // note: this is only intended to be used with workloads which are disjoint, i.e. no workload
    // writes a key which another workload reads. re-implementing BlockSTM or other OCC methods are
    // beyond the scope of benchtop.
    pub fn parallel_execute(
        &self,
        mut timer: Option<&mut Timer>,
        thread_pool: &rayon::ThreadPool,
        workloads: &mut [Box<dyn Workload>],
    ) {
        let mut overlay_window = self.overlay_window.lock().unwrap();
        if overlay_window.len() < self.overlay_window_capacity {
            timer = None;
        }

        let _timer_guard_total = timer.as_mut().map(|t| t.record_span("workload"));

        self.commit_overlay(&mut overlay_window, timer.as_mut().map(|t| &mut **t));

        let session_params = SessionParams::default().witness_mode(WitnessMode::read_write());

        let session_params = if self.overlay_window_capacity == 0 {
            session_params
        } else {
            session_params.overlay(overlay_window.iter()).unwrap()
        };
        let session = self.nomt.begin_session(session_params);
        let mut results: Vec<Option<_>> = (0..workloads.len()).map(|_| None).collect();

        let use_timer = timer.is_some();
        thread_pool.in_place_scope(|scope| {
            for (workload, result) in workloads.into_iter().zip(results.iter_mut()) {
                let session = &session;
                scope.spawn(move |_| {
                    let mut workload_timer = if use_timer {
                        Some(Timer::new(String::new()))
                    } else {
                        None
                    };
                    let mut transaction = Tx {
                        session,
                        access: FxHashMap::default(),
                        timer: workload_timer.as_mut(),
                    };
                    workload.run_step(&mut transaction);
                    *result = Some((transaction.access, workload_timer.map(|t| t.freeze())));
                })
            }
        });

        // absorb instrumented times from workload timers.
        for (_, ref mut workload_timer) in results.iter_mut().flatten() {
            if let (Some(ref mut t), Some(wt)) = (timer.as_mut(), workload_timer.take()) {
                t.add(wt);
            }
        }

        let _timer_guard_commit = timer.as_mut().map(|t| t.record_span("commit_and_prove"));
        let mut actual_access: Vec<_> = results
            .into_iter()
            .flatten()
            .map(|(access, _)| access)
            .flatten()
            .collect();
        actual_access.sort_by_key(|(k, _)| *k);

        let finished = session.finish(actual_access).unwrap();
        if self.overlay_window_capacity == 0 {
            finished.commit(&self.nomt).unwrap();
        } else {
            let new_overlay = finished.into_overlay();
            overlay_window.push_front(new_overlay);
        }
    }

    pub fn print_metrics(&self) {
        self.nomt.metrics().print();
        let ht_stats = self.nomt.hash_table_utilization();
        println!(
            "  buckets {}/{} ({})",
            ht_stats.occupied,
            ht_stats.capacity,
            ht_stats.occupancy_rate()
        );
    }
}

struct Tx<'a> {
    timer: Option<&'a mut Timer>,
    session: &'a Session<Blake3Hasher>,
    access: FxHashMap<KeyPath, KeyReadWrite>,
}

impl<'a> Transaction for Tx<'a> {
    fn read(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let key_path = sha2::Sha256::digest(key).into();
        let _timer_guard_read = self.timer.as_mut().map(|t| t.record_span("read"));

        match self.access.entry(key_path) {
            Entry::Occupied(o) => o.get().last_value().map(|v| v.to_vec()),
            Entry::Vacant(v) => {
                let value = self.session.read(key_path).unwrap();
                self.session.warm_up(key_path);

                v.insert(KeyReadWrite::Read(value.clone()));
                value.map(|v| v.to_vec())
            }
        }
    }

    fn note_read(&mut self, key: &[u8], value: Option<Vec<u8>>) {
        let key_path = sha2::Sha256::digest(key).into();

        match self.access.entry(key_path) {
            Entry::Occupied(mut o) => {
                o.get_mut().read(value);
            }
            Entry::Vacant(v) => {
                self.session.warm_up(key_path);
                v.insert(KeyReadWrite::Read(value));
            }
        }
    }

    fn write(&mut self, key: &[u8], value: Option<&[u8]>) {
        let key_path = sha2::Sha256::digest(key).into();
        let value = value.map(|v| v.to_vec());

        match self.access.entry(key_path) {
            Entry::Occupied(mut o) => {
                o.get_mut().write(value);
            }
            Entry::Vacant(v) => {
                v.insert(KeyReadWrite::Write(value));
            }
        }

        self.session.warm_up(key_path);
        self.session.preserve_prior_value(key_path);
    }
}
