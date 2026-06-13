// Re-export all the required components in Zexe's repo.

// Since Zexe's repo doesn't have a stable implementation and could be refactored in the future,
// we import all the required objects in one place and all its usage for this repo should import from here.

pub use ark_bls12_381::Bls12_381;
pub use ark_bn254::Bn254;
pub use ark_ec::{AffineCurve, PairingEngine, ProjectiveCurve};
pub use ark_ff::{
    utils::k_adicity, BigInteger, FftField, Field, FpParameters, FromBytes, One, PrimeField,
    ToBytes, UniformRand, Zero,
};
pub use ark_poly::{EvaluationDomain, Radix2EvaluationDomain};
pub use ark_serialize::{
    CanonicalDeserialize, CanonicalSerialize, Read, SerializationError, Write,
};

pub type G1<PE> = <PE as PairingEngine>::G1Projective;
pub type G1Aff<PE> = <PE as PairingEngine>::G1Affine;
pub type G2<PE> = <PE as PairingEngine>::G2Projective;
pub type G2Aff<PE> = <PE as PairingEngine>::G2Affine;
pub type Fr<PE> = <PE as PairingEngine>::Fr;
pub type FrInt<PE> = <Fr<PE> as PrimeField>::BigInt;
pub type FrParams<PE> = <Fr<PE> as PrimeField>::Params;

pub type Pairing = Bn254;
pub type G1Projective = ark_bn254::G1Projective;
pub type G1Affine = ark_bn254::G1Affine;

pub mod instances {
    use super::Pairing;
    pub type G1 = super::G1<Pairing>;
    pub type G1Aff = super::G1Aff<Pairing>;
    pub type G2 = super::G2<Pairing>;
    pub type G2Aff = super::G2Aff<Pairing>;
    pub type Fr = super::Fr<Pairing>;
    pub type FrInt = super::FrInt<Pairing>;
    pub type FrParams = super::FrParams<Pairing>;
}
