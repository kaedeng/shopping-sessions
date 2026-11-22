//! The users' directory: every `user_id` of the input numbered, found by hash.
//!
//! An ID is an opaque string (rules: one in eight is a letter and 32 hex digits, the others a
//! letter and up to 8 of `0-9a-z`, and nothing may be read from them). What the stages after
//! the parse need of a `user_id` is which user it is, so the parse looks each one up here and
//! a record carries the user's number, 4 bytes, where the ID itself would be 9 or 33. The
//! links are then unions over numbers, and a person's name is read back from the directory
//! once, when its rows are written.
//!
//! Two tables, both open addressing with linear probes, filled by all workers at once:
//!
//! - `short`: two words a slot, the key and the user's number, probed a cache line of four
//!   slots at a time (`Users::short`). The key of an ID spelled the
//!   short way is its digits (`short_key`); any other ID that is not spelled the long way
//!   goes through the intern tables (`id::intern_any`) and its key is that `Id`, top bit set.
//!   A slot is claimed by one compare-and-swap of the key, and then given its number.
//! - `long`: four words a slot, for an ID spelled the long way: a tag, the 128 bits of its
//!   hex digits, the number. A slot is claimed by a compare-and-swap of the tag to `BUSY`,
//!   filled, and then given its tag.
//!
//! A reader that meets a slot between its claim and its number waits for the number.
//!
//! The numbers are dense: a thread takes them a block at a time from one counter, so what is
//! kept per user after the parse (which person it is, `src/opaque.rs`) is an array of the
//! users there are, a few megabytes that stay in cache, and not one of the directory's
//! slots, most of them empty. (Numbered by slot, the person of a row was a cache miss a row:
//! the hottest line of the stages after the parse.)
//!
//! The tables do not grow: when a probe runs long the directory says so (`overflowed`), and
//! the caller starts over with a larger one.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Acquire, Ordering::Relaxed, Ordering::Release};

use crate::id::{self, Id, SLOW};
use crate::mem::Zeroed;

/// No user: an empty `user_id`.
pub const NO_USER: u32 = u32::MAX;
/// Slots a lookup walks before it calls the table full.
const PROBES: usize = 1 << 10;
/// A long slot being filled.
const BUSY: u64 = 1;
const ZEROS: u64 = 0x3030_3030_3030_3030;

/// Short slots a cache line: a lookup compares a line's keys at once.
const LINE: usize = 4;

/// Numbers a thread takes from the counter at a time.
const BLOCK: u32 = 1 << 10;

pub struct Users {
    /// `[key, number + 1]`.
    short: Zeroed<[AtomicU64; 2]>,
    /// `[tag, hi, lo, number + 1]`.
    long: Zeroed<[AtomicU64; 4]>,
    short_shift: u32,
    long_shift: u32,
    /// Per number: its slot, the long table's after the short one's.
    slot_of: Zeroed<AtomicU32>,
    /// The next block of numbers.
    next: AtomicU32,
    /// Which directory this is, among all the process makes: a thread's block is of one.
    made: u64,
    full: AtomicBool,
}

thread_local! {
    /// The calling thread's block of numbers: the directory it is of, the next number, the
    /// block's end.
    static MINE: Cell<(u64, u32, u32)> = const { Cell::new((0, 0, 0)) };
}

/// Directories made so far.
static MADE: AtomicU64 = AtomicU64::new(0);

/// What a user's slot holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UserKey {
    /// `short_key` of the ID, or the `Id` of its bytes (top bit set).
    Short(u64),
    /// `long_key` of the ID.
    Long(u64, u64),
}

/// The key of an ID spelled the short way: `letter`, then 1 to 8 of `0-9a-z`, the first not
/// `0` unless it is the only one. The digits less `'0'`, a byte each, the last in the top
/// byte and zeros below the first, plus one: what the parse kernel has of such an ID in a
/// register. Never 0 and never with the top bit set. `None` for any other spelling.
///
/// # Safety
/// `len` bytes readable at `p`.
#[inline]
pub unsafe fn short_key_at(p: *const u8, len: usize, letter: u8) -> Option<u64> {
    if !(2..=9).contains(&len) || unsafe { *p } != letter {
        return None;
    }
    let digits = len - 1;
    let mut word = 0u64;
    for at in 0..digits {
        let byte = unsafe { *p.add(1 + at) };
        if !(byte.is_ascii_digit() || byte.is_ascii_lowercase()) {
            return None;
        }
        word |= u64::from(byte - b'0') << (8 * (8 - digits + at));
    }
    if digits > 1 && unsafe { *p.add(1) } == b'0' {
        return None;
    }
    Some(word + 1)
}

