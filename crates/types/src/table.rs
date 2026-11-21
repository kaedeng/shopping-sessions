//! Slot `table-linear`: a fixed-size open-addressing map from an ID to a small value, linear
//! probing, at most half full. Key 0 (the empty ID) marks a free slot.

use crate::hash::hash;
use crate::id::Id;
use crate::mem::prefetch;

pub struct IdMap<V> {
    slots: Vec<(Id, V)>,
    shift: u32,
}

impl<V: Copy + Default> IdMap<V> {
    /// Room for `keys` distinct keys.
    pub fn new(keys: usize) -> Self {
        let slots = (2 * keys).next_power_of_two().max(16);
        Self { slots: vec![(0, V::default()); slots], shift: 64 - slots.trailing_zeros() }
    }

    /// The value already there, if any; it is kept.
    pub fn insert(&mut self, key: Id, value: V) -> Option<V> {
        assert!(key != 0, "the empty ID is not a key");
        let mask = self.slots.len() - 1;
        let mut at = (hash(key) >> self.shift) as usize;
        loop {
            let slot = &mut self.slots[at];
            if slot.0 == key {
                return Some(slot.1);
            }
            if slot.0 == 0 {
                *slot = (key, value);
                return None;
            }
            at = (at + 1) & mask;
        }
    }

    /// `None` for a key not in the map, and for the empty ID.
    #[inline(always)]
    pub fn get(&self, key: Id) -> Option<V> {
        let mask = self.slots.len() - 1;
        let mut at = (hash(key) >> self.shift) as usize;
        loop {
            let (found, value) = unsafe { *self.slots.get_unchecked(at) };
            if found == key {
                return (key != 0).then_some(value);
            }
            if found == 0 {
                return None;
            }
            at = (at + 1) & mask;
        }
    }

    #[inline(always)]
    pub fn prefetch(&self, key: Id) {
        prefetch(unsafe { self.slots.as_ptr().add((hash(key) >> self.shift) as usize) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::pack;

    #[test]
    fn finds_what_was_inserted_and_nothing_else() {
        let mut map = IdMap::<u32>::new(1000);
        for n in 0..1000u32 {
            assert_eq!(map.insert(pack(format!("p{n}").as_bytes()), n), None);
        }
        assert_eq!(map.insert(pack(b"p7"), 99), Some(7));
        for n in 0..1000u32 {
            assert_eq!(map.get(pack(format!("p{n}").as_bytes())), Some(n));
        }
        assert_eq!(map.get(pack(b"P7")), None);
        assert_eq!(map.get(0), None);
    }
}
