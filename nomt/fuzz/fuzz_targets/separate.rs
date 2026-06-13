#![no_main]

mod common;

use bitvec::{order::Msb0, view::BitView};
use common::Run;
use libfuzzer_sys::fuzz_target;
use nomt::beatree::separate;

fuzz_target!(|run: Run| {
    let Run {
        prefix_bit_len,
        mut a,
        mut b,
    } = run;

    if a > b {
        std::mem::swap(&mut a, &mut b);
    }

    let mut expected = [0u8; 32];
    expected.view_bits_mut::<Msb0>()[..prefix_bit_len + 1]
        .copy_from_bitslice(&b.view_bits::<Msb0>()[..prefix_bit_len + 1]);

    assert_eq!(expected, separate(&a, &b));
});
