use super::tree::{AMTConfigTrait, AMTData, AMTree};
use crate::amt::NodeIndex;
use crate::crypto::{
    export::{Fr, FrInt, Pairing, G1},
    AMTParams, TypeUInt,
};
use crate::storage::{FlattenArray, FlattenTree};
use crate::type_uint;
use std::sync::Arc;

struct TestConfig {}

type_uint! {
    struct TestDepths(6);
}

impl AMTConfigTrait for TestConfig {
    type PE = Pairing;
    type Name = u64;
    type Data = u64;
    type Commitment = G1<Pairing>;
    type DataLayout = FlattenArray;
    type TreeLayout = FlattenTree;
    type Height = TestDepths;
}

type TestTree = AMTree<TestConfig>;

fn test_all(amt: &mut TestTree, public_parameter: &AMTParams<Pairing>, task: &str) {
    for i in 0..TestConfig::LENGTH {
        let proof = amt.prove(i);
        let value = amt.get(i);

        assert!(
            TestTree::verify(
                i,
                value.as_fr(),
                amt.commitment(),
                proof.unwrap(),
                public_parameter
            ),
            "fail at task {} pos {}",
            task,
            i
        );
    }
}

impl AMTData<Fr<Pairing>> for u64 {
    fn as_fr_int(&self) -> FrInt<Pairing> {
        FrInt::<Pairing>::from(*self)
    }
}

#[test]
fn test_amt() {
    let db = crate::storage::test_db_col();

    const DEPTHS: usize = TestConfig::DEPTHS;
    const LENGTH: usize = 1 << DEPTHS;

    let pp = Arc::new(AMTParams::<Pairing>::from_dir("./pp", DEPTHS, true));

    let mut amt = TestTree::new(64, db, pp.clone(), Some(NodeIndex::<TestDepths>::root()));
    amt.set_commitment(&Default::default());

    test_all(&mut amt, &pp, "Empty");

    *amt.write_versions(0) += 1;
    assert_eq!(amt.get(0), &1);
    assert_eq!(amt.get(1), &0);
    test_all(&mut amt, &pp, "one-hot");

    *amt.write_versions(0) += &1;
    *amt.write_versions(LENGTH / 2) += &1;
    test_all(&mut amt, &pp, "sibling pair");
}

#[test]
fn test_one() {
    let db = crate::storage::test_db_col();

    const DEPTHS: usize = TestConfig::DEPTHS;

    let pp = Arc::new(AMTParams::<Pairing>::from_dir("./pp", DEPTHS, true));

    let mut amt = TestTree::new(64, db, pp.clone(), Some(NodeIndex::<TestDepths>::root()));
    amt.set_commitment(&Default::default());

    *amt.write_versions(0) += 1;
    assert_eq!(amt.get(0), &1);
    assert_eq!(amt.get(1), &0);

    let task = "one-hot";
    let i = 1;
    let proof = amt.prove(i);
    let value = amt.get(i);

    assert!(
        TestTree::verify(i, value.as_fr(), amt.commitment(), proof.unwrap(), &pp),
        "fail at task {} pos {}",
        task,
        i
    );
}
