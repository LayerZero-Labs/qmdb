use super::utils::poslist::PosList;

pub trait IndexerTrait: Send + Sync {
    /// Add a new key-value pair to the indexer.
    ///
    /// # Arguments
    ///
    /// * `key` - The key bytes to add
    /// * `position` - The position in the database where the value is stored
    /// * `serial_number` - A unique identifier for this entry
    fn add_kv(&self, key: &[u8], position: i64, serial_number: u64);

    /// Change an existing key-value pair in the indexer.
    ///
    /// # Arguments
    ///
    /// * `key` - The key bytes to change
    /// * `pos_old` - The old position in the database where the value is stored
    /// * `pos_new` - The new position in the database where the value is stored
    /// * `sn_old` - The old serial number for this entry
    /// * `sn_new` - The new serial number for this entry
    fn change_kv(&self, k80: &[u8], pos_old: i64, pos_new: i64, sn_old: u64, sn_new: u64);

    /// Erase a key-value pair from the indexer.
    ///
    /// # Arguments
    ///
    /// * `key` - The key bytes to erase
    /// * `position` - The position in the database where the value was stored
    /// * `serial_number` - A unique identifier for this entry
    fn erase_kv(&self, key: &[u8], position: i64, serial_number: u64);

    /// Iterate over all values for a key and (possibly) its previous key at a specific
    /// height in the indexer.
    ///
    /// # Arguments
    ///
    /// * `height` - Block height to search at (-1 for latest)
    /// * `key` - 80-bit key bytes to search for
    /// * `op` - if it equals OP_CREATE or OP_DELETE, this function is the same as
    /// for_each_adjacent_value; if it equals OP_WRITE or OP_READ, this function is the same as
    /// for_each_value
    ///
    /// # Returns
    ///
    /// Position list containing all matching file positions
    fn for_each(&self, h: i64, op: u8, k80: &[u8]) -> PosList;

    /// It's used in 'warmup_cache'. It can warm up the indexer's internal cache (if any).
    fn for_each_value_warmup(&self, h: i64, k80: &[u8]) -> PosList;

    /// Iterate over all values associated with a key at a specific height in the indexer.
    ///
    /// # Arguments
    ///
    /// * `height` - Block height to search at (-1 for latest)
    /// * `key` - 80-bit key bytes to search for
    ///
    /// # Returns
    ///
    /// Position list containing all matching file positions
    fn for_each_value(&self, h: i64, k80: &[u8]) -> PosList;

    /// Iterate over all values for a key and its previous key at a specific height in the indexer.
    ///
    /// # Arguments
    ///
    /// * `height` - Block height to search at (-1 for latest)
    /// * `key` - 80-bit key bytes to search for
    ///
    /// # Returns
    ///
    /// Position list containing all matching file positions for adjacent keys
    fn for_each_adjacent_value(&self, _h: i64, k80: &[u8]) -> PosList;

    /// Check if a key exists in the indexer.
    ///
    /// # Arguments
    ///
    /// * `key` - The key bytes to check
    ///
    /// # Returns
    ///
    /// `true` if the key exists, `false` otherwise
    fn key_exists(&self, k80: &[u8], file_pos: i64, sn: u64) -> bool;

    /// Get the number of entries in the indexer.
    ///
    /// # Returns
    ///
    /// The number of entries in the index
    fn len(&self, shard_id: usize) -> usize;
}
