use kvdb::{DBOp, DBTransaction, DBValue, KeyValueDB};
use std::io::Result;
use std::sync::Arc;

#[derive(Clone)]
pub struct DBColumn {
    db: Arc<dyn KeyValueDB>,
    col: u32,
}

impl DBColumn {
    pub fn from_kvdb(db: Arc<dyn KeyValueDB>, col: u32) -> Self {
        Self { db, col }
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<DBValue>> {
        self.db.get(self.col, key)
    }

    pub fn write_buffered(&self, mut transaction: DBTransaction) {
        let ops = &mut transaction.ops;
        ops.iter_mut().for_each(|x| match x {
            DBOp::Insert { col, .. } => *col = self.col,
            DBOp::Delete { col, .. } => *col = self.col,
        });
        self.db.write_buffered(transaction)
    }
}
