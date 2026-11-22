//! Shared types: the contracts between the stages. `implementation.md` has the map.
//!
//! A feature here marks a swap point: an alternative layout becomes a sibling module behind
//! its own feature, and the `pub mod` line below is where it is switched.

#[cfg(feature = "pipe-positional")]
pub mod chunks;
pub mod ctx;
pub mod env;
#[cfg(feature = "hash-mul")]
pub mod hash;
#[cfg(feature = "id-u64")]
pub mod id;
pub mod mem;
#[cfg(feature = "par-counter")]
pub mod par;
pub mod parts;
#[cfg(feature = "pipe-positional")]
pub mod pos;
#[cfg(feature = "rec-32")]
pub mod record;
pub mod rows;
#[cfg(feature = "pipe-positional")]
pub mod scramble;
#[cfg(feature = "table-linear")]
pub mod table;
pub mod time;
pub mod tl;
#[cfg(feature = "pipe-positional")]
pub mod users;

pub use ctx::Ctx;
pub use id::Id;
