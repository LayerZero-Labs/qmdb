use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct WriteBatch {
    pub operations: HashMap<Box<[u8]>, Option<Box<[u8]>>>,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self {
            operations: HashMap::new(),
        }
    }

    pub fn put(&mut self, key: impl Into<Box<[u8]>>, value: impl Into<Box<[u8]>>) {
        self.operations.insert(key.into(), Some(value.into()));
    }

    pub fn delete(&mut self, key: impl Into<Box<[u8]>>) {
        self.operations.insert(key.into(), None);
    }

    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    pub fn len(&self) -> usize {
        self.operations.len()
    }

    pub fn clear(&mut self) {
        self.operations.clear();
    }
}
