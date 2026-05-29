use bytemuck::{Pod, Zeroable};

#[repr(C)]
/// Metadata for a shard in the scene
#[derive(Clone, Copy, Debug, Pod, Zeroable, Default, PartialEq, Eq)]
pub struct ShardMeta {
    /// Starting serial number for this shard
    pub sn_start: u64,
    /// Ending serial number for this shard
    pub sn_end: u64,
    /// Number of active entries in this shard
    pub active_count: u64,
}
