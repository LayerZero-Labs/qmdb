use super::tree::{AMTConfigTrait, AMTData, AMTree};
use crate::crypto::export::{BigInteger, FpParameters, FrInt, FrParams};
use std::ops::{Deref, DerefMut, Drop};

pub struct AMTNodeWriteGuard<'a, C: AMTConfigTrait> {
    index: usize,
    value: C::Data,
    old_fr_int: FrInt<C::PE>,
    tree: &'a mut AMTree<C>,
}

impl<'a, C: AMTConfigTrait> AMTNodeWriteGuard<'a, C> {
    pub(super) fn new(index: usize, value: C::Data, tree: &'a mut AMTree<C>) -> Self {
        let old_fr_int = value.as_fr_int();
        Self {
            index,
            value,
            old_fr_int,
            tree,
        }
    }
}

impl<'a, C: AMTConfigTrait> Deref for AMTNodeWriteGuard<'a, C> {
    type Target = C::Data;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<'a, C: AMTConfigTrait> DerefMut for AMTNodeWriteGuard<'a, C> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

impl<'a, C: AMTConfigTrait> Drop for AMTNodeWriteGuard<'a, C> {
    fn drop(&mut self) {
        let mut fr_int = self.value.as_fr_int();
        let borrow_bit = fr_int.sub_noborrow(&self.old_fr_int);
        if borrow_bit {
            fr_int.add_nocarry(&FrParams::<C::PE>::R);
        }
        std::mem::swap(self.tree.get_mut(self.index), &mut self.value);
        self.tree.update(self.index, fr_int);
    }
}
