use super::error;
use super::export::{
    AffineCurve, CanonicalDeserialize, CanonicalSerialize, Fr, G1Aff, G2Aff, PairingEngine,
    ProjectiveCurve, SerializationError, UniformRand, G1, G2,
};
use super::pp_file_name;
use ark_ff::utils::k_adicity;
use ark_ff::Field;
use rand;
use rayon::prelude::*;
use std::fs::{create_dir_all, File};
use std::io::{Read, Write};
use std::path::Path;

#[derive(CanonicalDeserialize, CanonicalSerialize)]
pub struct PowerTau<PE: PairingEngine>(pub Vec<G1Aff<PE>>, pub Vec<G2Aff<PE>>);

fn power_tau<'a, G: AffineCurve>(gen: &'a G, tau: &'a G::ScalarField, length: usize) -> Vec<G> {
    let gen: G::Projective = gen.into_projective();
    (0usize..length)
        .into_par_iter()
        .chunks(1024)
        .map(|x| {
            ProjectiveCurve::batch_normalization_into_affine(
                &x.iter()
                    .map(|idx| {
                        let mut gen = gen.clone();
                        gen *= tau.pow([*idx as u64]);
                        gen
                    })
                    .collect::<Vec<G::Projective>>()[..],
            )
        })
        .flatten()
        .collect()
}

impl<PE: PairingEngine> PowerTau<PE> {
    #[cfg(test)]
    fn setup_with_tau(tau: Fr<PE>, depth: usize) -> PowerTau<PE> {
        Self::setup_inner(Some(tau), depth)
    }

    pub fn setup(depth: usize) -> PowerTau<PE> {
        Self::setup_inner(None, depth)
    }

    fn setup_inner(tau: Option<Fr<PE>>, depth: usize) -> PowerTau<PE> {
        let random_tau = Fr::<PE>::rand(&mut rand::thread_rng());
        let tau = tau.unwrap_or(random_tau);

        let gen1 = G1Aff::<PE>::prime_subgroup_generator();
        let gen2 = G2Aff::<PE>::prime_subgroup_generator();

        let g1pp: Vec<G1Aff<PE>> = power_tau(&gen1, &tau, 1 << depth);
        let g2pp: Vec<G2Aff<PE>> = power_tau(&gen2, &tau, 1 << depth);

        return PowerTau(g1pp, g2pp);
    }

    fn from_dir_inner(file: &str, expected_depth: usize) -> Result<PowerTau<PE>, error::Error> {
        let buffer = File::open(file)?;
        let pp: PowerTau<PE> = CanonicalDeserialize::deserialize_unchecked(buffer)?;
        let (g1_len, g2_len) = (pp.0.len(), pp.1.len());
        let depth = k_adicity(2, g1_len) as usize;
        if g1_len != g2_len {
            Err(error::ErrorKind::InconsistentLength.into())
        } else if expected_depth > depth {
            Err(error::ErrorKind::InconsistentLength.into())
        } else if expected_depth < g2_len {
            let g1_vec = pp.0[..1 << expected_depth].to_vec();
            let g2_vec = pp.1[..1 << expected_depth].to_vec();
            Ok(PowerTau(g1_vec, g2_vec))
        } else {
            Ok(pp)
        }
    }

    pub fn from_dir(dir: &str, expected_depth: usize) -> PowerTau<PE> {
        let file = &format!("{}/{}", dir, pp_file_name::<PE>(expected_depth));
        Self::from_dir_inner(file, expected_depth).expect(&format!(
            "Fail to load public parameters for {} at depth {}, read TODO to generate",
            std::any::type_name::<PE>(),
            expected_depth
        ))
    }

    pub fn from_dir_or_new(dir: &str, expected_depth: usize) -> PowerTau<PE> {
        let file = &format!("{}/{}", dir, pp_file_name::<PE>(expected_depth));
        match Self::from_dir_inner(file, expected_depth) {
            Ok(pp) => pp,
            Err(_) => {
                let pp = Self::setup(expected_depth);
                create_dir_all(Path::new(file).parent().unwrap()).unwrap();
                let buffer = File::create(file).unwrap();
                pp.serialize_uncompressed(&buffer).unwrap();
                pp
            }
        }
    }

    pub fn into_projective(self) -> (Vec<G1<PE>>, Vec<G2<PE>>) {
        let g1pp = self.0.iter().copied().map(|x| G1::<PE>::from(x)).collect();
        let g2pp = self.1.iter().copied().map(|x| G2::<PE>::from(x)).collect();
        (g1pp, g2pp)
    }
}

#[test]
fn test_partial_load() {
    type Pairing = super::export::Pairing;

    let tau = Fr::<Pairing>::rand(&mut rand::thread_rng());
    let large_pp = PowerTau::<Pairing>::setup_with_tau(tau, 8);
    let small_pp = PowerTau::<Pairing>::setup_with_tau(tau, 4);

    assert_eq!(small_pp.0[..], large_pp.0[..(small_pp.0.len())]);
    assert_eq!(small_pp.1[..], large_pp.1[..(small_pp.1.len())]);
}

#[test]
fn test_parallel_build() {
    use crate::crypto::export::{Pairing, ProjectiveCurve};

    const DEPTH: usize = 13;
    let tau = Fr::<Pairing>::rand(&mut rand::thread_rng());
    let gen1 = G1Aff::<Pairing>::prime_subgroup_generator();
    let g1pp_ans = power_tau(&gen1, &tau, 1 << DEPTH);

    let mut g1pp: Vec<G1Aff<Pairing>> = vec![];
    g1pp.reserve(1 << DEPTH);
    let mut gen1 = gen1.into_projective();
    for _ in 0..1 << DEPTH {
        g1pp.push(gen1.into_affine());
        gen1 *= tau.clone();
    }
    assert_eq!(g1pp, g1pp_ans)
}
