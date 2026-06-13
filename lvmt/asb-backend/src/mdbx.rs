#![allow(unused)]

use libmdbx::Cursor;
use libmdbx::Database;
use libmdbx::DatabaseFlags;
use libmdbx::Environment;
use libmdbx::EnvironmentBuilder;
use libmdbx::Geometry;
use libmdbx::Transaction;
use libmdbx::WriteFlags;
use libmdbx::WriteMap;
use libmdbx::RW;

use asb_options::Options;

use kvdb::DBValue;
use kvdb::KeyValueDB;
use std::io;
use std::io::ErrorKind::Other;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::RwLockWriteGuard;

use ouroboros::self_referencing;
use std::path::Path;

pub fn open_database(opts: &Options) -> MdbxDatabase {
    const TB: usize = 1 << 40;
    const GB: usize = 1 << 30;
    let mut builder: EnvironmentBuilder<WriteMap> = Environment::new();
    builder.set_max_dbs(10);
    builder.set_geometry(Geometry {
        size: Some(0..4 * TB),
        growth_step: Some(4 * GB as isize),
        shrink_threshold: None,
        page_size: None,
    });
    builder.set_rp_augment_limit(16 * 256 * 1024);
    let env = builder.open(Path::new(&opts.db_dir)).unwrap();
    let txn = MdbxTransaction::build(Arc::new(env));
    make_backend(opts, txn)
}

#[self_referencing]
pub struct MdbxTransaction {
    env: Arc<Environment<WriteMap>>,
    #[borrows(env)]
    #[covariant]
    txn: Option<Transaction<'this, RW, WriteMap>>,
}

impl MdbxTransaction {
    pub fn build(env: Arc<Environment<WriteMap>>) -> Self {
        MdbxTransactionBuilder {
            env,
            txn_builder: |env| Some(env.begin_rw_txn().unwrap()),
        }
        .build()
    }

    fn commit(&mut self) {
        self.with_txn_mut(|txn| std::mem::take(txn).unwrap().commit());
        let mut next_txn = MdbxTransaction::build(self.borrow_env().clone());
        std::mem::swap(self, &mut next_txn);
    }
}

pub struct MdbxDatabase {
    txn: RwLock<MdbxTransaction>,
    buffer: RwLock<Vec<kvdb::DBTransaction>>,
    num_cols: u32,
}

impl MdbxDatabase {
    fn cursor(&self, col: u32) -> Cursor<'_, RW> {
        let table_name = format!("table{}", col);
        self.txn.write().unwrap().with_txn_mut(|txn| {
            let txn = txn.as_mut().unwrap();
            let db = txn
                .create_db(Some(&table_name), DatabaseFlags::empty())
                .unwrap();
            txn.cursor(&db).unwrap()
        })
    }
}

pub fn make_backend(opts: &Options, db: MdbxTransaction) -> MdbxDatabase {
    let num_cols = opts.num_cols();
    MdbxDatabase {
        txn: RwLock::new(db),
        buffer: RwLock::new(Vec::new()),
        num_cols,
    }
}

fn into_io_error(err: libmdbx::Error) -> io::Error {
    io::Error::new(Other, err)
}

fn map_output<T, F>(
    output: libmdbx::Result<Option<(Vec<u8>, Vec<u8>)>>,
    f: F,
) -> io::Result<Option<T>>
where
    F: FnOnce((Vec<u8>, Vec<u8>)) -> T,
{
    match output {
        Ok(out) => Ok(out.map(f)),
        Err(err) => Err(into_io_error(err)),
    }
}

fn is_prefix(prefix: &[u8], full: &[u8]) -> bool {
    if prefix.len() > full.len() {
        return false;
    }
    full.starts_with(prefix)
}

impl KeyValueDB for MdbxDatabase {
    fn get(&self, col: u32, key: &[u8]) -> io::Result<Option<DBValue>> {
        map_output(self.cursor(col).set_key(key), |(_, v)| v)
    }

    fn get_by_prefix(&self, col: u32, prefix: &[u8]) -> Option<Box<[u8]>> {
        let value = map_output(self.cursor(col).set_range(prefix), |(k, v)| {
            is_prefix(prefix, &k).then(|| v.into_boxed_slice())
        })
        .unwrap();
        value.and_then(|x| x)
    }

    fn write_buffered(&self, transaction: kvdb::DBTransaction) {
        self.buffer.write().unwrap().push(transaction);
    }

    fn flush(&self) -> std::io::Result<()> {
        let mut cursors: Vec<_> = (0..self.num_cols).map(|db| self.cursor(db)).collect();
        for kvdb_txn in self.buffer.write().unwrap().drain(..) {
            for kvdb_op in kvdb_txn.ops {
                match kvdb_op {
                    kvdb::DBOp::Insert { col, key, value } => cursors[col as usize]
                        .put(key.as_ref(), value.as_ref(), WriteFlags::UPSERT)
                        .map_err(into_io_error)
                        .unwrap(),
                    kvdb::DBOp::Delete { col, key } => {
                        let mut cursor = &mut cursors[col as usize];
                        if cursor
                            .set_key::<(), ()>(&key)
                            .map_err(into_io_error)
                            .unwrap()
                            .is_some()
                        {
                            cursor
                                .del(WriteFlags::CURRENT)
                                .map_err(into_io_error)
                                .unwrap();
                        }
                    }
                }
            }
        }
        self.txn.write().unwrap().commit();
        // self.txn.read().unwrap().borrow_env().sync(true).unwrap();
        // println!("Commit");
        Ok(())
    }

    fn iter<'a>(&'a self, col: u32) -> Box<dyn Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a> {
        let mut cursor = self.cursor(col);
        cursor.first::<(), ()>().unwrap();
        return Box::new(MdbxCursorIterator {
            cursor,
            prefix: vec![],
        });
    }

    fn iter_from_prefix<'a>(
        &'a self,
        col: u32,
        prefix: &'a [u8],
    ) -> Box<dyn Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a> {
        let mut cursor = self.cursor(col);
        cursor.set_key::<(), ()>(prefix).unwrap();
        return Box::new(MdbxCursorIterator {
            cursor,
            prefix: prefix.to_vec(),
        });
    }

    fn restore(&self, new_db: &str) -> std::io::Result<()> {
        unimplemented!()
    }
}

pub struct MdbxCursorIterator<'txn> {
    cursor: Cursor<'txn, RW>,
    prefix: Vec<u8>,
}

impl<'txn> Iterator for MdbxCursorIterator<'txn> {
    type Item = (Box<[u8]>, Box<[u8]>);

    fn next(&mut self) -> Option<Self::Item> {
        let (k, v): Self::Item = map_output(self.cursor.get_current(), |(k, v)| {
            (k.into_boxed_slice(), v.into_boxed_slice())
        })
        .unwrap()?;
        if !is_prefix(&self.prefix, &v) {
            return None;
        }
        self.cursor.next::<(), ()>().unwrap();
        Some((k, v))
    }
}

impl parity_util_mem::MallocSizeOf for MdbxDatabase {
    fn size_of(&self, ops: &mut parity_util_mem::MallocSizeOfOps) -> usize {
        0
    }
}
