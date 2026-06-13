use super::{MyFromBytes, MyToBytes, SerdeType};
use keccak_hash::H256;
use std::io::{Read, Result, Write};

impl MyFromBytes for H256 {
    fn read<R: Read>(mut reader: R, _ty: SerdeType) -> Result<Self> {
        let mut answer = H256::default();
        reader.read_exact(answer.as_mut())?;
        Ok(answer)
    }
}

impl MyToBytes for H256 {
    fn write<W: Write>(&self, mut writer: W, _ty: SerdeType) -> Result<()> {
        writer.write_all(self.as_ref())
    }
}
