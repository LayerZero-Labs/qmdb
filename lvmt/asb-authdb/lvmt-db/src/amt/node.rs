use crate::crypto::export::{AffineCurve, FromBytes, ProjectiveCurve, Read, ToBytes, Write};
use crate::crypto::TypeUInt;
use crate::serde::{MyFromBytes, MyToBytes, SerdeType};
use std::io::Result as IoResult;
use std::marker::PhantomData;

#[derive(Clone, Copy, Default)]
pub struct AMTNode<G: ProjectiveCurve> {
    pub commitment: G,
    pub proof: G,
}

impl<G: ProjectiveCurve> MyFromBytes for AMTNode<G> {
    fn read<R: Read>(mut reader: R, ty: SerdeType) -> IoResult<Self> {
        if ty.consistent {
            let g1_aff: <G as ProjectiveCurve>::Affine = FromBytes::read(&mut reader)?;
            let g2_aff: <G as ProjectiveCurve>::Affine = FromBytes::read(&mut reader)?;
            Ok(Self {
                commitment: g1_aff.into_projective(),
                proof: g2_aff.into_projective(),
            })
        } else {
            Ok(Self {
                commitment: FromBytes::read(&mut reader)?,
                proof: FromBytes::read(&mut reader)?,
            })
        }
    }
}

impl<G: ProjectiveCurve> MyToBytes for AMTNode<G> {
    fn write<W: Write>(&self, mut writer: W, ty: SerdeType) -> IoResult<()> {
        if ty.consistent {
            ToBytes::write(&self.commitment.into_affine(), &mut writer)?;
            ToBytes::write(&self.proof.into_affine(), &mut writer)?;
        } else {
            ToBytes::write(&self.commitment, &mut writer)?;
            ToBytes::write(&self.proof, &mut writer)?;
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct NodeIndex<N: TypeUInt> {
    depth: usize,
    index: usize,
    _phantom: PhantomData<N>,
}

impl<N: TypeUInt> NodeIndex<N> {
    #[inline]
    pub(crate) fn new(depth: usize, index: usize) -> Self {
        assert!(index < (1 << depth));
        assert!(depth <= N::USIZE);
        Self {
            depth,
            index,
            _phantom: PhantomData,
        }
    }

    pub fn leaf(index: usize) -> Self {
        Self::new(N::USIZE, index)
    }

    pub fn root() -> Self {
        Self::new(0, 0)
    }

    #[inline]
    pub fn to_sibling(&self) -> Self {
        NodeIndex::new(self.depth, self.index ^ 1)
    }

    #[inline]
    pub fn to_ancestor(&self, height: usize) -> Self {
        assert!(height <= self.depth);
        NodeIndex::new(self.depth - height, self.index >> height)
    }

    pub fn needs_maintain(&self, shard_root: &Self) -> bool {
        if self == &Self::root() {
            return true;
        }

        if self.depth > shard_root.depth {
            let height_diff = self.depth - shard_root.depth;
            let index = self.index >> height_diff;
            return index == shard_root.index;
        } else {
            let sib = self.to_sibling();
            let height_diff = shard_root.depth - sib.depth;
            let index = shard_root.index >> height_diff;
            return sib.index == index;
        }
    }

    #[inline]
    pub fn depth(&self) -> usize {
        self.depth
    }

    #[inline]
    pub fn index(&self) -> usize {
        self.index
    }

    #[inline]
    pub fn total_depth(&self) -> usize {
        N::USIZE
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::type_uint;
    type_uint! {
        struct TestUInt(6);
    }
    type Index = NodeIndex<TestUInt>;

    #[test]
    fn test_needs_maintain() {
        let shard_root = Index::new(3, 4);
        assert!(Index::new(0, 0).needs_maintain(&shard_root));
        assert!(Index::new(1, 0).needs_maintain(&shard_root));
        assert!(Index::new(2, 3).needs_maintain(&shard_root));
        assert!(Index::new(3, 5).needs_maintain(&shard_root));
        assert!(Index::new(4, 8).needs_maintain(&shard_root));
        assert!(Index::new(4, 9).needs_maintain(&shard_root));
        assert!(Index::new(6, 32).needs_maintain(&shard_root));
        assert!(Index::new(6, 39).needs_maintain(&shard_root));

        assert!(!Index::new(1, 1).needs_maintain(&shard_root));
        assert!(!Index::new(2, 0).needs_maintain(&shard_root));
        assert!(!Index::new(2, 2).needs_maintain(&shard_root));
        assert!(!Index::new(3, 2).needs_maintain(&shard_root));
        assert!(!Index::new(3, 4).needs_maintain(&shard_root));
        assert!(!Index::new(4, 2).needs_maintain(&shard_root));
        assert!(!Index::new(4, 7).needs_maintain(&shard_root));
        assert!(!Index::new(4, 10).needs_maintain(&shard_root));
        assert!(!Index::new(6, 7).needs_maintain(&shard_root));
        assert!(!Index::new(6, 31).needs_maintain(&shard_root));
        assert!(!Index::new(6, 40).needs_maintain(&shard_root));
    }
}
