//! Event dedup, one partition at a time (decision: Event dedup).
//!
//! CONTRACT (the binary depends on exactly this; the bodies are the implementer's):
//! - `Scratch`, `Default`, here
//! - `partition<'s, 'p>(pieces: impl Iterator<Item = &'p [EventRec]> + Clone, part_bits: u32,
//!   scratch: &'s mut Scratch) -> &'s [EventRec]`, slot `dedup-partitioned`

use types::record::EventRec;

#[cfg(feature = "dedup-partitioned")]
mod dedup_partitioned;
#[cfg(feature = "dedup-partitioned")]
pub use dedup_partitioned::partition;

#[cfg(test)]
mod tests;

/// A worker's reusable buffers: the small table and the kept records.
#[derive(Default)]
pub struct Scratch {
    table: Vec<u32>,
    kept: Vec<EventRec>,
}