/// The key of an ID spelled the long way: `letter`, then 32 of `0-9a-f`. The digits two a
/// byte, 16 bytes in the ID's order, as two little-endian words. `None` for any other
/// spelling.
///
/// # Safety
/// `len` bytes readable at `p`.
#[inline]
pub unsafe fn long_key_at(p: *const u8, len: usize, letter: u8) -> Option<(u64, u64)> {
    if len != 33 || unsafe { *p } != letter {
        return None;
    }
    // SAFETY: the 32 bytes after the letter are the ID's.
    unsafe { hex32(p.add(1)) }
}

/// 32 hex digits at `p` as `long_key_at` has them, `None` if a byte is not one of `0-9a-f`:
/// in one register, each digit a value, two values a byte (the first sixteen times, the
/// second once), the bytes in the digits' order.
///
/// # Safety
/// 32 bytes readable at `p`.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
unsafe fn hex32(p: *const u8) -> Option<(u64, u64)> {
    use core::arch::x86_64::*;
    // SAFETY: the caller's contract; register arithmetic.
    #[allow(unused_unsafe)]
    unsafe {
        let text = _mm256_loadu_si256(p.cast());
        let digit = _mm256_sub_epi8(text, _mm256_set1_epi8(b'0' as i8));
        let hex = _mm256_sub_epi8(text, _mm256_set1_epi8(b'a' as i8));
        let is_digit = _mm256_cmpeq_epi8(_mm256_min_epu8(digit, _mm256_set1_epi8(9)), digit);
        let is_hex = _mm256_cmpeq_epi8(_mm256_min_epu8(hex, _mm256_set1_epi8(5)), hex);
        if _mm256_movemask_epi8(_mm256_or_si256(is_digit, is_hex)) != -1 {
            return None;
        }
        let value = _mm256_blendv_epi8(_mm256_add_epi8(hex, _mm256_set1_epi8(10)), digit, is_digit);
        let pairs = _mm256_maddubs_epi16(value, _mm256_set1_epi16(0x0110));
        let bytes = _mm256_packus_epi16(pairs, pairs);
        Some((_mm256_extract_epi64::<0>(bytes) as u64, _mm256_extract_epi64::<2>(bytes) as u64))
    }
}

