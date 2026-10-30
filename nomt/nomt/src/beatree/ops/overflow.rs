//! Overflow pages are used to store values which exceed the maximum size.
//!
//! Large values are chunked into pages in a deterministic way, optimized for parallel fetching.
//!
//! The format of an overflow page is:
//! ```rust,ignore
//! n_pointers: u16
//! n_bytes: u16
//! pointers: [PageNumber; n_pointers]
//! bytes: [u8; n_bytes]
//! ```
use crate::{
    beatree::{
        allocator::{StoreReader, SyncAllocator},
        leaf::node::{MAX_OVERFLOW_CELL_NODE_POINTERS, MAX_OVERFLOW_VALUE_SIZE},
        PageNumber,
    },
    io::{page_pool::FatPage, IoCommand, IoHandle, IoKind, PagePool, PAGE_SIZE},
};

const BODY_SIZE: usize = PAGE_SIZE - 4;
const MAX_PNS: usize = BODY_SIZE / 4;
const HEADER_SIZE: usize = 4;

/// Encode a large value into freshly allocated overflow pages. Returns a vector of page pointers
/// and the total number of page writes submitted.
pub fn chunk(
    value: &[u8],
    leaf_writer: &SyncAllocator,
    page_pool: &PagePool,
    io_handle: &IoHandle,
) -> std::io::Result<(Vec<PageNumber>, usize)> {
    assert!(!value.is_empty());

    let total_pages = total_needed_pages(value.len());
    let cell_pages = std::cmp::min(total_pages, MAX_OVERFLOW_CELL_NODE_POINTERS);
    let cell = (0..cell_pages)
        .map(|_| leaf_writer.allocate())
        .collect::<std::io::Result<Vec<_>>>()?;
    let other_pages = (0..total_pages)
        .skip(cell_pages)
        .map(|_| leaf_writer.allocate())
        .collect::<std::io::Result<Vec<_>>>()?;

    let all_pages = cell.iter().cloned().chain(other_pages.iter().cloned());
    let mut to_write = other_pages.iter().cloned();

    let mut value = value;
    // loop over all page numbers.
    for pn in all_pages {
        assert!(!value.is_empty());

        // allocate a page.
        let mut page = page_pool.alloc_fat_page();
        let mut pns_written = 0;

        // write as many page numbers as possible.
        while pns_written < MAX_PNS {
            let Some(pn) = to_write.next() else { break };
            let start = HEADER_SIZE + pns_written * 4;
            let end = start + 4;
            page[start..end].copy_from_slice(&pn.0.to_le_bytes());
            pns_written += 1;
        }

        // then write as many value bytes as possible.
        let bytes = std::cmp::min(BODY_SIZE - pns_written * 4, value.len());

        // write the header.
        page[0..2].copy_from_slice(&(pns_written as u16).to_le_bytes());
        page[2..4].copy_from_slice(&(bytes as u16).to_le_bytes());

        let start = HEADER_SIZE + pns_written * 4;
        let end = start + bytes;
        page[start..end].copy_from_slice(&value[..bytes]);
        value = &value[bytes..];

        // write the page.
        let command = IoCommand {
            kind: IoKind::Write(leaf_writer.store_fd(), pn.0 as u64, page),
            user_data: 0,
        };
        io_handle.send(command).expect("I/O Pool Down");
    }
    assert!(value.is_empty());

    Ok((cell, total_pages))
}

/// Decode an overflow cell, returning the size of the value plus the pages numbers within the cell.
pub fn decode_cell<'a>(raw: &'a [u8]) -> (usize, [u8; 32], impl Iterator<Item = PageNumber> + 'a) {
    // the minimum legal size is the length plus one page pointer plus the value hash.
    assert!(raw.len() >= 8 + 4 + 32);
    assert_eq!(raw.len() % 4, 0);

    let value_size = u64::from_le_bytes(raw[0..8].try_into().unwrap()) as usize;
    // values bigger than MAX_OVERFLOW_VALUE_SIZE are not allowed.
    assert!(value_size <= MAX_OVERFLOW_VALUE_SIZE);

    let value_hash: [u8; 32] = raw[8..40].try_into().unwrap();

    let iter = raw[40..]
        .chunks(4)
        .map(|slice| PageNumber(u32::from_le_bytes(slice.try_into().unwrap())));

    (value_size, value_hash, iter)
}

