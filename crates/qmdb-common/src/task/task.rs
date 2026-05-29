use crate::utils::changeset::ChangeSet;
use alloc::{sync::Arc, vec::Vec};

pub trait Task: Send + Sync {
    fn get_change_sets(&self) -> Arc<Vec<ChangeSet>>;
}
