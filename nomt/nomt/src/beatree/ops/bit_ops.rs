use crate::beatree::{
    branch::node::{RawPrefix, RawSeparators},
    Key,
};

// separate two keys a and b where b > a
pub fn separate(a: &Key, b: &Key) -> Key {
    //if b > a at some point b must have a 1 where a has a 0 and they are equal up to that point.
    let bit_len = prefix_len(a, b) + 1;
    let mut separator = [0u8; 32];

    let full_bytes = bit_len / 8;
    separator[..full_bytes].copy_from_slice(&b[..full_bytes]);

    let remaining = bit_len % 8;
    if remaining != 0 {
        let mask = !((1 << (8 - remaining)) - 1);
        separator[full_bytes] = b[full_bytes] & mask;
    }

    separator
}

pub fn prefix_len(key_a: &Key, key_b: &Key) -> usize {
    let mut bit_len = 0;
    'byte_loop: for byte in 0..32 {
        for bit in 0..8 {
            let mask = 1 << (7 - bit);
            if (key_a[byte] & mask) != (key_b[byte] & mask) {
                break 'byte_loop;
            }
            bit_len += 1;
        }
    }
    bit_len
}

pub fn separator_len(key: &Key) -> usize {
    if key == &[0u8; 32] {
        return 1;
    }
    let mut trailing_zeros = 0;
    'byte_offset: for byte in (0..32).rev() {
        for bit in (0..8).rev() {
            let mask = 1 << (7 - bit);
            if (key[byte] & mask) == mask {
                break 'byte_offset;
            }
            trailing_zeros += 1;
        }
    }

    256 - trailing_zeros
}

// Reconstruct a key starting from a prefix and a misaligned separator.
// Note: bit offsets starts from 0 going to 7, and the most significant bit is the one with index 0
pub fn reconstruct_key(maybe_prefix: Option<RawPrefix>, separator: RawSeparators) -> Key {
    let mut key = [0u8; 32];

    let prefix_bit_len = maybe_prefix.as_ref().map(|p| p.1).unwrap_or(0);
    let prefix_byte_len = (prefix_bit_len + 7) / 8;
    let prefix_end_bit_offset = prefix_bit_len % 8;
    let (separator_bytes, separator_bit_start, separator_bit_len) = separator;

    // where the separator will start to be stored
    let start_destination = match prefix_byte_len {
        0 => 0,
        len if prefix_end_bit_offset == 0 => len,
        // overlap between the end of the prefix and the beginning of the separator
        len => len - 1,
    };

    // shift the separator and store it in the key
    bitwise_memcpy(
        &mut key[start_destination..],
        prefix_end_bit_offset,
        separator_bytes,
        separator_bit_start,
        separator_bit_len,
    );

    if prefix_byte_len != 0 {
        // UNWRAP: prefix_byte_len can be different than 0 only if maybe_prefix is Some
        let prefix = maybe_prefix.unwrap();

        // copy the prefix into the key up to the penultimate byte
        key[0..prefix_byte_len - 1].copy_from_slice(&prefix.0[0..prefix_byte_len - 1]);

        // copy the last byte of the prefix without interfering with the separator
        let mask_shift = 8 - (prefix_bit_len % 8) as u32;
        let mask = 1u8.checked_shl(mask_shift).map(|m| !(m - 1)).unwrap_or(255);
        key[prefix_byte_len - 1] |= prefix.0[prefix_byte_len - 1] & mask;
    }

    key
}

fn first_chunk_mask(bit_start: usize) -> u64 {
    let mask_shift = (7 - bit_start) as u32 + 1 + (8 * 7);
    1u64.checked_shl(mask_shift)
        .map(|m| m - 1)
        .unwrap_or(u64::MAX)
}

fn last_chunk_mask(bit_start: usize, bit_len: usize, n_chunks: usize) -> u64 {
    let used_last_chunk_bits = (bit_start + bit_len).saturating_sub((n_chunks - 1) * 64);
    // saturating_sub is necessary to prevent overflow in the rare case in which a right
    // shift may shift some bits onto the `n_chunks` chunk, leading to more than 64 bits
    // on the `n_chunks - 1` chunk.
    1u64.checked_shl(64u32.saturating_sub(used_last_chunk_bits as u32))
        .map(|m| !(m - 1))
        .unwrap_or(0)
}