/// Encode a list of page numbers into an overflow cell.
pub fn encode_cell(value_size: usize, value_hash: [u8; 32], pages: &[PageNumber]) -> Vec<u8> {
    if value_size > MAX_OVERFLOW_VALUE_SIZE {
        panic!("Value size exceeded MAX_OVERFLOW_VALUE_SIZE");
    }

    let mut v = vec![0u8; 8 + 32 + pages.len() * 4];
    v[0..8].copy_from_slice(&(value_size as u64).to_le_bytes());
    v[8..40].copy_from_slice(&value_hash);
    for (pn, slice) in pages.iter().zip(v[40..].chunks_mut(4)) {
        slice.copy_from_slice(&pn.0.to_le_bytes());
    }

    v
}

fn total_needed_pages(value_size: usize) -> usize {
    // the encoded size is equal to the size of the value plus the number of node pointers that
    // will appear in pages.
    let needed_pages_raw_value = needed_pages(value_size);

    if needed_pages_raw_value <= MAX_OVERFLOW_CELL_NODE_POINTERS {
        // less than MAX_OVERFLOW_CELL_NODE_POINTERS,
        // space available in the leaf is enough
        return needed_pages_raw_value;
    }

    // not all needed_pages_raw_values are used to store values.
    // Therefore, there will be unused space in one of the pages
    // to store other page numbers
    let bytes_left = (needed_pages_raw_value * BODY_SIZE) - value_size;
    let available_page_numbers = bytes_left / 4;

    if needed_pages_raw_value <= MAX_OVERFLOW_CELL_NODE_POINTERS + available_page_numbers {
        // there are enough available bytes to store all the remaining page numbers
        return needed_pages_raw_value;
    }

    // additional pages are required
    //
    // given:
    // value_size = vs
    // MAX_OVERFLOW_CELL_NODE_POINTERS = M
    // needed_pages_raw_value = np
    // body_size = bs
    // required_additional_pages = rp
    //
    // total bytes available:
    // tot_bytes = (np + rp) * bs
    //
    // only a portion of the total available bytes will be used:
    // used_bytes = vs - ((np - M)) * 4 - (rp * 4)
    //
    // we want tot_bytes - used_bytes to be positive and as little as possible,
    // the only variable is rp, thus we can make everything dependent on rp:
    //
    // rp = ceil(n / (bs - 4))
    // where n = (vs + (np - M) * 4 - (np * bs))

    let n = value_size + (needed_pages_raw_value - MAX_OVERFLOW_CELL_NODE_POINTERS) * 4
        - (needed_pages_raw_value * BODY_SIZE);

    let required_additional_pages = (n + BODY_SIZE - 3) / (BODY_SIZE - 4);

    needed_pages_raw_value + required_additional_pages
}

fn needed_pages(size: usize) -> usize {
    (size + BODY_SIZE - 1) / BODY_SIZE
}

/// Read a large value from pages referenced by an overflow cell using blocking I/O.
pub fn read_blocking(cell: &[u8], leaf_reader: &StoreReader) -> Vec<u8> {
    let (value_size, _, cell_pages) = decode_cell(cell);
    let total_pages = total_needed_pages(value_size);

    let mut value = Vec::with_capacity(value_size);

    let mut page_numbers = Vec::with_capacity(total_pages);
    page_numbers.extend(cell_pages);

    for i in 0..total_pages {
        let page = leaf_reader.query(page_numbers[i]);
        let (page_pns, bytes) = parse_page(&page);
        page_numbers.extend(page_pns);
        value.extend(bytes);
    }

    assert_eq!(page_numbers.len(), total_pages);
    assert_eq!(value.len(), value_size);

    value
}

/// A non-blocking reader for an overflow value.
pub struct AsyncReader {
    value: Vec<u8>,
    pages: Vec<(PageNumber, Option<FatPage>)>,
    // the index of the next page to request
    request_index: usize,
    // the index of the next page to process.
    process_index: usize,
    store_reader: StoreReader,
    value_size: usize,
    total_pages: usize,
}

impl AsyncReader {
    /// Create a new async reader.
    pub fn new(cell: &[u8], store_reader: StoreReader) -> Self {
        let (value_size, _, cell_pages) = decode_cell(cell);
        let total_pages = total_needed_pages(value_size);

        let value = Vec::with_capacity(value_size);

        let mut pages = Vec::with_capacity(total_pages);
        pages.extend(cell_pages.into_iter().map(|pn| (pn, None)));

        AsyncReader {
            value,
            pages,
            request_index: 0,
            process_index: 0,
            store_reader,
            value_size,
            total_pages,
        }
    }

