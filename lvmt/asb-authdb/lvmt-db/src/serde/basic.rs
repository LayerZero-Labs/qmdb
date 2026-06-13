use std::io::{Read, Result, Write};

use super::{MyFromBytes, MyToBytes, SerdeType};

macro_rules! impl_for_basic {
    ($uint: ty) => {
        impl MyFromBytes for $uint {
            #[inline]
            fn read<R: Read>(mut reader: R, _ty: SerdeType) -> Result<Self> {
                let mut bytes = (0 as $uint).to_le_bytes();
                reader.read_exact(&mut bytes)?;
                Ok(<$uint>::from_le_bytes(bytes))
            }
        }

        impl MyToBytes for $uint {
            #[inline]
            fn write<W: Write>(&self, mut writer: W, _ty: SerdeType) -> Result<()> {
                writer.write_all(&self.to_le_bytes())
            }
        }
    };
}
impl_for_basic!(u8);
impl_for_basic!(u16);
impl_for_basic!(u32);
impl_for_basic!(u64);
impl_for_basic!(usize);

impl MyFromBytes for Vec<u8> {
    fn read<R: Read>(mut reader: R, ty: SerdeType) -> Result<Self> {
        let length: usize = MyFromBytes::read(&mut reader, ty)?;
        let mut answer = vec![0u8; length];
        reader.read_exact(&mut answer)?;
        Ok(answer)
    }
}

impl MyToBytes for Vec<u8> {
    fn write<W: Write>(&self, mut writer: W, ty: SerdeType) -> Result<()> {
        MyToBytes::write(&self.len(), &mut writer, ty)?;
        writer.write_all(&self)?;
        Ok(())
    }
}
