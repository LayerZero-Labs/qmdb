use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Defines how the sliding window should behave
#[derive(Debug)]
pub enum WindowType {
    /// Keep only samples within the last specified duration
    TimeBased(Duration),
    /// Keep only the last N samples
    CountBased(usize),
}

#[derive(Debug)]
pub struct LatencyStats {
    samples: Mutex<VecDeque<(Instant, Duration)>>,
    window_type: WindowType,
    total_samples: Mutex<usize>,
    total_latency: Mutex<Duration>,
}

impl LatencyStats {
    pub fn new(window_type: WindowType) -> Self {
        let capacity = match window_type {
            WindowType::CountBased(n) => n,
            WindowType::TimeBased(_) => 128, // reasonable initial capacity
        };

        Self {
            samples: Mutex::new(VecDeque::with_capacity(capacity)),
            window_type,
            total_samples: Mutex::new(0),
            total_latency: Mutex::new(Duration::ZERO),
        }
    }

    /// Creates a new latency tracker with a 60-second sliding window
    pub fn with_duration_window(window: Duration) -> Self {
        Self::new(WindowType::TimeBased(window))
    }

    /// Creates a new latency tracker that keeps the last N samples
    pub fn with_count_window(count: usize) -> Self {
        Self::new(WindowType::CountBased(count))
    }

    pub fn record(&self, duration: Duration) {
        let mut samples = self.samples.lock().unwrap();
        let mut total_samples = self.total_samples.lock().unwrap();
        let mut total_latency = self.total_latency.lock().unwrap();
        let now = Instant::now();

        // Add new sample
        samples.push_back((now, duration));

        // Update total samples and total latency
        *total_samples += 1;
        *total_latency += duration;

        // Enforce window constraint
        match self.window_type {
            WindowType::TimeBased(window) => {
                // Remove samples older than our window
                while let Some((timestamp, _)) = samples.front() {
                    if now.duration_since(*timestamp) > window {
                        samples.pop_front();
                    } else {
                        break;
                    }
                }
            }
            WindowType::CountBased(size) => {
                // Keep only the last `size` samples
                while samples.len() > size {
                    samples.pop_front();
                }
            }
        }
    }

    pub fn time<F, T>(&self, f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let start = Instant::now();
        let result = f();
        self.record(start.elapsed());
        result
    }

    /// Returns basic statistics (min, max, mean)
    pub fn stats(&self) -> Option<(Duration, Duration, Duration)> {
        let samples = self.samples.lock().unwrap();
        if samples.is_empty() {
            return None;
        }

        let durations: Vec<_> = samples.iter().map(|(_, d)| *d).collect();
        let min = *durations.iter().min().unwrap();
        let max = *durations.iter().max().unwrap();
        let sum: Duration = durations.iter().sum();
        let mean = sum / durations.len() as u32;

        Some((min, max, mean))
    }

    /// Returns detailed statistics including multiple percentiles
    pub fn detailed_stats(&self) -> Option<DetailedStats> {
        let samples = self.samples.lock().unwrap();
        if samples.is_empty() {
            return None;
        }

        let mut durations: Vec<_> = samples.iter().map(|(_, d)| *d).collect();
        durations.sort_unstable();

        let len = durations.len();
        let sum: Duration = durations.iter().sum();

        Some(DetailedStats {
            count: len,
            min: durations[0],
            max: durations[len - 1],
            mean: sum / len as u32,
            p10: durations[((10.0 / 100.0) * (len - 1) as f64).round() as usize],
            p20: durations[((20.0 / 100.0) * (len - 1) as f64).round() as usize],
            p25: durations[((25.0 / 100.0) * (len - 1) as f64).round() as usize],
            p30: durations[((30.0 / 100.0) * (len - 1) as f64).round() as usize],
            p40: durations[((40.0 / 100.0) * (len - 1) as f64).round() as usize],
            p50: durations[((50.0 / 100.0) * (len - 1) as f64).round() as usize],
            p60: durations[((60.0 / 100.0) * (len - 1) as f64).round() as usize],
            p70: durations[((70.0 / 100.0) * (len - 1) as f64).round() as usize],
            p75: durations[((75.0 / 100.0) * (len - 1) as f64).round() as usize],
            p80: durations[((80.0 / 100.0) * (len - 1) as f64).round() as usize],
            p90: durations[((90.0 / 100.0) * (len - 1) as f64).round() as usize],
            p95: durations[((95.0 / 100.0) * (len - 1) as f64).round() as usize],
            p99: durations[((99.0 / 100.0) * (len - 1) as f64).round() as usize],
            p999: durations[((99.9 / 100.0) * (len - 1) as f64).round() as usize],
            p9999: durations[((99.99 / 100.0) * (len - 1) as f64).round() as usize],
        })
    }

    /// Computes a specific percentile latency (0.0 to 100.0)
    pub fn percentile(&self, percentile: f64) -> Option<Duration> {
        let samples = self.samples.lock().unwrap();
        if samples.is_empty() {
            return None;
        }

        let mut durations: Vec<_> = samples.iter().map(|(_, d)| *d).collect();
        durations.sort_unstable();

        let index = (percentile / 100.0 * (durations.len() - 1) as f64).round() as usize;
        Some(durations[index])
    }