/// Copy `source_bit_len` from the source bytes, starting from the `source_bit_start`ith bit
/// within the first byte into the destination, starting from the `destination_bit_start`ith bit.
///
/// `destination` must be long enough to store the bits, accounting also possible shifts.
/// `source` bytes must have the smallest length, multiple of 8 bytes that can contain
/// all the source bits.
///
/// All the bits in `destination` not involved in the copy will be left unchanged.
pub fn bitwise_memcpy(
    destination: &mut [u8],
    destination_bit_start: usize,
    source: &[u8],
    source_bit_start: usize,
    source_bit_len: usize,
) {
    if source_bit_len == 0 {
        return;
    }

    enum Shift {
        Left(usize),                          // amount
        Right(usize, Option<u8>, Option<u8>), // amount, prev_remainder, curr_remainder
    }

    let mut shift = match destination_bit_start as isize - source_bit_start as isize {
        0 => None,
        shift if shift < 0 => Some(Shift::Left(shift.abs() as usize)),
        shift => Some(Shift::Right(shift as usize, None, None)),
    };

    // chunk is an 8-byte slice of the separator which will be cast to a u64 to simplify shifting
    let n_chunks = source.len() / 8;
    let bytes_to_write = (destination_bit_start + source_bit_len + 7) / 8;

    // container of all bits that will not be overwritten
    let mut chunk_data: Option<u64> = None;

    let mut destination_offset = 0;
    for chunk_index in 0..n_chunks {
        let chunk_start = chunk_index * 8;

        if let Some(Shift::Right(amount, _prev_remainder, curr_remainder)) = &mut shift {
            let mut mask = (1 << *amount) - 1;
            if chunk_index == n_chunks - 1 {
                // remove garbage of the last chunk from the remainder
                mask &=
                    last_chunk_mask(source_bit_start, source_bit_len, n_chunks).to_be_bytes()[7];
            }
            let bits = source[chunk_start + 7] & mask;
            *curr_remainder = Some(bits << (8 - *amount));
        }

        let mut chunk =
            u64::from_be_bytes(source[chunk_start..chunk_start + 8].try_into().unwrap());

        // maybe_masks could contain two masks:
        // + mask_from, used to extract only the useful bits from the source bytes
        // + mask_to, used to keep untouched bits unchanged in the destination
        let mut maybe_masks = None;
        if chunk_index == 0 {
            maybe_masks = Some((
                first_chunk_mask(source_bit_start),
                first_chunk_mask(destination_bit_start),
            ))
        }
        if chunk_index == n_chunks - 1 {
            let (mask_from, mask_to) = maybe_masks.take().unwrap_or((u64::MAX, u64::MAX));
            maybe_masks = Some((
                mask_from & last_chunk_mask(source_bit_start, source_bit_len, n_chunks),
                mask_to & last_chunk_mask(destination_bit_start, source_bit_len, n_chunks),
            ));
        }

        if let Some((mask_from, mask_to)) = maybe_masks {
            let destination_chunk = {
                // the destination could have less then 8 bytes available
                let n_byte = std::cmp::min(8, destination.len() - destination_offset);
                let destination_bytes = if n_byte < 8 {
                    let mut buf = [0u8; 8];
                    buf[..n_byte].copy_from_slice(
                        &destination[destination_offset..destination_offset + n_byte],
                    );
                    buf
                } else {
                    destination[destination_offset..destination_offset + n_byte]
                        .try_into()
                        .unwrap()
                };
                u64::from_be_bytes(destination_bytes)
            };
            chunk_data = Some(destination_chunk & !mask_to);
            chunk &= mask_from;
        }

        match &mut shift {
            Some(Shift::Left(amount)) => chunk <<= *amount,
            Some(Shift::Right(amount, _, _)) => chunk >>= *amount,
            _ => (),
        };

        if let Some(data) = chunk_data.take() {
            chunk |= data;
        }

        let mut chunk_shifted = chunk.to_be_bytes();

        // move bits remainder between chunk boundaries
        match &mut shift {
            Some(Shift::Left(amount)) if chunk_index < n_chunks - 1 => {
                // This is a rare case of copying the second-to-last source chunk to the destination,
                // taking only the remainder from the last source chunk. We know this is happening
                // because there is only 1 byte missing to be written in the destination after
                // `n_chunks - 2` chunks.
                // `mask` removes possible garbage from the last remainder.
                // `unchanged_bits` keeps data which is not involved in the memcpy in the destination.
                let mut mask = 255;
                let mut unchanged_bits = 0;
                if destination_offset + 8 == bytes_to_write {
                    mask = last_chunk_mask(source_bit_start, source_bit_len, n_chunks)
                        .to_be_bytes()[0];
                    let bits_to_keep =
                        (bytes_to_write * 8) - (destination_bit_start + source_bit_len);
                    let mask_to = (1 << bits_to_keep) - 1;
                    unchanged_bits = destination[destination_offset + 7] & mask_to;
                }

                let remainder_bits = (source[(chunk_index + 1) * 8] & mask) >> (8 - *amount);
                chunk_shifted[7] |= remainder_bits | unchanged_bits;
            }
            Some(Shift::Right(_, prev_remainder, curr_remainder)) => {
                if let Some(bits) = prev_remainder {
                    chunk_shifted[0] |= *bits;
                }
                *prev_remainder = *curr_remainder;
            }
            _ => (),
        };

        // store the shifted chunk into the key
        let n_byte = std::cmp::min(8, destination.len() - destination_offset);
        destination[destination_offset..destination_offset + n_byte]
            .copy_from_slice(&chunk_shifted[..n_byte]);
        destination_offset += n_byte;

        // break if the destination is already entirely being written
        if destination_offset >= bytes_to_write {
            break;
        }
    }

    // handle possible right remainder left unapplied
    if destination_offset < bytes_to_write {
        // There could be at most one byte which still needs to be written
        assert_eq!(bytes_to_write - destination_offset, 1);
        if let Some(Shift::Right(amount, Some(prev_remainder), _)) = &mut shift {
            // The last right shift could require a shift of fewer bits,
            // because there could be garbage between the end of the real
            // reminder and the end of the chunk.
            let garbage_bits = (n_chunks * 64) - (source_bit_start + source_bit_len);
            let mask = (1 << (8 - (*amount - garbage_bits))) - 1;

            destination[destination_offset] &= mask;
            destination[destination_offset] |= *prev_remainder & !mask;
        }
    }
}

