#![no_main]

mod common;

use common::Run;
use libfuzzer_sys::fuzz_target;
use nomt::beatree::prefix_len;

fuzz_target!(|run: Run| {
    let Run {
        prefix_bit_len,
        a,
        b,
    } = run;

    assert_eq!(prefix_bit_len, prefix_len(&a, &b));
});
