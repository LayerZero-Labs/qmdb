use arbitrary::Arbitrary;
use bitvec::{order::Msb0, view::BitView};

#[derive(Debug)]
pub struct Run {
    pub prefix_bit_len: usize,
    pub a: [u8; 32],
    pub b: [u8; 32],
}

impl<'a> Arbitrary<'a> for Run {
    fn arbitrary(input: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let prefix_bit_len = input.int_in_range(0..=255)?;
        let mut a = [0; 32];
        let mut b = [0; 32];
        input.fill_buffer(&mut a)?;
        input.fill_buffer(&mut b)?;
        b.view_bits_mut::<Msb0>()[0..prefix_bit_len]
            .copy_from_bitslice(&a.view_bits::<Msb0>()[0..prefix_bit_len]);

        let effective_prefix_bit_len = a
            .view_bits::<Msb0>()
            .iter()
            .zip(b.view_bits::<Msb0>().iter())
            .take_while(|(a, b)| a == b)
            .count();

        if effective_prefix_bit_len != prefix_bit_len {
            Err(arbitrary::Error::IncorrectFormat)
        } else {
            Ok(Self {
                prefix_bit_len,
                a,
                b,
            })
        }
    }
}
