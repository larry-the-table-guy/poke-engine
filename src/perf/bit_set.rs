/// Bitset of [PokemonVolatileStatus]
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct VolatileStatusBitSet(u128);

use crate::engine::state::PokemonVolatileStatus;
impl VolatileStatusBitSet {
    pub const fn new() -> Self {
        Self(0)
    }
    pub const fn is_empty(&self) -> bool {
        self.0 == 0
    }
    pub const fn contains(&self, vs: &PokemonVolatileStatus) -> bool {
        self.0 & (1u128 << (*vs as u8)) != 0
    }
    pub const fn insert(&mut self, vs: PokemonVolatileStatus) {
        self.0 |= 1u128 << (vs as u8);
    }
    pub const fn remove(&mut self, vs: &PokemonVolatileStatus) -> bool {
        let present = self.contains(vs);
        self.0 &= !(1u128 << (*vs as u8));
        present
    }
    pub fn retain(&mut self, mut f: impl FnMut(PokemonVolatileStatus) -> bool) {
        let mut remaining = self.0;
        while remaining != 0 {
            let bit_index = remaining.trailing_zeros() as u8;
            let vs = PokemonVolatileStatus::from(bit_index);
            if !f(vs) {
                self.remove(&vs);
            }
            remaining &= remaining - 1
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = PokemonVolatileStatus> {
        let mut remaining = self.0;
        // you can be more efficient implementing it by hand, but iter isn't used anywhere
        // important
        std::iter::from_fn(move || {
            if remaining != 0 {
                let bit_index = remaining.trailing_zeros() as u8;
                remaining &= remaining - 1;
                Some(PokemonVolatileStatus::from(bit_index))
            } else {
                None
            }
        })
    }
    pub fn from_iter(iter: impl IntoIterator<Item = PokemonVolatileStatus>) -> Self {
        let mut s = Self::new();
        for vs in iter {
            s.insert(vs);
        }
        s
    }
    pub fn clear(&mut self) {
        self.0 = 0
    }
}
