pub mod access;
pub mod kvdb;
pub mod layout;

pub use self::access::DBAccess;
pub use self::kvdb::DBColumn;
pub use self::layout::{FlattenArray, FlattenTree, LayoutTrait};
pub use self::test_tools::{test_db_col, test_kvdb};

mod test_tools {
    use super::DBColumn;
    use kvdb::KeyValueDB;
    use std::sync::Arc;

    pub fn test_db_col() -> DBColumn {
        DBColumn::from_kvdb(Arc::new(kvdb_memorydb::create(1)), 0)
    }

    pub fn test_kvdb(num_cols: u32) -> Arc<dyn KeyValueDB> {
        Arc::new(kvdb_memorydb::create(num_cols))
    }
}
