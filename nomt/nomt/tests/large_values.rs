mod common;

use common::Test;

#[test]
fn large_values() {
    let mut t = Test::new("large_values");

    let large1 = vec![1; 4096 * 128];
    let large2 = vec![2; 4096 * 80 - 1245];

    t.write_id(0, Some(large1.clone()));
    t.write_id(1, Some(large2.clone()));
    let _ = t.commit();
    assert_eq!(&*t.read_id(0).unwrap(), &large1);
    assert_eq!(&*t.read_id(1).unwrap(), &large2);
    t.write_id(1, None);
    let _ = t.commit();
    assert_eq!(&*t.read_id(0).unwrap(), &large1);
    assert!(t.read_id(1).is_none());
}
