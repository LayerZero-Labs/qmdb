use super::task::Task;
use crate::utils::changeset::ChangeSet;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, serde::Serialize, serde::Deserialize, bincode::Encode, bincode::Decode)]
pub struct SimpleTask {
    #[serde(
        serialize_with = "serialize_arc_vec",
        deserialize_with = "deserialize_arc_vec"
    )]
    pub change_sets: Arc<Vec<ChangeSet>>,
}

fn serialize_arc_vec<S>(value: &Arc<Vec<ChangeSet>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    value.as_ref().serialize(serializer)
}

fn deserialize_arc_vec<'de, D>(deserializer: D) -> Result<Arc<Vec<ChangeSet>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let vec = Vec::deserialize(deserializer)?;
    Ok(Arc::new(vec))
}

impl Task for SimpleTask {
    fn get_change_sets(&self) -> Arc<Vec<ChangeSet>> {
        self.change_sets.clone()
    }
}

impl SimpleTask {
    pub fn new(changesets: Vec<ChangeSet>) -> Self {
        Self {
            change_sets: Arc::new(changesets),
        }
    }
}
