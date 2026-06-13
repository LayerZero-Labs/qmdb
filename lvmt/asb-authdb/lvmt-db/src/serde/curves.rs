use std::io::{Read, Result, Write};

use crate::crypto::export::{
    AffineCurve, FromBytes, G1Affine, G1Projective, ProjectiveCurve, ToBytes,
};

use super::{MyFromBytes, MyToBytes, SerdeType};

impl MyFromBytes for G1Projective {
    #[inline]
    fn read<R: Read>(reader: R, ty: SerdeType) -> Result<Self> {
        if ty.consistent {
            let g1_aff: <Self as ProjectiveCurve>::Affine = FromBytes::read(reader)?;
            Ok(g1_aff.into_projective())
        } else {
            FromBytes::read(reader)
        }
    }
}

impl MyToBytes for G1Projective {
    #[inline]
    fn write<W: Write>(&self, writer: W, ty: SerdeType) -> Result<()> {
        if ty.consistent {
            let g1_aff = self.into_affine();
            ToBytes::write(&g1_aff, writer)
        } else {
            ToBytes::write(self, writer)
        }
    }
}

impl MyFromBytes for G1Affine {
    #[inline]
    fn read<R: Read>(reader: R, _ty: SerdeType) -> Result<Self> {
        FromBytes::read(reader)
    }
}

impl MyToBytes for G1Affine {
    #[inline]
    fn write<W: Write>(&self, writer: W, _ty: SerdeType) -> Result<()> {
        ToBytes::write(self, writer)
    }
}
