#![feature(test)]
#![cfg(test)]
extern crate test;
extern crate unroll;

use ethereum_types::H256;
use lvmt_db::crypto::export::G1Projective;
use lvmt_db::crypto::{AMTParams, Pairing, TypeDepths, TypeUInt};
use lvmt_db::lvmt_db::NUM_COLS;
use lvmt_db::{Key, LvmtDB, Proof};
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use test::{black_box, Bencher};
use unroll::unroll_for_loops;

#[unroll_for_loops]
fn mul_u64(x: &mut [u64; 1000], y: &[u64; 1000]) {
    for i in 0..1000 {
        x[i] *= y[i];
    }
}

#[bench]
fn bench_u64(b: &mut Bencher) {
    let mut rng = rand::thread_rng();
    let mut x: [u64; 1000] = [(); 1000].map(|_| rng.gen::<u64>());
    let y: [u64; 1000] = [(); 1000].map(|_| rng.gen::<u64>());
    b.iter(move || mul_u64(&mut x, &y))
}

#[bench]
fn bench_prove(b: &mut Bencher) {
    let mut rng = rand::thread_rng();

    let backend = lvmt_db::storage::test_kvdb(NUM_COLS);
    let pp = Arc::new(AMTParams::<Pairing>::from_dir(
        "./pp",
        TypeDepths::USIZE,
        true,
    ));
    let mut db = LvmtDB::new(backend, pp.clone(), false, Some((0, 0)));

    let mut epoch_root_dict = HashMap::new();

    let mut current_epoch = 0;
    let mut _latest_amt_root = G1Projective::default();

    for i in 0..=255 {
        db.set(&Key(vec![1, 2, i, 0]), vec![1, 2, i, 5].into());
        let (amt_root, epoch_root) = db.commit(current_epoch).unwrap();
        _latest_amt_root = amt_root;
        epoch_root_dict.insert(current_epoch, epoch_root);
        current_epoch += 1;
    }

    let prove_key =
        |key: Vec<u8>, value: Vec<u8>, db: &mut LvmtDB, epoch_root_dict: &HashMap<u64, H256>| {
            // println!("Verify key {:?}", key);
            let key = Key(key.to_vec());
            assert_eq!(value, db.get(&key).unwrap().unwrap().into_vec());
            db.prove(&key).unwrap()
            // AmtDb::verify(&key, &proof, |epoch| epoch_root_dict[&epoch], &pp).unwrap();
        };

    b.iter(|| {
        let i = rng.gen();
        let proof = prove_key(
            vec![1, 2, i, 0],
            vec![1, 2, i, 5],
            &mut db,
            &epoch_root_dict,
        );
        black_box(proof);
    })
}

#[bench]
fn bench_verify(b: &mut Bencher) {
    let mut rng = rand::thread_rng();

    let backend = lvmt_db::storage::test_kvdb(NUM_COLS);
    let pp = Arc::new(AMTParams::<Pairing>::from_dir(
        "./pp",
        TypeDepths::USIZE,
        true,
    ));
    let mut db = LvmtDB::new(backend, pp.clone(), false, Some((0, 0)));

    let mut epoch_root_dict = HashMap::new();

    let mut current_epoch = 0;
    let mut _latest_amt_root = G1Projective::default();

    for i in 0..=255 {
        db.set(&Key(vec![1, 2, i, 0]), vec![1, 2, i, 5].into());
        let (amt_root, epoch_root) = db.commit(current_epoch).unwrap();
        _latest_amt_root = amt_root;
        epoch_root_dict.insert(current_epoch, epoch_root);
        current_epoch += 1;
    }

    let prove_key =
        |key: Vec<u8>, value: Vec<u8>, db: &mut LvmtDB, epoch_root_dict: &HashMap<u64, H256>| {
            // println!("Verify key {:?}", key);
            let key = Key(key.to_vec());
            assert_eq!(value, db.get(&key).unwrap().unwrap().into_vec());
            db.prove(&key).unwrap()
            // AmtDb::verify(&key, &proof, |epoch| epoch_root_dict[&epoch], &pp).unwrap();
        };

    let proofs: Vec<Proof> = (0..=255u8)
        .map(|i| {
            prove_key(
                vec![1, 2, i, 0],
                vec![1, 2, i, 5],
                &mut db,
                &epoch_root_dict,
            )
        })
        .collect();

    b.iter(|| {
        let i = rng.gen();
        LvmtDB::verify(
            &Key(vec![1, 2, i, 0]),
            &proofs[i as usize],
            |epoch| epoch_root_dict[&epoch],
            &pp,
        )
        .unwrap();
    })
}
