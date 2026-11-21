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

/// product_id -> category index. A product written `p` and a number without a leading zero
/// (every product the generator writes) is a byte in an array at that number; any other ID
/// is in an `IdMap`. Built from a 50,000-row `products.csv`, the map alone was 1.5 ms of
/// hashing into 2 MB of fresh table before the first input line was read; the array is 50 KB
/// and a lookup is no cache miss.
pub struct Products {
    /// The category of product number `n`, or `ABSENT`.
    dense: Vec<u8>,
    other: IdMap<u8>,
}

impl Products {
    /// No category has this index (`rows::NO_CATEGORY`).
    const ABSENT: u8 = 0xFF;
    /// The array holds numbers below this; a larger one goes to the map.
    pub const NUMBERS: usize = 1 << 22;

    /// Room for product numbers below `numbers` and for `others` other IDs.
    pub fn new(numbers: usize, others: usize) -> Products {
        Products { dense: vec![Self::ABSENT; numbers.min(Self::NUMBERS)], other: IdMap::new(others) }
    }

    /// The number of the product ID in the `len` bytes at `p` when it is written the plain
    /// way: `p`, then 1 to 7 digits, the first not `0` unless it is the only one. Two IDs
    /// with a number are equal exactly when the numbers are.
    ///
    /// # Safety
    /// 8 bytes readable at `p`.
    #[inline(always)]
    pub unsafe fn number_at(p: *const u8, len: usize) -> Option<u32> {
        Self::number_in(u64::from_le(unsafe { p.cast::<u64>().read_unaligned() }), len)
    }

    /// `number_at` for an ID whose first 8 bytes are `word`, little-endian.
    #[inline(always)]
    fn number_in(word: u64, len: usize) -> Option<u32> {
        if !(2..=8).contains(&len) || word as u8 != b'p' {
            return None;
        }
        let (mut number, mut digits) = (0u32, true);
        for at in 1..len {
            let digit = ((word >> (8 * at)) as u8).wrapping_sub(b'0');
            digits &= digit <= 9;
            number = number.wrapping_mul(10).wrapping_add(u32::from(digit));
        }
        let leading_zero = (word >> 8) as u8 == b'0' && len > 2;
        (digits && !leading_zero).then_some(number)
    }

    /// `number_at` for a packed ID.
    #[inline(always)]
    fn number_of(id: Id) -> Option<u32> {
        if id == 0 || id & crate::id::SLOW != 0 {
            return None;
        }
        let (word, ninth, len) = crate::id::spread(id);
        if ninth != 0 { None } else { Self::number_in(word, len) }
    }

    /// Product number `number`'s category; the first given is kept. `false` when the number
    /// is past the array: the caller inserts the ID instead.
    #[inline(always)]
    pub fn insert_number(&mut self, number: u32, category: u8) -> bool {
        match self.dense.get_mut(number as usize) {
            Some(slot) => {
                if *slot == Self::ABSENT {
                    *slot = category;
                }
                true
            }
            None => false,
        }
    }

    /// The product's category; the first given is kept.
    pub fn insert(&mut self, id: Id, category: u8) {
        if !Self::number_of(id).is_some_and(|number| self.insert_number(number, category)) {
            self.other.insert(id, category);
        }
    }

    /// `None` for a product not in the table, and for the empty ID.
    #[inline(always)]
    pub fn get(&self, id: Id) -> Option<u8> {
        match Self::number_of(id) {
            Some(number) if (number as usize) < self.dense.len() => {
                // SAFETY: checked just above.
                let category = unsafe { *self.dense.get_unchecked(number as usize) };
                (category != Self::ABSENT).then_some(category)
            }
            _ => self.other.get(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::pack;

    #[test]
    fn products_by_number_and_by_id() {
        let mut products = Products::new(1000, 16);
        // Plain numbers, a number past the array, and IDs that are not plain numbers: each
        // is its own product.
        let ids: Vec<String> =
            ["p0", "p1", "p7", "p999", "p1000", "p123456", "p9999999", "p01", "p00", "P1", "p", "q1", "p1a", "p-1", "p12345678", "p123456789", "a long product id"].map(String::from).to_vec();
        for (at, id) in ids.iter().enumerate() {
            products.insert(pack(id.as_bytes()), at as u8);
        }
        // The first category given is kept.
        products.insert(pack(b"p7"), 99);
        products.insert(pack(b"p01"), 99);
        for (at, id) in ids.iter().enumerate() {
            assert_eq!(products.get(pack(id.as_bytes())), Some(at as u8), "{id}");
            let mut text = id.clone().into_bytes();
            text.resize(text.len() + 16, b'5');
            // SAFETY: 8 bytes and more at the pointer.
            let number = unsafe { Products::number_at(text.as_ptr(), id.len()) };
            assert_eq!(number, Products::number_of(pack(id.as_bytes())), "{id}");
        }
        assert_eq!(unsafe { Products::number_at(b"p123456,x".as_ptr(), 7) }, Some(123456));
        assert_eq!(Products::number_of(pack(b"p01")), None);
        assert_eq!(Products::number_of(pack(b"p0")), Some(0));
        for missing in ["p2", "p998", "p1001", "p010", "p3a", "q", ""] {
            assert_eq!(products.get(pack(missing.as_bytes())), None, "{missing}");
        }
    }

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
