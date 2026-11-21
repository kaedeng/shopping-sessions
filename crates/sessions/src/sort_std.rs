//! Slot `sort-std`: orders one bucket's keys.
//!
//! Contract, kept by every alternative (`sort-grouped`, `sort-radix`): `sort(keys, key_bits,
//! low_bits, scratch)` sorts `keys` ascending. Every key is below `1 << key_bits`, so a radix
//! sort knows its passes, and the bits from `low_bits` up are a dense group number;
//! `scratch` is the worker's spare buffers, any length on entry and left in any state.

use crate::SortScratch;

pub fn sort(keys: &mut Vec<u64>, key_bits: u32, low_bits: u32, scratch: &mut SortScratch) {
    let _ = (key_bits, low_bits, scratch);
    keys.sort_unstable();
}
