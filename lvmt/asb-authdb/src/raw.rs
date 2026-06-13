use kvdb::KeyValueDB;
use std::sync::Arc;

pub fn new(backend: Arc<dyn KeyValueDB>) -> Arc<dyn KeyValueDB> {
    backend
}
