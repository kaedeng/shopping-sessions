//! Which vector instructions the opaque parse runs (`SS_PARSE_WIDTH=512|256`), as a type its
//! functions are made for.
//!
//! A core of the scorer's generation (Skylake-SP to Cascade Lake) runs at one of three clocks,
//! by the "AVX licence" its instructions ask for: licence 0, the base turbo clock, for scalar
//! code, any 128-bit instruction and 256-bit ones that are neither multiplies nor floating
//! point; licence 1 for any 512-bit instruction or a 256-bit multiply; licence 2 for a 512-bit
//! multiply. On the AWS Cascade Lake licence 1 is 3.39 GHz against 3.61, and the licence holds
//! for both hyperthreads of the core and for about 650 us after the last instruction that asked
//! for it: a stage runs at licence 0 only if nothing it runs, on any worker, asks for more.
//!
//! `Wide` is the parse as it was: 512-bit compares, transposes and compress-stores, 256-bit
//! multiplies. `Narrow` keeps to licence 0: the same work in 256-bit registers (AVX-512VL's
//! masks and compress-stores on them are licence 0), the multiplies on 128-bit halves. The
//! compiler joins two 256-bit operations into one 512-bit one, and two 128-bit multiplies into
//! one 256-bit multiply, wherever it can (two compares whose masks are put side by side, the
//! halves of a register), so the narrow code writes those instructions as `asm!`.
pub trait Width: 'static {
    /// Only licence-0 instructions.
    const NARROW: bool;
}

/// 512-bit instructions and 256-bit multiplies allowed.
pub struct Wide;

/// Licence 0 only.
pub struct Narrow;

impl Width for Wide {
    const NARROW: bool = false;
}

impl Width for Narrow {
    const NARROW: bool = true;
}

/// `SS_PARSE_WIDTH`: whether the opaque parse keeps to licence 0 (`256`) or not (`512`, the
/// pick until the scorer says otherwise).
pub fn narrow_from_env() -> bool {
    crate::env::pick("SS_PARSE_WIDTH", &["512", "256"]) == 1
}
