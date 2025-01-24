pub const BYTES_CACHE_SHARD_COUNT: usize = 32;
pub const BIG_BUF_SIZE: usize = 64 * 1024; // 64KB

pub const NONCE_SIZE: usize = 12;
pub const TAG_SIZE: usize = 16;

pub const ENTRY_FIXED_LENGTH: usize = 1 + 3 + 1 + 32 + 8 + 8;
#[cfg(feature = "tee_cipher")]
pub const ENTRY_BASE_LENGTH: usize = ENTRY_FIXED_LENGTH + TAG_SIZE;
#[cfg(not(feature = "tee_cipher"))]
pub const ENTRY_BASE_LENGTH: usize = ENTRY_FIXED_LENGTH;

pub const NULL_ENTRY_VERSION: i64 = -2;
pub const SHARD_COUNT: usize = 16; // Default value, can be overridden by config
pub const DEFAULT_ENTRY_SIZE: usize = 300;

pub const SENTRY_COUNT: usize = (1 << 16) / SHARD_COUNT;

pub const PRUNE_EVERY_NBLOCKS: i64 = 500;

pub const JOB_COUNT: usize = 2000;
pub const SUB_JOB_RESERVE_COUNT: usize = 50;
pub const PRE_READ_BUF_SIZE: usize = 256 * 1024;
pub const HPFILE_RANGE: i64 = 1i64 << 51; // 2048TB

pub const SHARD_DIV: usize = (1 << 16) / SHARD_COUNT;

pub const OP_READ: u8 = 1;
pub const OP_WRITE: u8 = 2;
pub const OP_CREATE: u8 = 3;
pub const OP_DELETE: u8 = 4;

pub const DEFAULT_FILE_SIZE: i64 = 1024 * 1024;
pub const SMALL_BUFFER_SIZE: i64 = 32 * 1024;

pub const FIRST_LEVEL_ABOVE_TWIG: i64 = 13;
pub const TWIG_ROOT_LEVEL: i64 = FIRST_LEVEL_ABOVE_TWIG - 1; // 12
pub const MIN_PRUNE_COUNT: u64 = 2;
pub const CODE_PATH: &str = "code";
pub const ENTRIES_PATH: &str = "entries";
pub const TWIG_PATH: &str = "twig";
pub const TWIG_SHARD_COUNT: usize = 4;
pub const NODE_SHARD_COUNT: usize = 4;
pub const MAX_TREE_LEVEL: usize = 64;
pub const MAX_UPPER_LEVEL: usize = MAX_TREE_LEVEL - FIRST_LEVEL_ABOVE_TWIG as usize; // 51

pub const TWIG_SHIFT: u32 = 11; // a twig has 2**11 leaves
pub const LEAF_COUNT_IN_TWIG: u32 = 1 << TWIG_SHIFT; // 2**11==2048
pub const TWIG_MASK: u32 = LEAF_COUNT_IN_TWIG - 1;

pub const IN_BLOCK_IDX_BITS: usize = 24;
pub const IN_BLOCK_IDX_MASK: i64 = (1 << IN_BLOCK_IDX_BITS) - 1;

pub const COMPACT_RING_SIZE: usize = 1024;

pub fn is_compactible(
    utilization_div: i64,
    utilization_ratio: i64,
    compact_thres: i64,
    active_count: usize,
    sn_start: u64,
    sn_end: u64,
) -> bool {
    let total_count = sn_end - sn_start;
    /*
    ratio   active
    ----- < ------    ==> false
     div    total
     */
    if ((total_count as usize * utilization_ratio as usize)
        < (active_count * utilization_div as usize))
        || active_count < compact_thres as usize
    {
        return false; // no need to compact
    }
    true
}

pub fn calc_max_level(youngest_twig_id: u64) -> i64 {
    FIRST_LEVEL_ABOVE_TWIG + 63 - youngest_twig_id.leading_zeros() as i64
}
