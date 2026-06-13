mod common;
use common::Test;
use rand::{prelude::SliceRandom, Rng, SeedableRng};
use std::time::{SystemTime, UNIX_EPOCH};

fn seed() -> [u8; 16] {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("no time?")
        .as_nanos()
        .to_le_bytes()[0..16]
        .try_into()
        .unwrap()
}

fn fill_and_empty(seed: [u8; 16], commit_concurrency: usize) {
    let mut rng = rand_pcg::Lcg64Xsh32::from_seed(seed);

    let db_size = 1 << 12;
    let commit_size = db_size / 16;

    let mut items = std::collections::BTreeSet::new();
    while items.len() < db_size as usize {
        items.insert(rand_key(&mut rng));
    }
    let mut items: Vec<_> = items.into_iter().collect();
    items.shuffle(&mut rng);

    let mut to_delete: Vec<usize> = (0..db_size as usize).collect();
    to_delete.shuffle(&mut rng);

    let mut t = Test::new_with_params(
        format!("fill_and_empty_{}", commit_concurrency), // name
        commit_concurrency,
        15000, // hashtable_buckets
        None,  // panic_on_sync
        true,  //  cleanup_dir
    );

    // inserting all the values
    let mut to_check = vec![];
    for i in 0..db_size {
        let key = items[i];
        let value = vec![i as u8; 400];

        to_check.push((key, value.clone()));
        t.write(key, Some(value));

        if (i + 1) % commit_size == 0 {
            t.commit();
            // check for presence
            for (key, value) in to_check.drain(..) {
                assert_eq!(t.read(key), Some(value));
            }
        }
    }

    // deleting all the values in different order
    let mut to_check = vec![];
    for i in 0..db_size {
        let key = items[to_delete[i]];

        to_check.push(key);
        t.write(key, None);

        if (i + 1) % commit_size == 0 {
            t.commit();
            // check for absence
            for key in to_check.drain(..) {
                assert_eq!(t.read(key), None);
            }
        }
    }

    assert!(t.commit().0.is_empty());
}

fn rand_key(rng: &mut impl Rng) -> [u8; 32] {
    let mut key = [0; 32];
    rng.fill(&mut key[..]);
    key
}

#[test]
fn fill_and_empty_1_commit_worker() {
    let seed = seed();
    let test_result = std::panic::catch_unwind(|| {
        fill_and_empty(seed, 1);
    });
    if let Err(cause) = test_result {
        eprintln!(
            "fill_and_empty_1_commit_worker failed with seed: {:?}",
            seed
        );
        std::panic::resume_unwind(cause);
    }
}

#[test]
fn fill_and_empty_64_commit_worker() {
    let seed = seed();
    let test_result = std::panic::catch_unwind(|| {
        fill_and_empty(seed, 64);
    });
    if let Err(cause) = test_result {
        eprintln!(
            "fill_and_empty_64_commit_worker failed with seed: {:?}",
            seed
        );
        std::panic::resume_unwind(cause);
    }
}