#[cfg(feature = "benchmarks")]
pub mod benches {
    use crate::beatree::benches::get_key_pair;
    use criterion::{BenchmarkId, Criterion};
    use rand::RngCore;

    pub fn separate_benchmark(c: &mut Criterion) {
        let mut group = c.benchmark_group("separate");

        for shared_bytes in [0, 4, 8, 12, 16] {
            let (key1, key2) = get_key_pair(shared_bytes);
            group.bench_function(BenchmarkId::new("shared_bytes", shared_bytes), |b| {
                b.iter(|| super::separate(&key1, &key2));
            });
        }

        group.finish();
    }

    pub fn reconstruct_key_benchmark(c: &mut Criterion) {
        for i in 0..2 {
            let mut group = if i == 0 {
                c.benchmark_group("reconstruct_key_small_separator")
            } else {
                c.benchmark_group("reconstruct_key_big_separator")
            };

            for prefix_bits in [0, 1, 4, 7, 8, 9, 12, 15, 18] {
                let mut rand = rand::thread_rng();
                // 50 to extract multiples of 8 bytes for the separator
                let mut key = [0; 50];
                rand.fill_bytes(&mut key);

                let prefix_bytes = (prefix_bits + 7) / 8;
                let separator_bit_start = prefix_bits % 8;
                let separator_bit_len = if i == 0 {
                    // ensure that the raw separator is made up of only 8 bytes
                    57
                } else {
                    256 - prefix_bits
                };

                let separator_byte_start = if prefix_bytes == 0 {
                    0
                } else if prefix_bits % 8 == 0 {
                    prefix_bytes
                } else {
                    prefix_bytes - 1
                };
                let separator_byte_len = ((separator_bit_len as usize + 7) / 8).next_multiple_of(8);

                group.bench_function(BenchmarkId::new("prefix_len_bits", prefix_bits), |b| {
                    b.iter_batched(
                        || {
                            (
                                (&key[0..prefix_bytes], prefix_bits),
                                (
                                    &key[separator_byte_start..][..separator_byte_len],
                                    separator_bit_start,
                                    separator_bit_len,
                                ),
                            )
                        },
                        |(prefix, separator)| super::reconstruct_key(Some(prefix), separator),
                        criterion::BatchSize::SmallInput,
                    )
                });
            }

            group.finish();
        }
    }

    pub fn separator_len_benchmark(c: &mut Criterion) {
        let mut group = c.benchmark_group("separator_len");

        // n_bytes represents the amount of bytes set to one
        // from the beginning of the key
        for n_bytes in [16, 20, 24, 28, 31].into_iter().rev() {
            let mut separator = [0; 32];
            for byte in separator.iter_mut().take(n_bytes) {
                *byte = 255;
            }

            group.bench_function(BenchmarkId::new("zero_bytes", 32 - n_bytes), |b| {
                b.iter(|| super::separator_len(&separator));
            });
        }

        group.finish();
    }

