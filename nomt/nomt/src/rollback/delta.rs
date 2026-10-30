use nomt_core::trie::KeyPath;
use std::{
    collections::HashMap,
    io::{Cursor, Read as _},
};

/// A delta that should be applied to reverse a commit.
#[derive(Debug, Clone)]
pub struct Delta {
    /// This map contains the prior value for each key that was written by the commit this delta
    /// reverses. `None` indicates that the key did not exist before the commit.
    pub(crate) priors: HashMap<KeyPath, Option<Vec<u8>>>,
}

impl Delta {
    #[cfg(test)]
    fn empty() -> Self {
        Self {
            priors: HashMap::new(),
        }
    }

    /// Encode the delta into a buffer.
    ///
    /// Returns the number of bytes written.
    pub(super) fn encode(&self) -> Vec<u8> {
        // The serialization format has the following layout.
        //
        // The keys are split into two groups and written as separate arrays. Those groups are:
        //
        // 1. erase: The keys that did not exist before the commit.
        // 2. reinstateThe keys that had prior values.
        //
        // The keys that did not exist are written first. The keys that had prior values are
        // written second.
        //
        // For each kind of key, we first write out the length of the array encoded as a u32.
        // This is followed by the keys themselves, written contiguously in little-endian order.
        //
        // The keys are written as 32-byte big-endian values.

        // Sort the keys into two groups.
        let mut to_erase = Vec::with_capacity(self.priors.len());
        let mut to_reinstate = Vec::with_capacity(self.priors.len());
        for (key, value) in self.priors.iter() {
            match value {
                None => to_erase.push(key),
                Some(value) => to_reinstate.push((key, value)),
            }
        }

        let to_erase_len = to_erase.len() as u32;
        let mut buf = Vec::with_capacity(4 + 32 * to_erase.len());
        buf.extend_from_slice(&to_erase_len.to_le_bytes());
        for key in to_erase {
            buf.extend_from_slice(&key[..]);
        }

        let to_reinstate_len = to_reinstate.len() as u32;
        buf.extend_from_slice(&to_reinstate_len.to_le_bytes());
        for (key, value) in to_reinstate {
            buf.extend_from_slice(&key[..]);
            let value_len = value.len() as u32;
            buf.extend_from_slice(&value_len.to_le_bytes());
            buf.extend_from_slice(value);
        }

        buf
    }

    /// Decodes the delta from a buffer.
    pub(super) fn decode(reader: &mut Cursor<impl AsRef<[u8]>>) -> anyhow::Result<Self> {
        let mut priors = HashMap::new();

        // Read the number of keys to erase.
        let mut buf = [0; 4];
        reader.read_exact(&mut buf)?;
        let to_erase_len = u32::from_le_bytes(buf);
        // Read the keys to erase.
        for _ in 0..to_erase_len {
            let mut key_path = [0; 32];
            reader.read_exact(&mut key_path)?;
            let preemted = priors.insert(key_path, None).is_some();
            if preemted {
                anyhow::bail!("duplicate key path (erase): {:?}", key_path);
            }
        }

        // Read the number of keys to reinstate.
        reader.read_exact(&mut buf)?;
        let to_reinsate_len = u32::from_le_bytes(buf);
        // Read the keys to reinstate along with their values.
        for _ in 0..to_reinsate_len {
            // Read the key path.
            let mut key_path = [0; 32];
            reader.read_exact(&mut key_path)?;
            // Read the value.
            let mut value = Vec::new();
            reader.read_exact(&mut buf)?;
            let value_len = u32::from_le_bytes(buf);
            value.resize(value_len as usize, 0);
            reader.read_exact(&mut value)?;
            let preempted = priors.insert(key_path, Some(value)).is_some();
            if preempted {
                anyhow::bail!("duplicate key path (reinstate): {:?}", key_path);
            }
        }
        Ok(Delta { priors })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_roundtrip() {
        let mut delta = Delta::empty();
        delta.priors.insert([1; 32], Some(b"value1".to_vec()));
        delta.priors.insert([2; 32], None);
        delta.priors.insert([3; 32], Some(b"value3".to_vec()));

        let mut buf = delta.encode();
        let mut cursor = Cursor::new(&mut buf);
        let delta2 = Delta::decode(&mut cursor).unwrap();
        assert_eq!(delta.priors, delta2.priors);
    }

    #[test]
    fn delta_roundtrip_empty() {
        let delta = Delta::empty();
        let mut buf = delta.encode();
        let mut cursor = Cursor::new(&mut buf);
        let delta2 = Delta::decode(&mut cursor).unwrap();
        assert_eq!(delta.priors, delta2.priors);
    }
}