    /// Returns the number of samples in the current window
    pub fn sample_count(&self) -> usize {
        self.samples.lock().unwrap().len()
    }

    /// Clears all samples
    pub fn clear(&self) {
        self.samples.lock().unwrap().clear();
    }

    /// Returns the total number of samples recorded
    pub fn get_total_samples(&self) -> usize {
        *self.total_samples.lock().unwrap()
    }

    /// Returns the total latency recorded
    pub fn get_total_latency(&self) -> Duration {
        *self.total_latency.lock().unwrap()
    }
}

impl Serialize for LatencyStats {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.detailed_stats()
            .unwrap_or_default()
            .serialize(serializer)
    }
}

/// Detailed statistics about latency measurements
#[derive(Debug, Clone, Copy)]
pub struct DetailedStats {
    pub count: usize,
    pub min: Duration,
    pub max: Duration,
    pub mean: Duration,
    pub p10: Duration,
    pub p20: Duration,
    pub p25: Duration,
    pub p30: Duration,
    pub p40: Duration,
    pub p50: Duration, // median
    pub p60: Duration,
    pub p70: Duration,
    pub p75: Duration,
    pub p80: Duration,
    pub p90: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub p999: Duration,
    pub p9999: Duration,
}

impl DetailedStats {
    pub fn print(&self, name: &str, compact: bool) {
        if compact {
            println!("{} Latency: mean={:?}, min={:?}, p10={:?}, p20={:?}, p25={:?}, p30={:?}, p40={:?}, p50={:?}, p60={:?}, p70={:?}, p75={:?}, p80={:?}, p90={:?}, p95={:?}, p99={:?}, p999={:?}, p9999={:?}, max={:?}", 
                name, self.mean, self.min, self.p10, self.p20, self.p25, self.p30, self.p40, self.p50,
                self.p60, self.p70, self.p75, self.p80, self.p90, self.p95, self.p99, self.p999, self.p9999, self.max);
        } else {
            println!("{name} Latency Statistics:");
            println!("  Sample count: {}", self.count);
            println!("  Min:   {:?}", self.min);
            println!("  Max:   {:?}", self.max);
            println!("  Mean:  {:?}", self.mean);
            println!("  p10:   {:?}", self.p10);
            println!("  p20:   {:?}", self.p20);
            println!("  p25:   {:?}", self.p25);
            println!("  p30:   {:?}", self.p30);
            println!("  p40:   {:?}", self.p40);
            println!("  p50:   {:?}", self.p50);
            println!("  p60:   {:?}", self.p60);
            println!("  p70:   {:?}", self.p70);
            println!("  p75:   {:?}", self.p75);
            println!("  p80:   {:?}", self.p80);
            println!("  p90:   {:?}", self.p90);
            println!("  p95:   {:?}", self.p95);
            println!("  p99:   {:?}", self.p99);
            println!("  p999:  {:?}", self.p999);
            println!("  p9999: {:?}", self.p9999);
        }
    }
}

impl Default for DetailedStats {
    fn default() -> Self {
        Self {
            count: 0,
            min: Duration::ZERO,
            max: Duration::ZERO,
            mean: Duration::ZERO,
            p10: Duration::ZERO,
            p20: Duration::ZERO,
            p25: Duration::ZERO,
            p30: Duration::ZERO,
            p40: Duration::ZERO,
            p50: Duration::ZERO,
            p60: Duration::ZERO,
            p70: Duration::ZERO,
            p75: Duration::ZERO,
            p80: Duration::ZERO,
            p90: Duration::ZERO,
            p95: Duration::ZERO,
            p99: Duration::ZERO,
            p999: Duration::ZERO,
            p9999: Duration::ZERO,
        }
    }
}

impl Serialize for DetailedStats {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("DetailedStats", 19)?;
        state.serialize_field("count", &self.count)?;
        state.serialize_field("min", &self.min.as_secs_f64())?;
        state.serialize_field("max", &self.max.as_secs_f64())?;
        state.serialize_field("mean", &self.mean.as_secs_f64())?;
        state.serialize_field("p10", &self.p10.as_secs_f64())?;
        state.serialize_field("p20", &self.p20.as_secs_f64())?;
        state.serialize_field("p25", &self.p25.as_secs_f64())?;
        state.serialize_field("p30", &self.p30.as_secs_f64())?;
        state.serialize_field("p40", &self.p40.as_secs_f64())?;
        state.serialize_field("p50", &self.p50.as_secs_f64())?;
        state.serialize_field("p60", &self.p60.as_secs_f64())?;
        state.serialize_field("p70", &self.p70.as_secs_f64())?;
        state.serialize_field("p75", &self.p75.as_secs_f64())?;
        state.serialize_field("p80", &self.p80.as_secs_f64())?;
        state.serialize_field("p90", &self.p90.as_secs_f64())?;
        state.serialize_field("p95", &self.p95.as_secs_f64())?;
        state.serialize_field("p99", &self.p99.as_secs_f64())?;
        state.serialize_field("p999", &self.p999.as_secs_f64())?;
        state.serialize_field("p9999", &self.p9999.as_secs_f64())?;
        state.end()
    }
}
