//! Slot `hash-mul`: one multiply of the `u64` ID by an odd constant. The top bits of the
//! product pick the partition, the next bits the slot inside it, so a key is hashed once.

pub const C1: u64 = 0x9E37_79B9_7F4A_7C15;
pub const C2: u64 = 0xD6E8_FEB8_6659_FD93;

/// For tables keyed by an ID as read: the dedup partitions (`event_id`), the link table
/// (`user_id`), the product and campaign maps.
#[inline(always)]
pub fn hash(id: u64) -> u64 {
    id.wrapping_mul(C2)
}

/// For the person buckets, independent of `hash`: `>> (64 - bucket_bits)` is the bucket, and
/// the bits below it are free for the bucket's own table.
#[inline(always)]
pub fn person_hash(person: u64) -> u64 {
    person.wrapping_mul(C1)
}
