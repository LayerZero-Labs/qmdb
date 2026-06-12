use std::sync::Barrier;

pub struct BarrierSet {
    /// Barrier for coordinating flush operations across shards
    pub flush_bar: Barrier,
    /// Barrier for coordinating metadata updates across shards
    pub metadb_bar: Barrier,
}

impl BarrierSet {
    /// Creates a new BarrierSet for coordinating the specified number of shards.
    ///
    /// # Arguments
    /// * `n` - Number of shards to synchronize
    ///
    /// # Performance
    /// - Constant-time initialization
    /// - Scales efficiently with shard count
    pub fn new(n: usize) -> Self {
        Self {
            flush_bar: Barrier::new(n),
            metadb_bar: Barrier::new(n),
        }
    }
}