    /// Submit a request over the I/O handle.
    ///
    /// Returns `Some` with an index if a request was submitted. Otherwise, `None`.
    pub fn submit(&mut self, io_handle: &IoHandle, user_data: u64) -> Option<usize> {
        if self.is_done_requesting() {
            return None;
        }

        let page_index = self.request_index;
        let next_page = self.pages[page_index].0;
        self.request_index += 1;

        let command = self.store_reader.io_command(next_page, user_data);
        let _ = io_handle.send(command);

        Some(page_index)
    }

    /// Provide a completion.
    ///
    /// This may panic if the index provided is out of range. Likewise, it may read garbage data.
    ///
    /// If this returns `Some`, then that is the value and this reader should no longer be used.
    pub fn complete(&mut self, index: usize, page: FatPage) -> Option<Vec<u8>> {
        self.pages[index].1 = Some(page);

        if index == self.process_index {
            self.continue_parse()
        }

        if self.is_done() {
            assert_eq!(self.pages.len(), self.total_pages);
            assert_eq!(self.value.len(), self.value_size);

            Some(std::mem::take(&mut self.value))
        } else {
            None
        }
    }

    fn is_done_requesting(&self) -> bool {
        self.request_index == self.total_pages
    }

    fn is_done(&self) -> bool {
        self.process_index == self.total_pages
    }

    fn continue_parse(&mut self) {
        while self.process_index < self.total_pages {
            let Some(page) = self.pages[self.process_index].1.take() else {
                break;
            };

            let (page_pns, bytes) = parse_page(&page);
            self.pages.extend(page_pns.into_iter().map(|pn| (pn, None)));
            self.value.extend(bytes);

            self.process_index += 1;
        }
    }
}

/// Iterate all pages related to an overflow cell and push onto a free-list.
///
/// This only logically deletes the pages.
pub fn delete(cell: &[u8], leaf_reader: &StoreReader, freed: &mut Vec<PageNumber>) {
    let (value_size, _, cell_pages) = decode_cell(cell);
    let total_pages = total_needed_pages(value_size);

    let start = freed.len();
    freed.extend(cell_pages);

    for i in 0..total_pages {
        let page = leaf_reader.query(freed[start + i]);
        let (page_pns, bytes) = parse_page(&page);
        freed.extend(page_pns);

        // stop at the first page containing value data. no more pages will have more
        // page numbers to release.
        if bytes.len() > 0 {
            break;
        }
    }

    assert_eq!(freed.len() - start, total_pages);
}

fn parse_page<'a>(page: &'a FatPage) -> (impl Iterator<Item = PageNumber> + 'a, &'a [u8]) {
    let n_pages = u16::from_le_bytes(page[0..2].try_into().unwrap()) as usize;
    let n_bytes = u16::from_le_bytes(page[2..4].try_into().unwrap()) as usize;

    let iter = page[HEADER_SIZE..][..n_pages * 4]
        .chunks(4)
        .map(|slice| PageNumber(u32::from_le_bytes(slice.try_into().unwrap())));

    let bytes = &page[HEADER_SIZE + n_pages * 4..][..n_bytes];
    (iter, bytes)
}

#[cfg(test)]
mod tests {
    use crate::beatree::leaf::node::MAX_OVERFLOW_VALUE_SIZE;

    use super::{
        decode_cell, encode_cell, needed_pages, total_needed_pages, PageNumber, BODY_SIZE,
        MAX_OVERFLOW_CELL_NODE_POINTERS, MAX_PNS,
    };
    use quickcheck::{Arbitrary, Gen, QuickCheck, TestResult};

    #[test]
    fn total_needed_pages_all_in_cell() {
        for i in 1..=MAX_OVERFLOW_CELL_NODE_POINTERS {
            assert_eq!(total_needed_pages(BODY_SIZE * i), i);
        }
    }

    #[test]
    fn total_needed_pages_one_out_of_cell() {
        let size = BODY_SIZE * MAX_OVERFLOW_CELL_NODE_POINTERS + 1;
        assert_eq!(
            total_needed_pages(size),
            MAX_OVERFLOW_CELL_NODE_POINTERS + 1
        );
    }