    pub fn prefix_len_benchmark(c: &mut Criterion) {
        let mut group = c.benchmark_group("prefix_len");

        for prefix_len_bytes in [0, 4, 8, 12, 16] {
            let (key1, key2) = get_key_pair(prefix_len_bytes);
            group.bench_function(BenchmarkId::new("shared_bytes", prefix_len_bytes), |b| {
                b.iter(|| super::prefix_len(&key1, &key2));
            });
        }

        group.finish();
    }
}

#[cfg(test)]
mod tests {
    use crate::beatree::{
        branch::node::{RawPrefix, RawSeparators},
        Key,
    };
    use bitvec::{prelude::Msb0, view::BitView};

    fn reference_reconstruct_key(maybe_prefix: Option<RawPrefix>, separator: RawSeparators) -> Key {
        let mut key = [0; 32];

        let mut key_start_separator = 0;
        if let Some((prefix_bytes, prefix_bit_len)) = maybe_prefix {
            key.view_bits_mut::<Msb0>()[..prefix_bit_len]
                .copy_from_bitslice(&prefix_bytes.view_bits::<Msb0>()[..prefix_bit_len]);
            key_start_separator = prefix_bit_len;
        }

        let (separator_bytes, separator_bit_start, separator_bit_len) = separator;
        key.view_bits_mut::<Msb0>()[key_start_separator..][..separator_bit_len].copy_from_bitslice(
            &separator_bytes.view_bits::<Msb0>()[separator_bit_start..][..separator_bit_len],
        );

        key
    }

    fn reference_separate(a: &Key, b: &Key) -> Key {
        let len = reference_prefix_len(a, b) + 1;

        let mut separator = [0u8; 32];
        separator.view_bits_mut::<Msb0>()[..len].copy_from_bitslice(&b.view_bits::<Msb0>()[..len]);
        separator
    }

    fn reference_prefix_len(a: &Key, b: &Key) -> usize {
        a.view_bits::<Msb0>()
            .iter()
            .zip(b.view_bits::<Msb0>().iter())
            .take_while(|(a, b)| a == b)
            .count()
    }

    #[test]
    fn reconstruct_key_no_prefix() {
        // with no prefix the only possibilities are:
        // one iteration without shifts and then all the subsequent are left shifts
        for i in 0..8 {
            let separator_bit_start = i;
            let separator_bit_len = 256 - i;
            let separator_byte_len = ((separator_bit_len as usize + 7) / 8).next_multiple_of(8);

            let separator_bytes = vec![170; separator_byte_len];

            let separator = (&separator_bytes[..], separator_bit_start, separator_bit_len);
            let expected_key = reference_reconstruct_key(None, separator);
            let key = super::reconstruct_key(None, separator);

            assert_eq!(expected_key, key);
        }
    }

    #[test]
    fn reconstruct_key_shorter_separator() {
        // test separator smaller then 256 to ensure the garbage at the end is properly removed
        for separator_bit_len in 0..256 {
            let separator_bit_start = 0;
            let separator_byte_len = ((separator_bit_len as usize + 7) / 8).next_multiple_of(8);

            let separator_bytes = vec![170; separator_byte_len];

            let separator = (&separator_bytes[..], separator_bit_start, separator_bit_len);
            let expected_key = reference_reconstruct_key(None, separator);
            let key = super::reconstruct_key(None, separator);

            assert_eq!(expected_key, key);
        }
    }

    #[test]
    fn reconstruct_key_garbage_in_last_remainder() {
        // There could be the possibility of having garbage in the last remainder
        for left_and_right in 0..2 {
            for prefix_bit_len in 0..8 {
                let prefix = [255];

                let separator_bit_start_range = if left_and_right == 0 {
                    prefix_bit_len..=7
                } else {
                    0..=prefix_bit_len
                };

                for separator_bit_start in separator_bit_start_range {
                    for separator_bit_len in prefix_bit_len..=256 - prefix_bit_len {
                        let separator_byte_len =
                            ((separator_bit_start + separator_bit_len + 7 as usize) / 8)
                                .next_multiple_of(8);
                        let separator_bytes = vec![255; separator_byte_len];

                        let separator =
                            (&separator_bytes[..], separator_bit_start, separator_bit_len);
                        let prefix = (&prefix[..], prefix_bit_len);
                        let expected_key = reference_reconstruct_key(Some(prefix), separator);
                        let key = super::reconstruct_key(Some(prefix), separator);

                        assert_eq!(expected_key, key);
                    }
                }
            }
        }
    }

