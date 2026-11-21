//! Slot `sort-std`: orders one bucket's keys.
//!
//! Contract, kept by every alternative (`sort-radix`): `sort(keys, key_bits, scratch)` sorts
//! `keys` ascending. Every key is below `1 << key_bits`, so a radix sort knows its passes;
//! `scratch` is the worker's spare buffer, any length on entry and left in any state.

pub fn sort(keys: &mut [u64], key_bits: u32, scratch: &mut Vec<u64>) {
    let _ = (key_bits, scratch);
    keys.sort_unstable();
}
