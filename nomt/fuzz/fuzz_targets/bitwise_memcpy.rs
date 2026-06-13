#![no_main]

use arbitrary::Arbitrary;
use bitvec::{order::Msb0, view::BitView};
use libfuzzer_sys::fuzz_target;
use nomt::beatree::bitwise_memcpy;

const MAX_BYTES_LEN: usize = 1 << 12; // 4KiB

fuzz_target!(|run: Run| {
    let Run {
        source,
        mut destination,
    } = run;

    let expected = reference_bitwise_memcpy(&source, &destination);

    bitwise_memcpy(
        &mut destination.bytes,
        destination.bit_start,
        &source.bytes,
        source.bit_start,
        source.bit_len,
    );

    assert_eq!(expected, destination.bytes);
});

#[derive(Debug)]
struct Run {
    source: Source,
    destination: Destination,
}

#[derive(Debug)]
struct Source {
    bit_start: usize,
    bit_len: usize,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct Destination {
    bit_start: usize,
    bytes: Vec<u8>,
}

impl<'a> Arbitrary<'a> for Run {
    fn arbitrary(input: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let source = Source::arbitrary(input)?;

        // Destination must be long enough to store the source.
        let destination_bit_start = input.int_in_range(0..=7)?;
        let min_destination_len = (destination_bit_start + source.bit_len + 7) / 8;
        let destination_len = input.int_in_range(min_destination_len..=MAX_BYTES_LEN)?;
        let mut destination_bytes = vec![0; destination_len];
        input.fill_buffer(&mut destination_bytes)?;

        let run = Run {
            source,
            destination: Destination {
                bit_start: destination_bit_start,
                bytes: destination_bytes,
            },
        };

        Ok(run)
    }
}

impl<'a> Arbitrary<'a> for Source {
    fn arbitrary(input: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let bytes_len = (input.int_in_range(0..=MAX_BYTES_LEN)? as usize).next_multiple_of(8);

        let mut bytes: Vec<u8> = vec![0; bytes_len];
        input.fill_buffer(&mut bytes)?;

        let bit_start = if bytes_len != 0 {
            input.int_in_range(0..=7)?
        } else {
            0
        };

        let bit_len = if bytes_len > 0 {
            // `bitwise_memcpy` requires to the source length to be the smallest length,
            // multiple of 8 bytes that the contain the source bits.
            let min_bit_len = ((bytes_len - 8) * 8).saturating_sub(bit_start) + 1;
            let max_bit_len = (bytes_len * 8) - bit_start;
            input.int_in_range(min_bit_len..=max_bit_len)?
        } else {
            0
        };

        Ok(Self {
            bit_start,
            bit_len,
            bytes,
        })
    }
}

fn reference_bitwise_memcpy(source: &Source, destination: &Destination) -> Vec<u8> {
    let mut destination_bytes = destination.bytes.clone();

    destination_bytes.view_bits_mut::<Msb0>()[destination.bit_start..][..source.bit_len]
        .copy_from_bitslice(
            &source.bytes.view_bits::<Msb0>()[source.bit_start..][..source.bit_len],
        );

    destination_bytes
}
