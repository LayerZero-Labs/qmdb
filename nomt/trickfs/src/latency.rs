use std::{
    collections::VecDeque,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    time::{Duration, Instant},
};

use rand::SeedableRng;
use rand_distr::Distribution;

/// Max possible delay, in micros, used as injected latency.
const MAX_LATENCY_MICROS: u64 = 1000;
type Reply = Box<dyn FnOnce() + Send + 'static>;

/// An injector of latencies.
///
/// This allows to schedule replies after a certain delay.
/// Delays are randomly chosen following a Pareto Distribution.
/// 80% of the delay will be below 20% of MAX_LATENCY_MICROS
pub struct LatencyInjector {
    rng: rand_pcg::Pcg64,
    distr: rand_distr::Pareto<f64>,
    tx: Sender<(Reply, Duration)>,
}

impl LatencyInjector {
    pub fn new(seed: u64) -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = std::thread::spawn(|| scheduler(rx));
        Self {
            rng: rand_pcg::Pcg64::seed_from_u64(seed),
            distr: rand_distr::Pareto::new(1.0, 1.16).unwrap(),
            tx,
        }
    }

    pub fn schedule_reply(&mut self, reply: Reply) {
        // Shift and scale, values above 100.0 (0.05%) are clipped to MAX_LATENCY_MICROS.
        let f = f64::min((self.distr.sample(&mut self.rng) - 1.0) / 100.0, 1.0);
        let micros = (f * MAX_LATENCY_MICROS as f64).round() as u64;
        let delay = Duration::from_micros(micros);
        self.tx.send((reply, delay)).unwrap();
    }
}

/// Task used to execute every scheduled reply.
fn scheduler(rx: Receiver<(Reply, Duration)>) {
    let mut scheduled: VecDeque<(Reply, Instant)> = VecDeque::new();
    loop {
        let (_, deadline) = match scheduled.front() {
            Some((reply, deadline)) => (reply, deadline),
            None => {
                // Nothing scheduled, wait for next reply.
                match rx.recv() {
                    Ok((reply, delay)) => {
                        schedule_new_reply(&mut scheduled, reply, delay);
                    }
                    Err(_) => break,
                }
                continue;
            }
        };

        // Wait for a new reply to be scheduled or until we reach the deadline
        // of the first reply in the queue.
        let timeout = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(timeout) {
            Ok((reply, delay)) => schedule_new_reply(&mut scheduled, reply, delay),
            Err(RecvTimeoutError::Timeout) => {
                let (reply, _) = scheduled.pop_front().unwrap();
                reply();
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
    }

    // Answer to all pending replies.
    for (reply, _) in scheduled {
        reply();
    }
}

/// Insert the reply into the scheduled queue following an order by the deadlines.
fn schedule_new_reply(scheduled: &mut VecDeque<(Reply, Instant)>, reply: Reply, delay: Duration) {
    let deadline = std::time::Instant::now() + delay.clone();
    // If two replies happen to have the same deadline, then they will be kept in FIFO order.
    let idx = match scheduled.binary_search_by_key(&deadline, |(_, d)| *d) {
        Ok(idx) => idx + 1,
        Err(idx) => idx,
    };
    scheduled.insert(idx, (reply, deadline));
}
