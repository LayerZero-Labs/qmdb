use std::fmt::Debug;

use smallvec::{smallvec, SmallVec};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Nibble(u8);

impl Debug for Nibble {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner().fmt(f)
    }
}

impl Nibble {
    #[inline(always)]
    pub const fn inner(self) -> u8 {
        self.0
    }

    #[inline(always)]
    pub const fn from_hi(x: u8) -> Self {
        Self(x >> 4)
    }

    #[inline(always)]
    pub const fn from_lo(x: u8) -> Self {
        Self(x & 0xf)
    }

    #[inline(always)]
    pub const fn from_hi_and_lo(x: u8) -> [Self; 2] {
        [Self::from_hi(x), Self::from_lo(x)]
    }

    #[inline(always)]
    pub const fn combine_pair(hi: Nibble, lo: Nibble) -> u8 {
        (hi.0 << 4) | lo.0
    }

    #[inline(always)]
    pub const fn zero() -> Self {
        Nibble(0)
    }

    #[inline(always)]
    pub const fn is_zero(self) -> bool {
        self.inner() == 0u8
    }

    pub fn all() -> impl Iterator<Item = Nibble> {
        (0..16).map(Nibble::from_lo)
    }
}

pub fn bytes_to_nibble_list(data: Vec<u8>) -> Vec<Nibble> {
    nibble_iter(&data).collect()
}

fn nibble_iter(data: &[u8]) -> impl Iterator<Item = Nibble> + '_ {
    data.iter().cloned().map(Nibble::from_hi_and_lo).flatten()
}

pub fn from_mpt_key(key: Vec<u8>) -> (Vec<Nibble>, bool) {
    let mut iterator = nibble_iter(&key);
    let ty = iterator.next().unwrap();
    if (ty.inner() & 0x1) == 0 {
        iterator.next().unwrap();
    }
    let leaf = (ty.inner() & 0x2) != 0;

    return (iterator.collect(), leaf);
}

pub fn to_mpt_key(key: &[Nibble], leaf: bool) -> Vec<u8> {
    let odd = (key.len() % 2) == 1;
    let prefix: SmallVec<[Nibble; 4]> = match (odd, leaf) {
        (false, false) => smallvec![Nibble(0), Nibble(0)],
        (true, false) => smallvec![Nibble(1)],
        (false, true) => smallvec![Nibble(2), Nibble(0)],
        (true, true) => smallvec![Nibble(3)],
    };

    let mut iterator = prefix.iter().cloned().chain(key.iter().cloned());
    let mut answer = Vec::with_capacity(key.len() / 2 + 2);
    while let Some(hi) = iterator.next() {
        let lo = iterator.next().unwrap();
        answer.push(Nibble::combine_pair(hi, lo));
    }
    answer
}
