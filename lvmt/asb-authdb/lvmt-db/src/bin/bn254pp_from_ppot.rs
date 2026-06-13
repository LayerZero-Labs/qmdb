use lvmt_db::crypto::{
    export::{CanonicalSerialize, G1Aff, G2Aff},
    pp_file_name, PowerTau,
};
use ppot2ark::{from_challenge, Adapter, Bn256, CeremonyParams};
use std::fs::File;

use ark_bn254::Bn254;

fn fetch_pp_from_ppot(filename: &str, size: usize) -> PowerTau<Bn254> {
    let params = CeremonyParams::<Bn256>::new(28, 20);
    let accumulator = from_challenge(filename, size, &params);
    let g1: Vec<G1Aff<Bn254>> = (0..(1 << size))
        .map(|idx| accumulator.tau_powers_g1[idx].adapt())
        .collect();
    let g2: Vec<G2Aff<Bn254>> = (0..(1 << size))
        .map(|idx| accumulator.tau_powers_g2[idx].adapt())
        .collect();
    return PowerTau(g1, g2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        println!("Usage: \n<challenge_file> <pow_size> <dir>");
        std::process::exit(exitcode::USAGE);
    }

    let challenge_filename = &args[1];
    let pow_size = args[2].parse().expect("could not parse pow_size");
    let dir: &String = &args[3].parse().expect("could not parse file");

    let file = format!("{}/{}", dir, pp_file_name::<Bn254>(pow_size));

    let pp = fetch_pp_from_ppot(challenge_filename, pow_size);

    let buffer = File::create(file).unwrap();
    pp.serialize_uncompressed(&buffer).unwrap();
}
