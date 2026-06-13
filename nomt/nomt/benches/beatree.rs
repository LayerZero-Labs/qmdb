#[cfg(feature = "benchmarks")]
use criterion::{criterion_group, criterion_main};
#[cfg(feature = "benchmarks")]
use nomt::beatree::benches::beatree_benchmark;

#[cfg(feature = "benchmarks")]
criterion_group!(benches, beatree_benchmark);
#[cfg(feature = "benchmarks")]
criterion_main!(benches);

#[cfg(not(feature = "benchmarks"))]
fn main() {}
