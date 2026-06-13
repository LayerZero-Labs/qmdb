#![allow(dead_code, unused)]

mod adapter;

pub use adapter::Adapter;

pub use bellman_ce::pairing::bn256::Bn256;
pub use powersoftau::batched_accumulator::BatchedAccumulator;
pub use powersoftau::parameters::{CeremonyParams, CheckForCorrectness, UseCompression};

use memmap::MmapOptions;
use std::fs::OpenOptions;

pub fn from_challenge<'a>(
    response_filename: &str,
    size: usize,
    parameters: &'a CeremonyParams<Bn256>,
) -> Box<BatchedAccumulator<'a, Bn256>> {
    let mut accumulator = BatchedAccumulator::empty(&parameters);
    let reader = OpenOptions::new()
        .read(true)
        .open(response_filename)
        .expect("unable open response file in this directory");
    let input_map = unsafe {
        MmapOptions::new()
            .map(&reader)
            .expect("unable to create a memory map for input")
    };
    accumulator
        .read_chunk(
            0,
            1 << size,
            UseCompression::No,
            CheckForCorrectness::Yes,
            &input_map,
        )
        .unwrap();
    Box::new(accumulator)
}

// #[test]
// fn test_load_and_pair() {
//     use ark_bn254::Bn254;
//     use ark_ec::PairingEngine;
//
//     println!("adc");
//     let (g1, g2) = from_challenge("/data/chenxing/challenge0072", 6, &CeremonyParams::<Bn254>::new(28, 20));
//     println!("{} {}", g1.len(), g2.len());
//     assert_eq!(Bn254::pairing(g1[0], g2[3]), Bn254::pairing(g1[4], g2[0]));
// }