    #[test]
    fn total_needed_pages_encoded_page_adds_more() {
        // last page is totally full
        let size = BODY_SIZE * (MAX_OVERFLOW_CELL_NODE_POINTERS + 1) - 4;
        assert_eq!(
            total_needed_pages(size),
            MAX_OVERFLOW_CELL_NODE_POINTERS + 1
        );

        // last page not full
        let size = BODY_SIZE * (MAX_OVERFLOW_CELL_NODE_POINTERS + 1) - 3;
        assert_eq!(
            total_needed_pages(size),
            MAX_OVERFLOW_CELL_NODE_POINTERS + 2
        );
    }

    #[test]
    fn total_needed_pages_additional_pages_adds_more() {
        let size = (BODY_SIZE * MAX_OVERFLOW_CELL_NODE_POINTERS) + (BODY_SIZE * MAX_PNS);
        assert_eq!(
            total_needed_pages(size),
            // one page is used to save MAX_PNS page numbers,
            // and another page is used to save the page number of the previous one
            MAX_OVERFLOW_CELL_NODE_POINTERS + MAX_PNS + 1 + 1
        );
    }

    #[test]
    fn total_needed_pages_really_big() {
        let size = 1 << 30;

        // this many pages for the value
        let pages0 = 262401;
        assert_eq!(needed_pages(size), pages0);

        let pages_in_pages0 = pages0 - MAX_OVERFLOW_CELL_NODE_POINTERS;

        // this many pages for the value plus those pages
        let size1 = size + pages_in_pages0 * 4;
        let pages1 = needed_pages(size1);

        let pages_in_pages1 = pages1 - MAX_OVERFLOW_CELL_NODE_POINTERS - pages_in_pages0;

        // this many pages for the value plus _those_ pages
        let size2 = size1 + pages_in_pages1 * 4;
        let pages2 = needed_pages(size2);

        assert_eq!(pages1, pages2);
        assert_eq!(pages1, total_needed_pages(size));
    }

    #[derive(Debug, Clone)]
    struct ValidOverflowCell {
        value_size: usize,
        value_hash: [u8; 32],
        pages: Vec<PageNumber>,
    }

    impl Arbitrary for ValidOverflowCell {
        fn arbitrary(g: &mut Gen) -> Self {
            let value_size = (usize::arbitrary(g) % MAX_OVERFLOW_VALUE_SIZE) + 1;
            let mut value_hash = [0u8; 32];
            for b in value_hash.iter_mut() {
                *b = u8::arbitrary(g);
            }

            // Generate 1 to MAX_OVERFLOW_CELL_NODE_POINTERS page numbers
            let len = (usize::arbitrary(g) % MAX_OVERFLOW_CELL_NODE_POINTERS) + 1;
            let pages = (0..len).map(|_| PageNumber(u32::arbitrary(g))).collect();

            ValidOverflowCell {
                value_size,
                value_hash,
                pages,
            }
        }
    }

    #[test]
    fn test_encode_decode_cell_roundtrip() {
        fn prop(cell: ValidOverflowCell) -> bool {
            let encoded = encode_cell(cell.value_size, cell.value_hash, &cell.pages);
            let (decoded_size, decoded_hash, decoded_pages) = decode_cell(&encoded);

            let pages_match = decoded_pages.eq(cell.pages.iter().cloned());

            decoded_size == cell.value_size && decoded_hash == cell.value_hash && pages_match
        }

        QuickCheck::new()
            .tests(5000000)
            .quickcheck(prop as fn(ValidOverflowCell) -> bool);
    }

    #[test]
    fn test_decode_cell_safety() {
        fn prop(mut bytes: Vec<u8>) -> TestResult {
            // Only test vectors that could potentially be valid cells
            // (must be at least 44 bytes and a multiple of 4)
            let len = std::cmp::max(bytes.len(), 44);
            // make sure the length is a multiple of 4
            let len = (len + 3) & !3;
            bytes.resize(len, 0);

            // make sure value size is smaller than MAX_OVERFLOW_VALUE_SIZE
            let value_size = u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as usize;
            if value_size > MAX_OVERFLOW_VALUE_SIZE {
                return TestResult::discard();
            }

            // decode_cell should not panic
            let result = std::panic::catch_unwind(|| {
                let _ = decode_cell(&bytes);
            });

            TestResult::from_bool(result.is_ok())
        }

        QuickCheck::new()
            .tests(5000000)
            .quickcheck(prop as fn(Vec<u8>) -> TestResult);
    }
}
