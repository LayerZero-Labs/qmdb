mod basic;
mod curves;
mod h256;

use std::io::{Read, Result, Write};

#[derive(Copy, Clone)]
pub struct SerdeType {
    pub consistent: bool,
}

pub trait MyFromBytes: Sized {
    fn read<R: Read>(reader: R, ty: SerdeType) -> Result<Self>;
    fn read_vec<R: Read>(mut reader: R, ty: SerdeType) -> Result<Vec<Self>> {
        let length: usize = MyFromBytes::read(&mut reader, ty)?;
        let mut answer = Vec::<Self>::with_capacity(length);
        for _ in 0..length {
            answer.push(<Self as MyFromBytes>::read(&mut reader, ty)?);
        }
        Ok(answer)
    }

    fn from_bytes(mut data: &[u8], ty: SerdeType) -> Result<Self> {
        MyFromBytes::read(&mut data, ty)
    }
    fn from_bytes_local(data: &[u8]) -> Result<Self> {
        MyFromBytes::from_bytes(data, SerdeType { consistent: false })
    }
    fn from_bytes_consensus(data: &[u8]) -> Result<Self> {
        MyFromBytes::from_bytes(data, SerdeType { consistent: true })
    }
}

pub trait MyToBytes: Sized {
    fn write<W: Write>(&self, writer: W, ty: SerdeType) -> Result<()>;
    fn write_vec<W: Write>(vec_self: &Vec<Self>, mut writer: W, ty: SerdeType) -> Result<()> {
        MyToBytes::write(&vec_self.len(), &mut writer, ty)?;
        for item in vec_self.iter() {
            MyToBytes::write(item, &mut writer, ty)?;
        }
        Ok(())
    }

    fn to_bytes(&self, ty: SerdeType) -> Vec<u8> {
        let mut serialized = Vec::with_capacity(1024);
        // Write to Vec<u8> should always return Ok(..)
        MyToBytes::write(self, &mut serialized, ty).unwrap();
        serialized.shrink_to_fit();
        serialized
    }
    fn to_bytes_local(&self) -> Vec<u8> {
        MyToBytes::to_bytes(self, SerdeType { consistent: false })
    }
    fn to_bytes_consensus(&self) -> Vec<u8> {
        MyToBytes::to_bytes(self, SerdeType { consistent: true })
    }
}
