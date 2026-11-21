//! Shared types: the contracts between the stages. `implementation.md` has the map.
//!
//! A feature here marks a swap point: an alternative layout becomes a sibling module behind
//! its own feature, and the `pub mod` line below is where it is switched.

pub mod ctx;
pub mod env;
#[cfg(feature = "hash-mul")]
pub mod hash;
#[cfg(feature = "id-u64")]
pub mod id;
pub mod mem;
#[cfg(feature = "par-counter")]
pub mod par;
#[cfg(feature = "rec-32")]
pub mod record;
pub mod rows;
#[cfg(feature = "table-linear")]
pub mod table;
pub mod time;

pub use ctx::Ctx;
pub use id::Id;
