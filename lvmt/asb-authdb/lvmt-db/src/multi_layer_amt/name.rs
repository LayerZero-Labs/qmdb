use crate::serde::{MyFromBytes, MyToBytes, SerdeType};
use std::io::{Read, Result, Write};

#[derive(Default, Debug, Clone, Hash, PartialEq, Eq, Ord, PartialOrd)]
pub struct TreeName(pub(super) Vec<u32>);

impl MyFromBytes for TreeName {
    fn read<R: Read>(mut reader: R, ty: SerdeType) -> Result<Self> {
        let length: u8 = MyFromBytes::read(&mut reader, ty)?;
        let mut answer = Vec::<u32>::with_capacity(length as usize);
        for _ in 0..length {
            answer.push(MyFromBytes::read(&mut reader, ty)?);
        }
        Ok(Self(answer))
    }
}

impl MyToBytes for TreeName {
    fn write<W: Write>(&self, mut writer: W, ty: SerdeType) -> Result<()> {
        MyToBytes::write(&(self.0.len() as u8), &mut writer, ty)?;
        for item in self.0.iter() {
            MyToBytes::write(item, &mut writer, ty)?;
        }
        Ok(())
    }
}

impl TreeName {
    pub const fn root() -> Self {
        TreeName(Vec::new())
    }

    pub fn level_index(&self) -> Option<u32> {
        self.0.last().cloned()
    }

    pub fn child(&self, index: u32) -> Self {
        let mut answer = self.clone();
        answer.0.push(index);
        answer
    }

    pub fn parent(&self) -> Option<Self> {
        let mut answer = self.clone();
        let top_element = answer.0.pop();
        if top_element.is_none() {
            None
        } else {
            Some(answer)
        }
    }
}

#[test]
fn test_tree_name_string() {
    assert_eq!(TreeName(vec![]).to_bytes_consensus(), [0u8]);

    assert_eq!(
        TreeName(vec![1]).to_bytes_consensus(),
        [1u8, 1u8, 0u8, 0u8, 0u8]
    );

    assert_eq!(
        TreeName::from_bytes_consensus(&TreeName(vec![1, 2, 3]).to_bytes_consensus()).unwrap(),
        TreeName(vec![1, 2, 3])
    );
}