/// `hex32` a digit at a time, where the build has no 256-bit registers.
///
/// # Safety
/// 32 bytes readable at `p`.
#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
#[inline(always)]
unsafe fn hex32(p: *const u8) -> Option<(u64, u64)> {
    let mut bytes = [0u8; 16];
    for at in 0..32 {
        let value = match unsafe { *p.add(at) } {
            byte @ b'0'..=b'9' => byte - b'0',
            byte @ b'a'..=b'f' => byte - b'a' + 10,
            _ => return None,
        };
        bytes[at / 2] |= value << (4 * (1 - at % 2));
    }
    Some((u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes")), u64::from_le_bytes(bytes[8..].try_into().expect("8 bytes"))))
}

/// `short_key_at` for an ID with 9 bytes readable at `p` whatever `len` is, as a field of a
/// segment has: the digits a word at a time. (Byte by byte, the links' two IDs a line were
/// 2% of a run's instructions.)
///
/// # Safety
/// 9 bytes readable at `p`.
#[inline(always)]
pub unsafe fn short_key_wide(p: *const u8, len: usize, letter: u8) -> Option<u64> {
    if !(2..=9).contains(&len) || unsafe { *p } != letter {
        return None;
    }
    let digits = len - 1;
    // The digits in the low bytes, in their order; the bytes past them read as `1`, a digit.
    let raw = u64::from_le(unsafe { p.add(1).cast::<u64>().read_unaligned() });
    let mask = u64::MAX >> (8 * (8 - digits));
    let word = (raw & mask) | (0x3131_3131_3131_3131 & !mask);
    // A byte is one of `0-9a-z`: below 0x80, and bit 7 of each sum below says which side of
    // a bound it is (no sum carries into the next byte).
    const SEVEN: u64 = 0x7F7F_7F7F_7F7F_7F7F;
    let low = word & SEVEN;
    let at_least = |bound: u8| low + (0x0101_0101_0101_0101 * u64::from(0x80 - bound));
    let digit = at_least(b'0') & !at_least(b'9' + 1);
    let lower = at_least(b'a') & !at_least(b'z' + 1);
    let fine = (digit | lower) & !word & !SEVEN;
    // No leading zero, unless it is the only digit.
    if fine != !SEVEN || (digits > 1 && raw as u8 == b'0') {
        return None;
    }
    Some((((raw & mask) - (ZEROS & mask)) << (8 * (8 - digits))) + 1)
}

/// `key_at` for a field of a segment: 16 bytes readable at `p` whatever `len` is.
///
/// # Safety
/// 16 bytes readable at `p`, and `len` of them at least.
#[inline(always)]
pub unsafe fn key_at_wide(p: *const u8, len: usize) -> UserKey {
    if len == 0 {
        return UserKey::Short(0);
    }
    // SAFETY: the caller's contract.
    unsafe {
        if let Some(key) = short_key_wide(p, len, b'u') {
            return UserKey::Short(key);
        }
        if let Some((hi, lo)) = long_key_at(p, len, b'u') {
            return UserKey::Long(hi, lo);
        }
        UserKey::Short(id::intern_any(std::slice::from_raw_parts(p, len)))
    }
}

/// The digits of a short key as text: up to 8 bytes, and how many.
#[inline]
pub fn short_digits(key: u64) -> ([u8; 8], usize) {
    debug_assert!(key != 0 && key & SLOW == 0);
    let word = key - 1;
    // The first digit is the lowest byte that is not 0; a lone `0` leaves none.
    let count = if word == 0 { 1 } else { 8 - (word.trailing_zeros() / 8) as usize };
    let text = (word >> (8 * (8 - count))) + (ZEROS >> (8 * (8 - count)));
    (text.to_le_bytes(), count)
}

/// Appends the 32 hex digits of a long key.
pub fn long_digits(hi: u64, lo: u64, out: &mut Vec<u8>) {
    for byte in hi.to_le_bytes().into_iter().chain(lo.to_le_bytes()) {
        out.push(b"0123456789abcdef"[usize::from(byte >> 4)]);
        out.push(b"0123456789abcdef"[usize::from(byte & 15)]);
    }
}

/// The directory's key of the `user_id` that is the `len` bytes at `p`, for any spelling:
/// `Short(0)` for an empty field. Apart from the lookup (`Users::get`), so that a batch of
/// keys can be asked for before the first is looked up.
///
/// # Safety
/// `len` bytes readable at `p`.
pub unsafe fn key_at(p: *const u8, len: usize) -> UserKey {
    if len == 0 {
        return UserKey::Short(0);
    }
    // SAFETY: the caller's contract.
    unsafe {
        if let Some(key) = short_key_at(p, len, b'u') {
            return UserKey::Short(key);
        }
        if let Some((hi, lo)) = long_key_at(p, len, b'u') {
            return UserKey::Long(hi, lo);
        }
        UserKey::Short(id::intern_any(std::slice::from_raw_parts(p, len)))
    }
}

#[inline(always)]
fn mix(key: u64) -> u64 {
    key.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// A hash of a long key, for tables of them.
#[inline(always)]
pub fn mix_long(hi: u64, lo: u64) -> u64 {
    let wide = u128::from(hi ^ 0x2545_F491_4F6C_DD1D) * u128::from(lo ^ 0x9E37_79B9_7F4A_7C15);
    (wide as u64) ^ (wide >> 64) as u64
}

/// The short table as the first step of a lookup sees it, for a loop that keeps it in
/// registers (`Users::short_lines`): the line a key's lookup begins at, and the number of a
/// key that line already holds. Through `Users::short` a loop read the table's address, its
/// shift and its mask again for every key, 34 instructions a lookup; this way it is 15.
#[derive(Clone, Copy)]
pub struct ShortLines {
    base: *const [AtomicU64; 2],
    shift: u32,
}

impl ShortLines {
    /// The cache line the lookup of `key` begins at.
    #[inline(always)]
    pub fn line(&self, key: u64) -> *const AtomicU64 {
        self.base.wrapping_add((mix(key) >> self.shift) as usize & !(LINE - 1)).cast()
    }

    /// One more than the number of the user with key `key` (not 0), when the line its
    /// lookup begins at holds the key and its number: nearly every lookup after a user's
    /// first. 0 when the lookup has more to do, which `Users::short` then does.
    #[inline(always)]
    pub fn hit(&self, key: u64) -> u32 {
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
        // SAFETY: the line is four slots of the table, 64 bytes aligned to 64 (`Users::keys`
        // has why reading them whole is sound), and a hit's number is the word after its key.
        unsafe {
            use core::arch::x86_64::*;
            let line = self.line(key);
            let hit = _mm512_cmpeq_epi64_mask(_mm512_load_si512(line.cast()), _mm512_set1_epi64(key as i64)) & 0x55;
            if hit == 0 {
                return 0;
            }
            // 0 while the thread that claimed the slot is between its claim and its store.
            (*line.add(hit.trailing_zeros() as usize + 1)).load(Relaxed) as u32
        }
        #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f")))]
        {
            let _ = key;
            0
        }
    }
}

impl Users {
    /// A directory of `short` and `long` slots, each rounded up to a power of two (at least
    /// 64). No page of it is touched until a key lands there.
    pub fn new(short: usize, long: usize) -> Users {
        let (short, long) = (short.max(64).next_power_of_two(), long.max(64).next_power_of_two());
        // Every slot's number, and a block a thread may hold unused (64 threads' worth).
        let numbers = short + long + 64 * BLOCK as usize;
        assert!(numbers < NO_USER as usize, "{short} + {long} user slots: a number is 32 bits");
        // SAFETY: all zeros is a valid atomic integer.
        let (short_slots, long_slots, slot_of) = unsafe { (Zeroed::new(short), Zeroed::new(long), Zeroed::new(numbers)) };
        Users {
            short: short_slots,
            long: long_slots,
            short_shift: 64 - short.trailing_zeros(),
            long_shift: 64 - long.trailing_zeros(),
            slot_of,
            next: AtomicU32::new(0),
            made: MADE.fetch_add(1, Relaxed) + 1,
            full: AtomicBool::new(false),
        }
    }

    /// For about `rows` event lines: a user has 15 of them on generated data, so a slot for
    /// every 8 lines is half empty or more; one ID in eight is long.
    pub fn for_rows(rows: usize, grow: u32) -> Users {
        // `SS_USERS_SLOTS`: the short table's slots before any growing, for tests of a
        // directory that turns out too small.
        let short = crate::env::number("SS_USERS_SLOTS", (rows / 8).max(1 << 12)) << grow;
        Users::new(short, short / 4)
    }

    /// Piece `piece` of `pieces` of both tables onto whole 2 MB pages (`mem::touch`): a
    /// lookup reads a slot before anything is written to it. At any time, by any thread.
    pub fn touch(&self, piece: usize, pieces: usize) {
        crate::mem::touch(&self.short, piece, pieces, |slot| &slot[0]);
        crate::mem::touch(&self.long, piece, pieces, |slot| &slot[0]);
    }

    /// A bound on the numbers: every user's is below this. Far more than the users there
    /// are; an array of this many entries is touched only where a user's number is.
    pub fn bound(&self) -> usize {
        self.slot_of.len()
    }

    /// Numbers given out so far, counting the blocks threads hold: every user's number is
    /// below this too.
    pub fn count(&self) -> usize {
        (self.next.load(Relaxed) as usize).min(self.slot_of.len())
    }

    /// A number for the user whose slot is `slot`: the next of the calling thread's block.
    #[inline(always)]
    fn number(&self, slot: usize) -> u32 {
        let (made, next, end) = MINE.get();
        let number = if made == self.made && next < end {
            MINE.set((made, next + 1, end));
            next
        } else {
            self.block()
        };
        // SAFETY: `number < bound()`: `block` gives none past it.
        unsafe { self.slot_of.get_unchecked(number as usize) }.store(slot as u32, Relaxed);
        number
    }

    #[cold]
    #[inline(never)]
    fn block(&self) -> u32 {
        let first = self.next.fetch_add(BLOCK, Relaxed);
        if (first + BLOCK) as usize > self.slot_of.len() {
            // More threads than the room allowed for: as a full table.
            self.full.store(true, Relaxed);
            return 0;
        }
        MINE.set((self.made, first + 1, first + BLOCK));
        first
    }

    /// Whether some lookup found no room: the numbers given since then mean nothing, and
    /// the caller starts over with a larger directory.
    pub fn overflowed(&self) -> bool {
        self.full.load(Relaxed)
    }

    /// The short table for a loop of lookups (`ShortLines`).
    #[inline(always)]
    pub fn short_lines(&self) -> ShortLines {
        ShortLines { base: self.short.as_ptr(), shift: self.short_shift }
    }

    /// Asks for the cache line a short key's lookup begins at.
    #[inline(always)]
    pub fn prefetch_short(&self, key: u64) {
        crate::mem::prefetch(self.short.as_ptr().wrapping_add((mix(key) >> self.short_shift) as usize & !(LINE - 1)));
    }

    /// Asks for the cache line a long key's lookup begins at.
    #[inline(always)]
    pub fn prefetch_long(&self, hi: u64, lo: u64) {
        crate::mem::prefetch(self.long.as_ptr().wrapping_add((mix_long(hi, lo) >> self.long_shift) as usize));
    }

    /// The number of the user with short-table key `key` (not 0), which is given one if
    /// this is its first lookup.
    #[inline(always)]
    pub fn short(&self, key: u64) -> u32 {
        debug_assert!(key != 0);
        let mask = self.short.len() - 1;
        // A line of four slots at a time, from the key's own: its keys are compared at once,
        // so another user's key in the way costs no wrong guess (slot by slot, in a table a
        // third full, one lookup in six met one before its own, and the jump on "is this
        // slot's key mine" was a quarter of the parse's wrong guesses). A key goes into the
        // first empty slot of the first line that has one, and no key ever leaves: a lookup
        // that meets an empty slot has seen every place its key could be.
        //
        // Where the key is and where the first empty slot is are both taken from ONE
        // reading of the line. From two readings, another thread's claim of that slot
        // for this same key between them was seen by neither: the first found no such
        // key, the second found the slot taken and went on to the next, and the key got
        // a second slot and a second number (a user counted as two). A build that
        // optimizes made one reading of the two; the tests' build did not, and failed
        // one run in five.
        let mut at = (mix(key) >> self.short_shift) as usize & !(LINE - 1);
        for _ in 0..PROBES / LINE {
            // SAFETY: `at` is masked to the table, whose slots are a multiple of `LINE`: the
            // line's eight words, a key and a number in turn.
            let line = unsafe { std::slice::from_raw_parts(self.short.as_ptr().add(at & mask).cast::<AtomicU64>(), 2 * LINE) };
            loop {
                // Bit `2 * n` for slot `n`.
                let (hit, empty) = Self::keys(line, key);
                if hit != 0 {
                    return Self::numbered(&line[(hit.trailing_zeros() as usize + 1) & (2 * LINE - 1)]);
                }
                if empty == 0 {
                    break;
                }
                let word = empty.trailing_zeros() as usize & (2 * LINE - 2);
                match line[word].compare_exchange(0, key, Relaxed, Relaxed) {
                    Ok(_) => {
                        let number = self.number((at & mask) + word / 2);
                        line[word + 1].store(u64::from(number) + 1, Release);
                        return number;
                    }
                    Err(other) if other == key => return Self::numbered(&line[word + 1]),
                    // Another key took the slot this instant: the line again.
                    Err(_) => {}
                }
            }
            at += LINE;
        }
        self.overflow()
    }

    /// Which of a line's four keys (its even words) are `key`, and which of its slots are
    /// empty, as one reading of the line has them: bit `2 * n` for slot `n`.
    #[inline(always)]
    fn keys(line: &[AtomicU64], key: u64) -> (u32, u32) {
        debug_assert!(line.len() == 2 * LINE);
        // The line's 64 bytes in one register (the table is aligned far above a line, and a
        // line begins at a multiple of four slots).
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
        // SAFETY: eight words are 64 readable bytes. Other threads store into them
        // meanwhile, a word at a time, and an aligned word is read whole: each key read
        // here is zero or a key some thread put there, as a relaxed load of it would be.
        unsafe {
            use core::arch::x86_64::*;
            // Volatile: one reading, whatever the build makes of the code around it.
            let slots = line.as_ptr().cast::<__m512i>().read_volatile();
            (u32::from(_mm512_cmpeq_epi64_mask(slots, _mm512_set1_epi64(key as i64)) & 0x55), u32::from(_mm512_testn_epi64_mask(slots, slots) & 0x55))
        }
        #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f")))]
        {
            let (mut hit, mut empty) = (0u32, 0u32);
            for slot in 0..LINE {
                let held = line[2 * slot].load(Relaxed);
                hit |= u32::from(held == key) << (2 * slot);
                empty |= u32::from(held == 0) << (2 * slot);
            }
            (hit, empty)
        }
    }

    /// The number in a claimed slot's last word, waited for while the claiming thread is
    /// between its claim and its store.
    #[inline(always)]
    fn numbered(word: &AtomicU64) -> u32 {
        loop {
            let number = word.load(Acquire);
            if number != 0 {
                return (number - 1) as u32;
            }
            std::hint::spin_loop();
        }
    }

    /// The number of the user with long key `(hi, lo)`, as `short`.
    #[inline(never)]
    pub fn long(&self, hi: u64, lo: u64) -> u32 {
        let mask = self.long.len() - 1;
        let hash = mix_long(hi, lo);
        // Not `BUSY` and not empty.
        let tag = hash | 2;
        let mut at = (hash >> self.long_shift) as usize;
        let mut probes = 0;
        while probes < PROBES {
            // SAFETY: `at` is masked to the table.
            let slot = unsafe { self.long.get_unchecked(at & mask) };
            let held = slot[0].load(Acquire);
            if held == tag && slot[1].load(Relaxed) == hi && slot[2].load(Relaxed) == lo {
                return (slot[3].load(Relaxed) - 1) as u32;
            }
            if held == 0 {
                if slot[0].compare_exchange(0, BUSY, Acquire, Relaxed).is_ok() {
                    let number = self.number(self.short.len() + (at & mask));
                    slot[1].store(hi, Relaxed);
                    slot[2].store(lo, Relaxed);
                    slot[3].store(u64::from(number) + 1, Relaxed);
                    slot[0].store(tag, Release);
                    return number;
                }
                // Another worker took it this instant: look at what it puts there.
                continue;
            }
            if held == BUSY {
                std::hint::spin_loop();
                continue;
            }
            at += 1;
            probes += 1;
        }
        self.overflow()
    }

    #[cold]
    #[inline(never)]
    fn overflow(&self) -> u32 {
        self.full.store(true, Relaxed);
        0
    }

    /// The number of the user whose `user_id` is the `len` bytes at `p`, `NO_USER` for none
    /// (an empty field): the row code's lookup, for any spelling.
    ///
    /// # Safety
    /// `len` bytes readable at `p`.
    pub unsafe fn at(&self, p: *const u8, len: usize) -> u32 {
        // SAFETY: the caller's contract.
        self.get(unsafe { key_at(p, len) })
    }

    /// Asks for the cache line the lookup of `key` begins at.
    #[inline(always)]
    pub fn prefetch(&self, key: UserKey) {
        match key {
            UserKey::Short(0) => {}
            UserKey::Short(key) => self.prefetch_short(key),
            UserKey::Long(hi, lo) => self.prefetch_long(hi, lo),
        }
    }

    /// The number of the user with key `key` (`key_at`), `NO_USER` for the key of no ID.
    #[inline(always)]
    pub fn get(&self, key: UserKey) -> u32 {
        match key {
            UserKey::Short(0) => NO_USER,
            UserKey::Short(key) => self.short(key),
            UserKey::Long(hi, lo) => self.long(hi, lo),
        }
    }

    /// Asks for the cache line that says which slot user `user` has.
    #[inline(always)]
    pub fn prefetch_slot(&self, user: u32) {
        crate::mem::prefetch(self.slot_of.as_ptr().wrapping_add(user as usize));
    }

    /// Asks for the slot of user `user`: its key's cache line.
    #[inline(always)]
    pub fn prefetch_key(&self, user: u32) {
        let slot = self.slot_of[user as usize].load(Relaxed) as usize;
        if slot < self.short.len() {
            crate::mem::prefetch(self.short.as_ptr().wrapping_add(slot));
        } else {
            crate::mem::prefetch(self.long.as_ptr().wrapping_add(slot - self.short.len()));
        }
    }

    /// The key of user `user`, a number a lookup gave.
    #[inline]
    pub fn key(&self, user: u32) -> UserKey {
        let slot = self.slot_of[user as usize].load(Relaxed) as usize;
        if slot < self.short.len() {
            UserKey::Short(self.short[slot][0].load(Relaxed))
        } else {
            let slot = &self.long[slot - self.short.len()];
            UserKey::Long(slot[1].load(Relaxed), slot[2].load(Relaxed))
        }
    }

    /// Appends the `user_id` of user `user`.
    pub fn text(&self, user: u32, out: &mut Vec<u8>) {
        match self.key(user) {
            UserKey::Short(key) if key & SLOW != 0 => id::unpack(key, out),
            UserKey::Short(key) => {
                let (digits, count) = short_digits(key);
                out.push(b'u');
                out.extend_from_slice(&digits[..count]);
            }
            UserKey::Long(hi, lo) => {
                out.push(b'u');
                long_digits(hi, lo, out);
            }
        }
    }

    /// The `user_id` of user `user` as an `Id` (`types::id`): packed when its bytes pack,
    /// interned otherwise, as `id::pack` of its text.
    pub fn id(&self, user: u32) -> Id {
        match self.key(user) {
            UserKey::Short(key) if key & SLOW != 0 => {
                let mut text = Vec::new();
                id::unpack(key, &mut text);
                id::pack(&text)
            }
            UserKey::Short(key) => {
                let (digits, count) = short_digits(key);
                let mut text = [0u8; 16];
                text[0] = b'u';
                text[1..9].copy_from_slice(&digits);
                // SAFETY: 16 bytes are readable.
                unsafe { id::pack_at(text.as_ptr(), 1 + count) }
            }
            UserKey::Long(..) => {
                let mut text = Vec::with_capacity(33);
                self.text(user, &mut text);
                id::pack(&text)
            }
        }
    }

    /// Whether the `user_id` of user `a` is byte-wise below that of user `b`.
    pub fn less(&self, a: u32, b: u32) -> bool {
        if let (UserKey::Short(x), UserKey::Short(y)) = (self.key(a), self.key(b))
            && (x | y) & SLOW == 0
        {
            // The digits as text, the first in the top byte and zeros after the last: a
            // shorter ID that begins another compares below it, as its bytes do.
            let order = |key: u64| {
                let (digits, _) = short_digits(key);
                u64::from_be_bytes(digits)
            };
            return order(x) < order(y);
        }
        let (mut x, mut y) = (Vec::new(), Vec::new());
        self.text(a, &mut x);
        self.text(b, &mut y);
        x < y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(text: &[u8]) -> Option<u64> {
        let mut bytes = [0u8; 48];
        bytes[..text.len()].copy_from_slice(text);
        // SAFETY: 48 bytes are readable.
        unsafe { short_key_at(bytes.as_ptr(), text.len(), b'u') }
    }

    fn long(text: &[u8]) -> Option<(u64, u64)> {
        let mut bytes = [0u8; 48];
        bytes[..text.len()].copy_from_slice(text);
        // SAFETY: 48 bytes are readable.
        unsafe { long_key_at(bytes.as_ptr(), text.len(), b'u') }
    }

    /// The keys read a word at a time are the keys read a byte at a time, for IDs and for
    /// everything near one.
    #[test]
    fn wide_keys_are_the_keys() {
        let mut texts: Vec<Vec<u8>> = Vec::new();
        for id in [&b""[..], b"u", b"u0", b"u1", b"uz", b"u10", b"u01", b"u00", b"u1a0", b"uzzzzzzzz", b"u4k2nn2cj", b"u10000000", b"ua0b0c0d0", b"u123456789", b"U1", b"uA", b"u1 ", b" u1", b"e1", b"u1\r", b"u1_", b"u\xff", b"u1\x80", b"u/", b"u:", b"u`", b"u{", b"u9z", b"u0a"] {
            texts.push(id.to_vec());
        }
        // Every byte value at every digit's place of a full-width ID and of a short one.
        for base in [&b"u12345678"[..], b"uab"] {
            for at in 0..base.len() {
                for byte in 0..=255u8 {
                    let mut text = base.to_vec();
                    text[at] = byte;
                    texts.push(text);
                }
            }
        }
        let hex = b"u02423c3d84ab053af78764ce49f23b5c";
        for at in 0..hex.len() {
            for byte in [b'0', b'9', b'a', b'f', b'g', b'A', b'F', b'/', b':', b'`', 0, 0x80, 0xFF] {
                let mut text = hex.to_vec();
                text[at] = byte;
                texts.push(text);
            }
        }
        texts.push(hex[..32].to_vec());
        texts.push([&hex[..], b"c"].concat());
        for text in &texts {
            // What follows the ID must not matter: once zeros, once digits.
            for fill in [0u8, b'7'] {
                let mut bytes = text.clone();
                bytes.resize(text.len() + 48, fill);
                // SAFETY: 48 bytes are readable after the text.
                unsafe {
                    assert_eq!(short_key_wide(bytes.as_ptr(), text.len(), b'u'), short_key_at(bytes.as_ptr(), text.len(), b'u'), "{:?}", String::from_utf8_lossy(text));
                    // The long key against the digits read one by one.
                    let slow = (text.len() == 33 && text[0] == b'u' && text[1..].iter().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))).then(|| {
                        let nibble = |byte: u8| if byte.is_ascii_digit() { byte - b'0' } else { byte - b'a' + 10 };
                        let value: Vec<u8> = text[1..].chunks(2).map(|pair| nibble(pair[0]) << 4 | nibble(pair[1])).collect();
                        (u64::from_le_bytes(value[..8].try_into().unwrap()), u64::from_le_bytes(value[8..].try_into().unwrap()))
                    });
                    assert_eq!(long_key_at(bytes.as_ptr(), text.len(), b'u'), slow, "{:?}", String::from_utf8_lossy(text));
                    let short = short_key_at(bytes.as_ptr(), text.len(), b'u');
                    if short.is_some() || slow.is_some() || text.is_empty() {
                        assert_eq!(key_at_wide(bytes.as_ptr(), text.len()), key_at(bytes.as_ptr(), text.len()), "{:?}", String::from_utf8_lossy(text));
                    }
                }
            }
        }
    }

    #[test]
    fn keys_are_the_spellings_and_come_back() {
        let ids: [&[u8]; 9] = [b"u0", b"u1", b"uz", b"u10", b"u1a0", b"uzzzzzzzz", b"u4k2nn2cj", b"u10000000", b"ua0b0c0d0"];
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            let key = short(id).unwrap_or_else(|| panic!("{:?}", std::str::from_utf8(id)));
            assert!(key != 0 && key & SLOW == 0 && seen.insert(key));
            let (digits, count) = short_digits(key);
            assert_eq!(&digits[..count], &id[1..], "{:?}", std::str::from_utf8(id));
        }
        for bad in [&b""[..], b"u", b"U1", b"u01", b"u00", b"uA", b"u1 ", b" u1", b"e1", b"u123456789", b"u1\r", b"u1_", b"u\xff"] {
            assert_eq!(short(bad), None, "{:?}", std::str::from_utf8(bad));
        }
        let hex = b"u02423c3d84ab053af78764ce49f23b5c";
        let (hi, lo) = long(hex).unwrap();
        let mut back = Vec::new();
        long_digits(hi, lo, &mut back);
        assert_eq!(back, &hex[1..]);
        for bad in [&b"u02423c3d84ab053af78764ce49f23b5"[..], b"u02423c3d84ab053af78764ce49f23b5cc", b"e02423c3d84ab053af78764ce49f23b5c", b"u02423C3d84ab053af78764ce49f23b5c", b"u02423c3d84ab053af78764ce49f23b5g"] {
            assert_eq!(long(bad), None, "{:?}", std::str::from_utf8(bad));
        }
    }

    #[test]
    fn every_id_has_one_place_from_any_thread() {
        let users = Users::new(1 << 12, 1 << 10);
        let texts: Vec<Vec<u8>> = (0..1500u64)
            .map(|n| match n % 4 {
                0 => format!("u{:032x}", u128::from(n).wrapping_mul(0x9E37_79B9_7F4A_7C15_1234_5678_9ABC_DEF1)).into_bytes(),
                1 => format!("U{n}").into_bytes(),
                2 => format!("anonymous-user-{n}").into_bytes(),
                _ => format!("u{}", n + 1).into_bytes(),
            })
            .collect();
        let place = |text: &[u8]| {
            let mut bytes = text.to_vec();
            bytes.resize(text.len() + 16, 0);
            // SAFETY: the text's bytes are readable.
            unsafe { users.at(bytes.as_ptr(), text.len()) }
        };
        let (all, place_of) = (&texts, &place);
        let places: Vec<Vec<u32>> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..4).map(|turn| scope.spawn(move || all.iter().cycle().skip(turn * 377).take(all.len()).map(|text| place_of(text)).collect::<Vec<u32>>())).collect();
            threads.into_iter().map(|thread| thread.join().unwrap()).collect()
        });
        assert!(!users.overflowed());
        let first: Vec<u32> = texts.iter().map(|text| place(text)).collect();
        let distinct: std::collections::HashSet<u32> = first.iter().copied().collect();
        assert_eq!(distinct.len(), texts.len());
        // Numbers are a block a thread: all below the count, which is far below the bound.
        assert!(first.iter().all(|&user| (user as usize) < users.count()) && users.count() <= 8 * BLOCK as usize && users.count() < users.bound());
        for (turn, mine) in places.iter().enumerate() {
            for (at, &got) in mine.iter().enumerate() {
                assert_eq!(got, first[(at + turn * 377) % texts.len()]);
            }
        }
        for (text, &user) in texts.iter().zip(&first) {
            let mut back = Vec::new();
            users.text(user, &mut back);
            assert_eq!(&back, text);
            assert_eq!(users.id(user), id::pack(text));
        }
        // Byte order, across the three spellings.
        for a in (0..texts.len()).step_by(7) {
            for b in (0..texts.len()).step_by(13) {
                assert_eq!(users.less(first[a], first[b]), texts[a] < texts[b], "{:?} {:?}", String::from_utf8_lossy(&texts[a]), String::from_utf8_lossy(&texts[b]));
            }
        }
        assert_eq!(place(b""), NO_USER);
    }

    /// Every thread looks the same new keys up at the same moment, round after round: a
    /// key has one number whichever thread claimed its slot.
    #[test]
    fn threads_that_meet_on_a_new_key_get_one_number() {
        const THREADS: usize = 8;
        const KEYS: u64 = 4;
        for round in 0..3_000u64 {
            let users = Users::new(64, 64);
            let go = std::sync::Barrier::new(THREADS);
            let got: Vec<Vec<u32>> = std::thread::scope(|scope| {
                let threads: Vec<_> = (0..THREADS)
                    .map(|_| {
                        scope.spawn(|| {
                            go.wait();
                            // Keys of one line of the table, taken in one order by all.
                            (0..KEYS).map(|n| users.short(1 + round * KEYS + n)).collect::<Vec<u32>>()
                        })
                    })
                    .collect();
                threads.into_iter().map(|thread| thread.join().unwrap()).collect()
            });
            for mine in &got[1..] {
                assert_eq!(mine, &got[0], "round {round}");
            }
            for n in 0..KEYS {
                assert_eq!(users.short(1 + round * KEYS + n), got[0][n as usize], "round {round}");
            }
        }
    }

    #[test]
    fn a_full_table_says_so() {
        let users = Users::new(64, 64);
        for n in 0..5000u64 {
            users.short(n + 1);
        }
        assert!(users.overflowed());
    }
}
