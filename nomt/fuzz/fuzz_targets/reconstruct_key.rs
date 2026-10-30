#![no_main]

use arbitrary::Arbitrary;
use bitvec::{order::Msb0, view::BitView};
use libfuzzer_sys::fuzz_target;
use nomt::beatree::reconstruct_key;

fuzz_target!(|run: Run| {
    let Run {
        raw_separator,
        raw_prefix,
    } = run;

    let expected = reference_reconstruct_key(&raw_prefix, &raw_separator);

    let maybe_prefix = if raw_prefix.bit_len == 0 {
        None
    } else {
        Some((&raw_prefix.bytes[..], raw_prefix.bit_len))
    };

    let raw_separator = (
        &raw_separator.bytes[..],
        raw_separator.bit_start,
        raw_separator.bit_len,
    );

    assert_eq!(expected, reconstruct_key(maybe_prefix, raw_separator));
});

#[derive(Debug)]
struct Run {
    raw_separator: RawSeparator,
    raw_prefix: RawPrefix,
}

#[derive(Debug)]
struct RawSeparator {
    bit_start: usize,
    bit_len: usize,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct RawPrefix {
    bit_len: usize,
    bytes: Vec<u8>,
}

impl<'a> Arbitrary<'a> for Run {
    fn arbitrary(input: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let raw_separator = RawSeparator::arbitrary(input)?;

        let raw_prefix_bit_len = input.int_in_range(0..=(256 - raw_separator.bit_len))?;
        let raw_prefix_min_byte_len = (raw_prefix_bit_len + 7) / 8;
        let raw_prefix_byte_len = input.int_in_range(raw_prefix_min_byte_len..=(1 << 12))?;
        let mut raw_prefix_bytes = vec![0; raw_prefix_byte_len];
        input.fill_buffer(&mut raw_prefix_bytes)?;

        let run = Run {
            raw_separator,
            raw_prefix: RawPrefix {
                bit_len: raw_prefix_bit_len,
                bytes: raw_prefix_bytes,
            },
        };

        Ok(run)
    }
}

impl<'a> Arbitrary<'a> for RawSeparator {
    fn arbitrary(input: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let bit_start = input.int_in_range(0..=7)?;

        let bit_len = input.int_in_range(0..=(256 - bit_start))?;

        let bytes_len = (((bit_start + bit_len + 7) / 8) as usize).next_multiple_of(8);
        let mut bytes: Vec<u8> = vec![0; bytes_len];
        input.fill_buffer(&mut bytes)?;

        Ok(Self {
            bit_start,
            bit_len,
            bytes,
        })
    }
}

fn reference_reconstruct_key(maybe_prefix: &RawPrefix, separator: &RawSeparator) -> [u8; 32] {
    let mut key = [0; 32];

    let mut key_start_separator = 0;
    let RawPrefix { bit_len, bytes } = maybe_prefix;
    if *bit_len != 0 {
        key.view_bits_mut::<Msb0>()[..*bit_len]
            .copy_from_bitslice(&bytes.view_bits::<Msb0>()[..*bit_len]);
        key_start_separator = *bit_len;
    }

    let RawSeparator {
        bit_start,
        bit_len,
        bytes,
    } = separator;

    key.view_bits_mut::<Msb0>()[key_start_separator..][..*bit_len]
        .copy_from_bitslice(&bytes.view_bits::<Msb0>()[*bit_start..][..*bit_len]);

    key
}
