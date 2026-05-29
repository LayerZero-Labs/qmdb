//! Task ID encoding and decoding utilities.
//!
//! This module provides functions for encoding and decoding task IDs used in the QMDB database.
//! Task IDs are 64-bit integers that encode:
//! - Block height (bits 24-62)
//! - Index within block (bits 0-23)
//! - End-of-block flag (bit 63)
//!
//! # Encoding Format
//!
//! ```text
//! Bit layout of a 64-bit task ID:
//!    63  62         24 23          0
//! |     |             |             |
//! | END |   HEIGHT    |   INDEX     |
//! |     |             |             |
//! ```
//!
//! - Bits 0-23: Index within the block (24 bits, up to 16M Txns per block)
//! - Bits 24-62: Block height (39 bits)
//! - Bit 63: End-of-block flag
//!
//! # Examples
//!
//! ```
//! use qmdb_common::task::taskid::{join_task_id, split_task_id};
//!
//! let task_id = join_task_id(100, 42, false);
//! let (height, index) = split_task_id(task_id);
//! assert_eq!(height, 100);
//! assert_eq!(index, 42);
//! ```

/// Number of bits used for the index within a block (24 bits = 16M max txns per block)
const IN_BLOCK_IDX_BITS: usize = 24;

/// Mask for extracting the index bits (bits 0-23)
const IN_BLOCK_IDX_MASK: i64 = (1 << IN_BLOCK_IDX_BITS) - 1;

/// Flag bit indicating the end of a block (bit 63)
const END_BLOCK_FLAG: i64 = 1 << 63;

/// Mask for extracting the task ID without the end-of-block flag
const TASK_ID_MASK: i64 = !(END_BLOCK_FLAG);

/// Splits a task ID into its component parts.
///
/// Extracts the block height and index from a task ID, ignoring the end-of-block flag.
#[inline]
pub fn split_task_id(mut task_id: i64) -> (i64, usize) {
    task_id &= TASK_ID_MASK;
    let height = task_id >> IN_BLOCK_IDX_BITS;
    let index = (task_id & IN_BLOCK_IDX_MASK) as usize;
    (height, index)
}

/// Joins height, index, and end-of-block flag into a task ID.
///
/// Creates a task ID by combining the block height, index within the block,
/// and an optional end-of-block flag. Entry versions use the same encoding rule as task IDs.
#[inline]
pub fn join_task_id(height: i64, idx: usize, end_block: bool) -> i64 {
    assert!((idx >> IN_BLOCK_IDX_BITS) == 0, "index out of range");
    let mut id = (height << IN_BLOCK_IDX_BITS) | (idx as i64);
    if end_block {
        id |= 1 << 63;
    }
    id
}

/// Removes the end-of-block flag from a task ID.
///
/// Clears bit 63 (the end-of-block flag) while preserving the height and index information.
#[inline]
pub fn exclude_end_block(task_id: i64) -> i64 {
    task_id & TASK_ID_MASK
}

/// Checks if a task ID represents the first task in a block.
///
/// A task is the first in a block if its index (bits 0-23) is zero.
#[inline]
pub fn is_first_task_in_block(task_id: i64) -> bool {
    (task_id & IN_BLOCK_IDX_MASK) == 0
}

/// Checks if a task ID represents the last task in a block.
///
/// A task is the last in a block if the end-of-block flag (bit 63) is set.
#[inline]
pub fn is_end_task_in_block(task_id: i64) -> bool {
    (task_id & END_BLOCK_FLAG) != 0
}