    #[test]
    fn reconstruct_key_no_shift() {
        // No shift means that the separator bit start is just right
        // after the end of the prefix, thus no shift for the separator
        // is required but just an overlap of the common byte between
        // prefix and separator
        for i in 0..8 {
            let mut prefix_bytes = [0; 3];
            if i != 0 {
                prefix_bytes[2] = 1 << (8 - i);
            }
            let prefix_bit_len = 16 + i;
            let separator_bit_start = i;
            let separator_bit_len = 256 - prefix_bit_len;
            let separator_byte_len = ((separator_bit_len as usize + 7) / 8).next_multiple_of(8);

            let separator_bytes = vec![170; separator_byte_len];

            let prefix = Some((&prefix_bytes[..], prefix_bit_len));
            let separator = (&separator_bytes[..], separator_bit_start, separator_bit_len);
            let expected_key = reference_reconstruct_key(prefix, separator);
            let key = super::reconstruct_key(prefix, separator);

            assert_eq!(expected_key, key);
        }
    }

    #[test]
    fn reconstruct_key_left_shift() {
        // Given a prefix that ends at all bit offset possibilities,
        // tests all the separators that start after the prefix ends
        for i in 0..8 {
            let mut prefix_bytes = [0; 3];
            if i != 0 {
                prefix_bytes[2] = 1 << (8 - i);
            }
            let prefix_bit_len = 16 + i;

            for separator_bit_start_offset in 1..(8 - i) {
                let separator_bit_start = i + separator_bit_start_offset;
                let separator_bit_len = 256 - prefix_bit_len;
                let separator_byte_len = ((separator_bit_len as usize + 7) / 8).next_multiple_of(8);

                let separator_bytes = vec![170; separator_byte_len];

                let prefix = Some((&prefix_bytes[..], prefix_bit_len));
                let separator = (&separator_bytes[..], separator_bit_start, separator_bit_len);

                let expected_key = reference_reconstruct_key(prefix, separator);
                let key = super::reconstruct_key(prefix, separator);

                assert_eq!(expected_key, key);
            }
        }
    }

    #[test]
    fn reconstruct_key_right_shift() {
        // Given a prefix that ends at all bit offset possibilities,
        // tests all the separators that start before the prefix ends
        for i in 0..8 {
            let mut prefix_bytes = [0; 3];
            if i != 0 {
                prefix_bytes[2] = 1 << (8 - i);
            }
            let prefix_bit_len = 16 + i;

            for separator_bit_start_offset in 0..i {
                let separator_bit_start = separator_bit_start_offset;
                let separator_bit_len = 256 - prefix_bit_len;
                let separator_byte_len = ((separator_bit_len as usize + 7) / 8).next_multiple_of(8);

                let separator_bytes = vec![170; separator_byte_len];

                let prefix = Some((&prefix_bytes[..], prefix_bit_len));
                let separator = (&separator_bytes[..], separator_bit_start, separator_bit_len);

                let expected_key = reference_reconstruct_key(prefix, separator);
                let key = super::reconstruct_key(prefix, separator);

                assert_eq!(expected_key, key);
            }
        }
    }

    #[test]
    fn separate() {
        for prefix_bit_len in 0..256 {
            let mut a = [255; 32];
            a.view_bits_mut::<Msb0>()[prefix_bit_len..].fill(false);
            let b = [255; 32];

            let expected_res = reference_separate(&a, &b);
            let res = super::separate(&a, &b);

            assert_eq!(expected_res, res);
        }
    }

    #[test]
    fn prefix_len() {
        for prefix_bit_len in 0..256 {
            let mut a = [255; 32];
            a.view_bits_mut::<Msb0>()[prefix_bit_len..].fill(false);
            let b = [255; 32];

            let expected_res = reference_prefix_len(&a, &b);
            let res = super::prefix_len(&a, &b);

            assert_eq!(expected_res, res);
        }
    }

    #[test]
    fn separator_len() {
        for separator_bit_len in 0..257 {
            let mut a = [255; 32];
            if separator_bit_len != 257 {
                a.view_bits_mut::<Msb0>()[separator_bit_len..].fill(false);
            }

            let expected_res = if a == [0u8; 32] {
                1
            } else {
                256 - a.view_bits::<Msb0>().trailing_zeros()
            };
            let res = super::separator_len(&a);

            assert_eq!(expected_res, res);
        }
    }
}
