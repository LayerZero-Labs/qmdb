#![no_main]

use arbitrary::Arbitrary;
use bitvec::{order::Msb0, view::BitView};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|run: Run| {
    let Run {
        separator_len,
        separator,
    } = run;

    assert_eq!(separator_len, nomt::beatree::separator_len(&separator));
});

#[derive(Debug)]
struct Run {
    separator_len: usize,
    separator: [u8; 32],
}

impl<'a> Arbitrary<'a> for Run {
    fn arbitrary(input: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let mut separator_len = input.int_in_range(0..=255)?;
        let mut separator = [0; 32];
        input.fill_buffer(&mut separator)?;
        separator.view_bits_mut::<Msb0>()[separator_len..].fill(false);

        if separator == [0u8; 32] {
            separator_len = 1;
        } else {
            let effective_separator_len = 256 - separator.view_bits::<Msb0>().trailing_zeros();
            if separator_len != effective_separator_len {
                return Err(arbitrary::Error::IncorrectFormat);
            }
        };

        Ok(Self {
            separator_len,
            separator,
        })
    }
}
