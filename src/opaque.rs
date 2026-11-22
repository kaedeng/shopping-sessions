//! The stages between the parse and the sessions for IDs that are opaque strings
//! (`ingest::opaque` has the parse).
//!
//! 1. Parse: an event line becomes a record in the list of its `event_id`'s hash, its user
//!    looked up in the run's directory as the line is read; a click a record with its user's
//!    place; the links pairs of places, joined into persons by one worker beside the parse
//!    (`Persons`).
//! 2. Dedup: each list's records into a table that stays in cache, a slot an `event_id`,
//!    which keeps the copy with the smallest `ingest` (copies that tie are identical, rules
//!    "Events" step 2). A survivor that is valid goes to the bucket of its person, a click
//!    likewise: a person is the place of its component's root, an array read.
//! 3. Sessions: the classic bucket stage, `sessions::bucket`, with a person named by that
//!    place; the name, the smallest ID of the component, is read back from the directory
//!    when the rows are written.

use std::sync::OnceLock;

use ingest::Ingest;
use ingest::opaque::{Ev, EvL, EvN, LinkRec, Out, Placing, Plan};
use types::chunks::Chunks;
use types::hash::{hash, person_hash};
use types::parts::{Combine, Mode, Parts, Plain};
use types::record::{ClickN, ClickRec, PersonEvent};
use types::mem::Zeroed;
use types::id::SLOW;
use types::users::{UserKey, Users, mix_long, short_digits};
use types::{Ctx, Id, par};

use crate::Scattered;

/// The person buckets as ranges of the persons' numbers (`sessions::bucket_dense`): a
/// worker's lists of the events' keys (`person << 32 | ts`), of the purchases among them and
/// of the clicks, a list per bucket.
pub struct DenseBuckets {
    pub keys: Chunks<u64>,
    pub pays: Chunks<sessions::DensePay>,
    pub clicks: Chunks<ClickRec>,
}

impl DenseBuckets {
    /// Room for a worker's share of `events` and `clicks`, and a chunk a list to begin with.
    fn new(buckets: usize, events: usize, clicks: usize, workers: usize) -> DenseBuckets {
        let share = |rows: usize, size: usize| rows / workers * size * 9 / 8 + buckets * 4096 + (2 << 20);
        DenseBuckets { keys: Chunks::new(buckets, share(events, 8)), pays: Chunks::new(buckets, share(events / 16, 16)), clicks: Chunks::new(buckets, share(clicks, 24)) }
    }
}


/// Links `Persons::new` asks for the entries of ahead of.
const AHEAD: usize = 16;

/// Which person each user is, and what a person is called.
pub struct Persons {
    /// Per user's number: 1 + the user this one hangs under, which once the links are joined
    /// is its component's root. 0 for a root, and for a user no link names: its own person,
    /// called by its own ID.
    up: Zeroed<u32>,
    /// Per root: 1 + the user with the component's byte-wise smallest ID (0 before any link
    /// named the root), and `LinkRec::order` of that ID.
    names: Zeroed<(u32, u64)>,
}

impl Persons {
    /// The persons the links make: a union-find over the users' numbers, each root keeping
    /// its component's smallest ID as the links come.
    pub fn new(users: &Users, links: &[&[LinkRec]]) -> Persons {
        // Zeroed lazily by the kernel, and touched only where a user's number is: the
        // numbers are dense, so that is the arrays' first few megabytes.
        // SAFETY: all zeros is a valid integer, and a valid pair of them.
        let (mut up, mut names) = unsafe { (Zeroed::<u32>::new(users.bound()), Zeroed::<(u32, u64)>::new(users.bound())) };
        // The unions read an entry before they write it, which would leave the arrays on
        // 4 KB pages (`types::mem::touch`). The users the links name have the numbers given
        // out so far; the entries past them are only ever read.
        let numbered = users.count();
        up.touch_first(numbered);
        names.touch_first(numbered);
        // `SS_TAIL=small`: the entries past those are read for every row of a user that no
        // link names, and are zeros: kept on 4 KB pages, they read as one zero page.
        up.small_tail(numbered);
        names.small_tail(numbered);
        // The root of `x`, every user on the way pointed at its grandparent.
        fn find(up: &mut [u32], mut x: u32) -> u32 {
            loop {
                let above = up[x as usize];
                if above == 0 {
                    return x;
                }
                let further = up[above as usize - 1];
                if further != 0 {
                    up[x as usize] = further;
                }
                x = above - 1;
            }
        }
        // Whether the ID of `a` is the smaller: by the first 8 bytes, and by all of them
        // where those are the same.
        let less = |a: (u32, u64), b: (u32, u64)| if a.1 != b.1 { a.1 < b.1 } else { a.0 != b.0 && users.less(a.0, b.0) };
        let all = || links.iter().copied().flatten();
        // The entries of the link a few on are asked for as each link is taken: the links
        // come in no order.
        let ahead = |up: &[u32], names: &[(u32, u64)], next: Option<&LinkRec>| {
            if let Some(next) = next {
                for user in next.users {
                    types::mem::prefetch(up.as_ptr().wrapping_add(user as usize));
                    types::mem::prefetch(names.as_ptr().wrapping_add(user as usize));
                }
            }
        };
        let mut coming = all().skip(AHEAD);
        for link in all() {
            ahead(&up, &names, coming.next());
            let [a, b] = link.users;
            let (root_a, root_b) = (find(&mut up, a), find(&mut up, b));
            // A user's first link: so far it is its own component, named by itself.
            for (root, user, order) in [(root_a, a, link.order[0]), (root_b, b, link.order[1])] {
                let name = &mut names[root as usize];
                if name.0 == 0 {
                    debug_assert_eq!(root, user);
                    *name = (user + 1, order);
                }
            }
            if root_a != root_b {
                let (low, high) = (root_a.min(root_b), root_a.max(root_b));
                let (kept, gone) = (names[low as usize], names[high as usize]);
                up[high as usize] = low + 1;
                if less((gone.0 - 1, gone.1), (kept.0 - 1, kept.1)) {
                    names[low as usize] = gone;
                }
            }
        }
        types::tl::mark("links joined");
        // Every linked user straight under its root.
        let mut coming = all().skip(AHEAD);
        for link in all() {
            ahead(&up, &names, coming.next());
            for user in link.users {
                let root = find(&mut up, user);
                if root != user {
                    up[user as usize] = root + 1;
                }
            }
        }
        Persons { up, names }
    }

    /// The person of user `user`: its component's root.
    #[inline(always)]
    pub fn of(&self, user: u32) -> u32 {
        // SAFETY: a user's number is below the directory's bound, which the array has.
        let up = *unsafe { self.up.get_unchecked(user as usize) };
        if up == 0 { user } else { up - 1 }
    }

    /// Asks for the cache line `of(user)` reads.
    #[inline(always)]
    pub fn prefetch(&self, user: u32) {
        types::mem::prefetch(self.up.as_ptr().wrapping_add(user as usize));
    }

    /// Asks for the cache line `name(person)` reads.
    #[inline(always)]
    pub fn prefetch_name(&self, person: u32) {
        types::mem::prefetch(self.names.as_ptr().wrapping_add(person as usize));
    }

    /// The user whose ID names person `person` (a root): its component's smallest.
    pub fn name(&self, person: u32) -> u32 {
        let name = self.names[person as usize].0;
        if name == 0 { person } else { name - 1 }
    }
}

/// An event record of either key width, as the dedup reads it.
trait Row: Copy {
    fn slot(&self) -> u64;
    fn same(&self, other: &Self) -> bool;
    /// `(user | ts << 32, ingest | pay << 32)`.
    fn words(&self) -> (u64, u64);
}

impl Row for Ev {
    #[inline(always)]
    fn slot(&self) -> u64 {
        hash(self.key)
    }

    #[inline(always)]
    fn same(&self, other: &Ev) -> bool {
        self.key == other.key
    }

    #[inline(always)]
    fn words(&self) -> (u64, u64) {
        (self.user_ts, self.ingest_pay)
    }
}

impl Row for EvN {
    #[inline(always)]
    fn slot(&self) -> u64 {
        hash(self.key())
    }

    #[inline(always)]
    fn same(&self, other: &EvN) -> bool {
        self.key() == other.key()
    }

    #[inline(always)]
    fn words(&self) -> (u64, u64) {
        (self.user_ts(), u64::from(self.ingest()))
    }
}

impl Row for EvL {
    #[inline(always)]
    fn slot(&self) -> u64 {
        mix_long(self.hi, self.lo)
    }

    #[inline(always)]
    fn same(&self, other: &EvL) -> bool {
        self.hi == other.hi && self.lo == other.lo
    }

    #[inline(always)]
    fn words(&self) -> (u64, u64) {
        (self.user_ts, self.ingest_pay)
    }
}

/// A worker's buffers for one list's dedup.
#[derive(Default)]
pub struct Scratch {
    /// Each slot 0 or 1 + an index into the kept rows.
    table: Vec<u32>,
    kept: Vec<Ev>,
    kept_narrow: Vec<EvN>,
    kept_long: Vec<EvL>,
}

/// Slots per row, rounded up to a power of two: the table ends a quarter full or less.
/// `SS_DEDUP_ROOM` sets it.
const ROOM: usize = 4;

/// How `dedup` reads a list and probes its table: the switches, read once.
#[derive(Clone, Copy)]
struct Way {
    /// Rows `dedup` asks for the slot of ahead of the row it is at: `SS_DEDUP_AHEAD`, 8 unless
    /// set, 0 for none. A row asked for is 11 instructions more (2% of a run's), for 6% of the
    /// stage's time at 21,000 rows a list (a table of half a megabyte) on a machine with a core
    /// a worker; see `SS_ASK` in `ingest::opaque::kern`.
    ahead: usize,
    /// `SS_DEDUP_ROOM`: slots per row before the rounding, `ROOM` unless set. 2 makes the
    /// table at 20M events a quarter of a megabyte, between a quarter and half full.
    room: usize,
    /// `SS_DEDUP_READ`: rows `dedup` asks for the input row ahead of the row it is at, on over
    /// the end of a chunk into the next (`fill_piece`); 0, unless set, for none, which with
    /// `SS_DEDUP_PROBE=one` is the loop as it was.
    read: usize,
    /// `SS_DEDUP_PROBE=one|tag`: a slot holds 1 + a kept row's index (`one`, the pick), or
    /// that and above it hash bits of the row's key, so that a slot of another key is passed
    /// over without its kept row read (`tag_row`).
    tag: bool,
}

fn way() -> Way {
    static WAY: OnceLock<Way> = OnceLock::new();
    *WAY.get_or_init(|| Way {
        ahead: types::env::number("SS_DEDUP_AHEAD", 8).min(64),
        room: types::env::number("SS_DEDUP_ROOM", ROOM).clamp(1, 64),
        read: types::env::number("SS_DEDUP_READ", 0).min(4096),
        tag: types::env::pick("SS_DEDUP_PROBE", &["one", "tag"]) == 1,
    })
}

/// List `list` of every worker deduplicated into `kept`: one record per `event_id`, the copy
/// with the smallest `ingest`. `bits` are the hash bits that chose the list.
fn dedup<'p, R: Row + 'p>(lists: impl Iterator<Item = &'p Chunks<R>> + Clone, list: usize, bits: u32, table: &mut Vec<u32>, kept: &mut Vec<R>) {
    dedup_as(way(), lists, list, bits, table, kept);
}

/// `dedup` with the switches `way`.
#[inline(always)]
fn dedup_as<'p, R: Row + 'p>(way: Way, lists: impl Iterator<Item = &'p Chunks<R>> + Clone, list: usize, bits: u32, table: &mut Vec<u32>, kept: &mut Vec<R>) {
    kept.clear();
    let rows: usize = lists.clone().map(|chunks| chunks.len_of(list)).sum();
    if rows == 0 {
        return;
    }
    let slot_bits = (way.room * rows).next_power_of_two().max(1024).trailing_zeros();
    assert!(bits + slot_bits <= 64 && rows < u32::MAX as usize);
    let (shift, mask) = (64 - bits - slot_bits, (1usize << slot_bits) - 1);
    table.clear();
    table.resize(mask + 1, 0);
    kept.reserve(rows);
    // `tag`: a slot's low bits hold 1 + the index of a kept row, at least 8 bits above them
    // hash bits of its key.
    let low_bits = usize::BITS - rows.leading_zeros();
    let tag = way.tag && low_bits <= 24;
    if tag || way.read != 0 {
        let fill = Fill {
            table: table.as_mut_ptr(),
            shift,
            mask,
            low: u32::MAX >> (32 - low_bits),
            low_bits,
            tag_shift: shift.saturating_sub(32 - low_bits),
            kept: kept.as_mut_ptr(),
            ahead: way.ahead,
            read: way.read,
        };
        let mut count = 0;
        // SAFETY: the table has `mask + 1` slots; `kept` has room for every row of the list,
        // and is empty.
        if tag {
            paired(lists, list, |piece, next| count = unsafe { fill_piece::<R, true>(fill, count, piece, next) });
        } else {
            paired(lists, list, |piece, next| count = unsafe { fill_piece::<R, false>(fill, count, piece, next) });
        }
        // SAFETY: rows `..count` were written, each a copy of a row of the list.
        unsafe { kept.set_len(count) };
        return;
    }
    let ahead = way.ahead;
    for chunks in lists {
        chunks.for_each(list, |piece| {
            for (number, row) in piece.iter().enumerate() {
                // `SS_DEDUP_AHEAD`: the slot of the row a few on (at 60M events a list's
                // table is half a megabyte and its kept rows more, past L2 together).
                if ahead != 0
                    && let Some(coming) = piece.get(number + ahead)
                {
                    types::mem::prefetch(table.as_ptr().wrapping_add((coming.slot() >> shift) as usize & mask));
                }
                let mut at = (row.slot() >> shift) as usize & mask;
                loop {
                    // SAFETY: `at` is at most `mask`, and the table has `mask + 1` slots.
                    let slot = unsafe { table.get_unchecked_mut(at) };
                    if *slot == 0 {
                        kept.push(*row);
                        *slot = kept.len() as u32;
                        break;
                    }
                    // SAFETY: a slot is set to the kept length right after a push.
                    let first = unsafe { kept.get_unchecked_mut(*slot as usize - 1) };
                    if first.same(row) {
                        if (row.words().1 as u32) < (first.words().1 as u32) {
                            *first = *row;
                        }
                        break;
                    }
                    at = (at + 1) & mask;
                }
            }
        });
    }
}

/// In a slot of `dedup_short`'s table: the row is one of the wider records kept.
const WIDE: u32 = 1 << 31;

/// `dedup` for the short-keyed events of list `list`: every worker's records of that list,
/// the narrow ones into `kept` and the wider rows, which lie among them as two records each
/// (`EvN::head`), into `wide`. One table for both, by the key as `canon` has it, which both
/// kinds of record hold: a copy of an `event_id` is found whichever record its row became,
/// in one pass over the list. A record that loses to one of the other kind loses its `ts`:
/// no row any more for the placing loop.
///
/// Where no row may be narrow (`Plan::narrow`) the rows are an `Ev` each in lists of their
/// own, and this is `dedup` of those.
///
/// The loop keeps what it reads in locals and writes the kept rows through pointers into
/// room made before it. As a closure over the table, the two vectors, the shift and the
/// mask it read each of them from memory again for every record (a store through a pointer
/// may be to the closure's own fields, for all the compiler knows), and every push checked
/// the vector's room: 17 instructions a record more than this.
fn dedup_short(parsed: &[Out], list: usize, plan: &Plan, table: &mut Vec<u32>, kept: &mut Vec<EvN>, wide: &mut Vec<Ev>) {
    kept.clear();
    if !plan.narrow {
        return dedup(parsed.iter().map(|out| &out.events), list, plan.part_bits, table, wide);
    }
    wide.clear();
    // Records: a few more than the rows, a wider row being two and a pad one.
    let rows: usize = parsed.iter().map(|out| out.narrow.len_of(list)).sum();
    if rows == 0 {
        return;
    }
    let way = way();
    // `SS_DEDUP_PROBE=tag`: a slot's low bits hold 1 + the index of a kept row, the bits
    // between them and `WIDE` (8 at the least) hash bits of its key.
    let low_bits = usize::BITS - rows.leading_zeros();
    if way.tag && low_bits <= 23 {
        dedup_short_as::<true>(way, parsed, list, plan, rows, low_bits, table, kept, wide);
    } else {
        dedup_short_as::<false>(way, parsed, list, plan, rows, low_bits, table, kept, wide);
    }
}

/// `dedup_short` with the switches `way` (`SS_DEDUP_ROOM`, `SS_DEDUP_READ`, `SS_DEDUP_AHEAD`),
/// and with tagged slots where `TAG`. `rows` is the list's count of records, below
/// `1 << low_bits`.
///
/// `SS_DEDUP_READ`: the parse wrote the lists with streaming stores, a 4 KB chunk at a time
/// from anywhere in its block, so a list is read from memory, and the hardware's prefetch
/// starts over at every chunk; the record `read` on is asked for, on over a chunk's end into
/// the next (`paired`). `SS_DEDUP_AHEAD`'s slot likewise.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn dedup_short_as<const TAG: bool>(way: Way, parsed: &[Out], list: usize, plan: &Plan, rows: usize, low_bits: u32, table: &mut Vec<u32>, kept: &mut Vec<EvN>, wide: &mut Vec<Ev>) {
    let slot_bits = (way.room * rows).next_power_of_two().max(1024).trailing_zeros();
    assert!(plan.part_bits + slot_bits <= 64 && rows < WIDE as usize);
    let (shift, mask) = (64 - plan.part_bits - slot_bits, (1usize << slot_bits) - 1);
    table.clear();
    table.resize(mask + 1, 0);
    // A narrow record is kept once at the most, and so is a wider row, which is two records.
    kept.reserve(rows);
    wide.reserve(rows / 2 + 1);
    let (ahead, read) = (way.ahead, way.read);
    // A slot is `WIDE` or not, its tag (`TAG` only) and 1 + the index of a kept row: the
    // index in the bits of `low`, the tag in those of `field`, hash bits of the key from
    // `tag_shift` up. Without `TAG` the index is all but `WIDE` and there is no tag.
    let low: u32 = if TAG { u32::MAX >> (32 - low_bits) } else { !WIDE };
    let field: u32 = !low & !WIDE;
    let tag_shift = shift.saturating_sub(31 - low_bits);
    const KEY: u64 = (1 << 48) - 1;
    let (slots, kept_at, wide_at) = (table.as_mut_ptr(), kept.as_mut_ptr(), wide.as_mut_ptr());
    let (mut kept_len, mut wide_len) = (0usize, 0usize);
    paired(parsed.iter().map(|out| &out.narrow), list, |piece, next| {
        let (slots, kept_at, wide_at, shift, mask, ahead, read, low, field, tag_shift) = (slots, kept_at, wide_at, shift, mask, ahead, read, low, field, tag_shift);
        let tag_of = |hashed: u64| if TAG { ((hashed >> tag_shift) as u32) << low_bits & field } else { 0 };
        let (mut kept_n, mut wide_n) = (kept_len, wide_len);
        let (records, count) = (piece.as_ptr(), piece.len());
        let (later, later_count) = (next.as_ptr(), next.len());
        let mut number = 0;
        // SAFETY: `number < count` where a record is read, and a wider row's second
        // record is in the piece its first is in (`Chunks::push_pair_at`); a record of
        // `next` is read only below `later_count`. A slot's index is masked to the table.
        // `kept_n` and `wide_n` grow by one a record or a pair of them at the most, so they
        // stay inside the room reserved above (below `1 << low_bits` either way); a slot
        // holds one more than the index of a row written before it was set.
        unsafe {
            while number < count {
                let row = *records.add(number);
                if read != 0 {
                    let on = number + read;
                    if on < count {
                        types::mem::prefetch(records.add(on));
                    } else if on - count < later_count {
                        types::mem::prefetch(later.add(on - count));
                    }
                }
                // `SS_DEDUP_AHEAD`: the slot of the record a few on (some slot, for a
                // record that is no narrow one).
                if ahead != 0 {
                    let on = number + ahead;
                    let coming = if on < count {
                        *records.add(on)
                    } else if on - count < later_count {
                        *later.add(on - count)
                    } else {
                        *records.add(count - 1)
                    };
                    types::mem::prefetch(slots.add((hash(coming.a & KEY) >> shift) as usize & mask));
                }
                if row.a & EvN::HEAD != EvN::HEAD {
                    number += 1;
                    let key = row.a & KEY;
                    let hashed = hash(key);
                    let tag = tag_of(hashed);
                    let mut at = (hashed >> shift) as usize & mask;
                    loop {
                        let slot = slots.add(at);
                        let held = *slot;
                        if held == 0 {
                            kept_at.add(kept_n).write(row);
                            kept_n += 1;
                            *slot = tag | kept_n as u32;
                            break;
                        }
                        if !TAG || held & field == tag {
                            if held & WIDE == 0 {
                                let first = &mut *kept_at.add((held & low) as usize - 1);
                                if first.a & KEY == key {
                                    if row.ingest() < first.ingest() {
                                        *first = row;
                                    }
                                    break;
                                }
                            } else {
                                let first = &mut *wide_at.add((held & low) as usize - 1);
                                if first.key == key {
                                    if row.ingest() < first.ingest_pay as u32 {
                                        first.user_ts &= 0xFFFF_FFFF;
                                        kept_at.add(kept_n).write(row);
                                        kept_n += 1;
                                        *slot = tag | kept_n as u32;
                                    }
                                    break;
                                }
                            }
                        }
                        at = (at + 1) & mask;
                    }
                    continue;
                }
                if row.a == EvN::PAD.a {
                    number += 1;
                    continue;
                }
                // A wider row: its key here, its other two words in the record after.
                assert!(number + 1 < count, "a wider row's second record is in its piece");
                let rest = *records.add(number + 1);
                number += 2;
                let row = Ev { key: row.b, user_ts: rest.a, ingest_pay: rest.b };
                let key = row.key;
                let hashed = hash(key);
                let tag = tag_of(hashed);
                let mut at = (hashed >> shift) as usize & mask;
                loop {
                    let slot = slots.add(at);
                    let held = *slot;
                    if held == 0 {
                        wide_at.add(wide_n).write(row);
                        wide_n += 1;
                        *slot = WIDE | tag | wide_n as u32;
                        break;
                    }
                    if !TAG || held & field == tag {
                        if held & WIDE != 0 {
                            let first = &mut *wide_at.add((held & low) as usize - 1);
                            if first.key == key {
                                if (row.ingest_pay as u32) < (first.ingest_pay as u32) {
                                    *first = row;
                                }
                                break;
                            }
                        } else {
                            let first = &mut *kept_at.add((held & low) as usize - 1);
                            if first.a & KEY == key {
                                if (row.ingest_pay as u32) < first.ingest() {
                                    first.b &= !EvN::TS;
                                    wide_at.add(wide_n).write(row);
                                    wide_n += 1;
                                    *slot = WIDE | tag | wide_n as u32;
                                }
                                break;
                            }
                        }
                    }
                    at = (at + 1) & mask;
                }
            }
        }
        (kept_len, wide_len) = (kept_n, wide_n);
    });
    // SAFETY: that many rows were written into the room reserved, each once.
    unsafe {
        kept.set_len(kept_len);
        wide.set_len(wide_len);
    }
}

/// A list's table and kept rows as `fill_piece` fills them (the count of kept rows apart).
#[derive(Clone, Copy)]
struct Fill<R> {
    /// `mask + 1` slots; a row's place is `(slot() >> shift) & mask`.
    table: *mut u32,
    shift: u32,
    mask: usize,
    /// `tag`: a slot is `tag | 1 + index`, the index in the bits of `low` (the low
    /// `low_bits`), the tag the key's hash bits from `tag_shift` up, placed above them.
    low: u32,
    low_bits: u32,
    tag_shift: u32,
    /// Room for every row of the list.
    kept: *mut R,
    /// `Way::ahead`, `Way::read`.
    ahead: usize,
    read: usize,
}

/// Each piece of list `list` of every worker with the piece after it: the next worker's first
/// where a worker's list ends, none after the last.
#[inline(always)]
fn paired<'p, R: Copy + 'p>(lists: impl Iterator<Item = &'p Chunks<R>>, list: usize, mut each: impl FnMut(&'p [R], &'p [R])) {
    let mut held: &'p [R] = &[];
    for chunks in lists {
        chunks.for_each(list, |piece| {
            if !held.is_empty() {
                each(held, piece);
            }
            held = piece;
        });
    }
    if !held.is_empty() {
        each(held, &[]);
    }
}

/// Row `at` counted on from the start of `piece` into `next`.
#[inline(always)]
fn ahead_of<'a, R>(piece: &'a [R], next: &'a [R], at: usize) -> Option<&'a R> {
    match piece.get(at) {
        Some(row) => Some(row),
        None => next.get(at - piece.len()),
    }
}

/// The rows of `piece` into the table, `tag_row` or `one_row`, `count` rows kept before them;
/// asked for ahead, as far as into `next`, are the slots of the rows `ahead` on and the rows
/// `read` on. Returns the rows kept after them.
///
/// The parse wrote its lists with streaming stores, so a list is read from memory, a chunk at
/// a time from anywhere in the block: the first read of each line of it was 18% of the cycles
/// of the dedup tasks. On the two threads of a core the dedup is short of instructions as much
/// as of memory, so the rows whose rows ahead are in the same piece (all but the last
/// `ahead.max(read)`) ask for them with no check on where they are.
///
/// # Safety
/// As `Fill` says: the table and `kept` are as large as it says, and `kept` holds `count`.
#[inline(never)]
unsafe fn fill_piece<R: Row, const TAG: bool>(fill: Fill<R>, mut count: usize, piece: &[R], next: &[R]) -> usize {
    let slot_of = |row: &R| fill.table.wrapping_add((row.slot() >> fill.shift) as usize & fill.mask);
    let inside = piece.len().saturating_sub(fill.ahead.max(fill.read));
    for (number, row) in piece[..inside].iter().enumerate() {
        // SAFETY: `number + ahead` and `number + read` are below `inside + ahead.max(read)`,
        // at most the piece's length (a distance of 0 asks for the row itself); the rest is
        // the caller's.
        unsafe {
            types::mem::prefetch(slot_of(piece.get_unchecked(number + fill.ahead)));
            types::mem::prefetch(piece.as_ptr().add(number + fill.read));
            count = if TAG { tag_row(&fill, count, row) } else { one_row(&fill, count, row) };
        }
    }
    for (number, row) in piece.iter().enumerate().skip(inside) {
        if fill.ahead != 0
            && let Some(coming) = ahead_of(piece, next, number + fill.ahead)
        {
            types::mem::prefetch(slot_of(coming));
        }
        if fill.read != 0
            && let Some(coming) = ahead_of(piece, next, number + fill.read)
        {
            types::mem::prefetch(coming as *const R);
        }
        // SAFETY: the caller's.
        count = unsafe { if TAG { tag_row(&fill, count, row) } else { one_row(&fill, count, row) } };
    }
    count
}

/// `row` into a table a slot at a time, as `dedup` does: a slot is 1 + a kept row's index.
/// Returns the rows kept.
///
/// # Safety
/// As `fill_piece`.
#[inline(always)]
unsafe fn one_row<R: Row>(f: &Fill<R>, count: usize, row: &R) -> usize {
    let mut at = (row.slot() >> f.shift) as usize & f.mask;
    // SAFETY: `at` is at most `mask`; a slot is set to the count of kept rows right after the
    // row is written.
    unsafe {
        loop {
            let slot = &mut *f.table.add(at);
            if *slot == 0 {
                f.kept.add(count).write(*row);
                *slot = count as u32 + 1;
                return count + 1;
            }
            let first = &mut *f.kept.add(*slot as usize - 1);
            if first.same(row) {
                if (row.words().1 as u32) < (first.words().1 as u32) {
                    *first = *row;
                }
                return count;
            }
            at = (at + 1) & f.mask;
        }
    }
}

/// `one_row` with the slots tagged: a slot holding another tag is another key's, passed over
/// without its kept row read (a read from anywhere in the kept rows, and at 2 slots a row a
/// row's place is another key's a quarter of the time). Returns the rows kept.
///
/// # Safety
/// As `fill_piece`.
#[inline(always)]
unsafe fn tag_row<R: Row>(f: &Fill<R>, count: usize, row: &R) -> usize {
    let hash = row.slot();
    let mut at = (hash >> f.shift) as usize & f.mask;
    let tag = ((hash >> f.tag_shift) as u32) << f.low_bits;
    // SAFETY: as `one_row`; a slot holds `tag | 1 + index` of a kept row.
    unsafe {
        loop {
            let slot = &mut *f.table.add(at);
            if *slot == 0 {
                f.kept.add(count).write(*row);
                *slot = tag | (count as u32 + 1);
                return count + 1;
            }
            if *slot & !f.low == tag {
                let first = &mut *f.kept.add((*slot & f.low) as usize - 1);
                if first.same(row) {
                    if (row.words().1 as u32) < (first.words().1 as u32) {
                        *first = *row;
                    }
                    return count;
                }
            }
            at = (at + 1) & f.mask;
        }
    }
}

/// Rows a placing loop asks for the person of ahead of the row it is at.
///
/// The persons' array is 4 bytes a user, 18 MB at 60M events, and a list's rows name users
/// in no order: every lookup is a line from outside the caches. Asked for all at once before
/// the loop, a list's lines (31,000 of them at 60M) were gone from L2 again before the loop
/// came to read them, and that one load was a tenth of a run's cycles.
const AHEAD_ROWS: usize = 32;

/// The valid ones of `kept` to their persons' buckets.
#[inline(always)]
fn scatter<R: Row, M: Mode>(kept: &[R], persons: &Persons, bucket_bits: u32, out: &mut Parts<PersonEvent>) {
    let shift = 64 - bucket_bits;
    for row in kept.iter().take(AHEAD_ROWS) {
        persons.prefetch(row.words().0 as u32);
    }
    for (at, row) in kept.iter().enumerate() {
        if let Some(coming) = kept.get(at + AHEAD_ROWS) {
            persons.prefetch(coming.words().0 as u32);
        }
        let (user_ts, ingest_pay) = row.words();
        let ts = (user_ts >> 32) as u32;
        // No `ts`: the row that won its `event_id` is not valid.
        if ts != 0 {
            // A person's key is its root's number and one: 0 is no ID.
            let person = u64::from(persons.of(user_ts as u32)) + 1;
            // SAFETY: the shift leaves `bucket_bits` bits, and `out` has that many buckets.
            unsafe { out.push_as::<M>((person_hash(person) >> shift) as usize, PersonEvent { person, ts, pay: (ingest_pay >> 32) as u32 }) };
        }
    }
    // Streamed lines become visible to the sessions stage's threads.
    out.fence();
}

/// `scatter` into buckets that are ranges of the persons' numbers: bucket
/// `(person - 1) >> shift`, an event the key `person << 32 | ts`.
#[inline(always)]
fn scatter_dense<R: Row>(kept: &[R], persons: &Persons, shift: u32, out: &mut DenseBuckets) {
    for row in kept.iter().take(AHEAD_ROWS) {
        persons.prefetch(row.words().0 as u32);
    }
    for (at, row) in kept.iter().enumerate() {
        if let Some(coming) = kept.get(at + AHEAD_ROWS) {
            persons.prefetch(coming.words().0 as u32);
        }
        let (user_ts, ingest_pay) = row.words();
        let ts = (user_ts >> 32) as u32;
        if ts != 0 {
            let root = persons.of(user_ts as u32);
            let (list, key) = ((root >> shift) as usize, u64::from(root + 1) << 32 | u64::from(ts));
            out.keys.push(list, key);
            let pay = (ingest_pay >> 32) as u32;
            if pay != 0 {
                out.pays.push(list, sessions::DensePay { key, pay, pad: 0 });
            }
        }
    }
    out.keys.fence();
}

/// `scatter_clicks` into the same buckets. With `recycle`, a chunk of the worker's list goes
/// to the buckets' lists once its clicks are placed (`Chunks::give`).
#[inline(always)]
fn scatter_clicks_dense(from: &Out, persons: &Persons, shift: u32, recycle: bool, out: &mut DenseBuckets) {
    #[inline(always)]
    fn place(piece: &[ingest::opaque::Ck], persons: &Persons, shift: u32, out: &mut DenseBuckets) {
        for click in piece {
            let root = persons.of(click.user_ts as u32);
            let rec = ClickRec { user: u64::from(root) + 1, click: click.click, ts: (click.user_ts >> 32) as u32, campaign: click.campaign as u32 };
            out.clicks.push((root >> shift) as usize, rec);
        }
    }
    // A piece is a chunk of the list, 168 clicks: its persons are asked for when it comes,
    // and placed is the piece before it, asked for a piece ago.
    let mut asked: (&[ingest::opaque::Ck], usize) = (&[], 0);
    from.clicks.for_each_chunk(0, |piece, chunk| {
        for click in piece {
            persons.prefetch(click.user_ts as u32);
        }
        place(asked.0, persons, shift, out);
        if recycle && asked.1 != 0 {
            // SAFETY: the chunk's clicks are placed and the piece after it has come: the
            // worker's list, which outlives the buckets, is not read there again.
            unsafe { out.clicks.give(asked.1) };
        }
        asked = (piece, chunk);
    });
    place(asked.0, persons, shift, out);
    out.clicks.fence();
}

/// The chunks of list `list` of every worker's parse, which the dedup has just read, to the
/// buckets' lists of the worker that read them (`Chunks::give`): a few for the purchases'
/// lists, the rest for the keys'.
fn give_chunks<R: Copy>(parsed: &[Out], of: impl Fn(&Out) -> &Chunks<R>, list: usize, ranges: &mut DenseBuckets) {
    for out in parsed {
        of(out).chunks_of(list, |chunk| {
            // SAFETY: the parse's lists outlive the buckets' (neither is dropped before the
            // run ends), a list is read by the one task that deduplicates it, and that
            // task has read it.
            unsafe { if ranges.pays.spare() < 4 { ranges.pays.give(chunk) } else { ranges.keys.give(chunk) } };
        });
    }
}

/// A worker's clicks to their persons' buckets.
#[inline(always)]
fn scatter_clicks<M: Mode>(from: &Out, persons: &Persons, bucket_bits: u32, out: &mut Parts<ClickRec>) {
    let shift = 64 - bucket_bits;
    from.clicks.for_each(0, |piece| {
        for click in piece {
            persons.prefetch(click.user_ts as u32);
        }
        for click in piece {
            let person = u64::from(persons.of(click.user_ts as u32)) + 1;
            let rec = ClickRec { user: person, click: click.click, ts: (click.user_ts >> 32) as u32, campaign: click.campaign as u32 };
            // SAFETY: as `scatter`.
            unsafe { out.push_as::<M>((person_hash(person) >> shift) as usize, rec) };
        }
    });
    out.fence();
}

/// What the stages here leave for the sessions stage: the person buckets, and the names.
pub struct Staged {
    pub scattered: Vec<Scattered>,
    pub names: Names,
    /// The buckets as ranges of the persons' numbers (`Scattered::dense`): the shift from a
    /// number less one to its bucket, and the buckets.
    pub dense: Option<(u32, usize)>,
}

/// What a person's place is called.
pub struct Names {
    plan: Plan,
    persons: Persons,
}

impl Names {
    /// Appends the names of the persons whose keys are `keys` (a root's number and one), as
    /// `output::user_daily_named` wants them: each name's length in four little-endian
    /// bytes, then its bytes. A name is read through three tables (the root's name, that
    /// user's slot, the slot's key), each far larger than the caches, so the keys go a few
    /// at a time: every table's lines are asked for before any is read.
    ///
    /// An entry is stored, not pushed: room for every key's entry at its longest is made
    /// first, a short name is its length, the `u` and a word of digits whatever their
    /// count, and a long one's 32 digits come a byte's two at a time from a table. (Pushed
    /// byte by byte through `Users::text`, each with its test for room, a name was 90
    /// cycles, and the names a fifth of the output stage.)
    pub fn texts(&self, keys: &[Id], out: &mut Vec<u8>) {
        let users = &self.plan.users;
        let mut named = [0u32; NAMES_HELD];
        out.reserve(keys.len() * ENTRY_MOST + 16);
        let (mut at, mut left) = (out.len(), keys.len());
        for held in keys.chunks(NAMES_HELD) {
            for &key in held {
                self.persons.prefetch_name(key as u32 - 1);
            }
            for (name, &key) in named.iter_mut().zip(held) {
                *name = self.persons.name(key as u32 - 1);
                users.prefetch_slot(*name);
            }
            for &name in &named[..held.len()] {
                users.prefetch_key(name);
            }
            for &name in &named[..held.len()] {
                left -= 1;
                // SAFETY: `out` has room for `ENTRY_MOST` bytes for this key and each of
                // the `left` after it, and 16 more: an entry here is at most `ENTRY_MOST`,
                // and a short one's word of digits ends inside them.
                unsafe {
                    let to = out.as_mut_ptr().add(at);
                    match users.key(name) {
                        UserKey::Short(key) if key & SLOW != 0 => at = interned(key, out, at, left),
                        UserKey::Short(key) => {
                            let (digits, count) = short_digits(key);
                            to.cast::<u32>().write_unaligned((1 + count as u32).to_le());
                            *to.add(4) = b'u';
                            to.add(5).cast::<[u8; 8]>().write_unaligned(digits);
                            at += 5 + count;
                        }
                        UserKey::Long(hi, lo) => {
                            to.cast::<u32>().write_unaligned(33u32.to_le());
                            *to.add(4) = b'u';
                            for (place, byte) in hi.to_le_bytes().into_iter().chain(lo.to_le_bytes()).enumerate() {
                                to.add(5 + 2 * place).cast::<[u8; 2]>().write_unaligned(HEX[usize::from(byte)]);
                            }
                            at += ENTRY_MOST;
                        }
                    }
                }
            }
        }
        // SAFETY: the bytes up to `at` were written, inside the room made.
        unsafe { out.set_len(at) };
    }
}

/// The entry of a name the directory holds interned (not a `u` and digits): any length, so
/// through the vector, and room made again for the `left` entries after it. Returns where
/// the next entry goes.
#[cold]
#[inline(never)]
fn interned(key: u64, out: &mut Vec<u8>, at: usize, left: usize) -> usize {
    // SAFETY: the bytes up to `at` were written.
    unsafe { out.set_len(at) };
    out.extend_from_slice(&[0; 4]);
    types::id::unpack(key, out);
    let len = (out.len() - at - 4) as u32;
    out[at..at + 4].copy_from_slice(&len.to_le_bytes());
    out.reserve(left * ENTRY_MOST + 16);
    out.len()
}

/// Names `Names::texts` reads together.
const NAMES_HELD: usize = 32;
/// The longest entry of a name the directory keeps as a key: its length in four bytes, `u`,
/// 32 hexadecimal digits.
const ENTRY_MOST: usize = 37;
/// A byte's two hexadecimal digits.
static HEX: [[u8; 2]; 256] = {
    let mut table = [[0u8; 2]; 256];
    let mut byte = 0;
    while byte < 256 {
        table[byte] = [b"0123456789abcdef"[byte >> 4], b"0123456789abcdef"[byte & 15]];
        byte += 1;
    }
    table
};

/// Stages 1 and 2. `None` when the directory was too small for the input's users: the
/// caller starts over with a larger `grow`. `whole_clicks`: `ingest::opaque::click_id_at`.
pub fn stages(ctx: &Ctx, ingest: &Ingest, grow: u32, whole_clicks: bool, parallel_alloc: bool, scatter_mode: Option<usize>, lap: &mut dyn FnMut(&'static str)) -> Option<Staged> {
    let mut plan = ingest.opaque_plan(grow, whole_clicks);
    // `SS_BUCKETS=dense|hash`: the person buckets as ranges of the persons' numbers, for
    // `sessions::bucket_dense`, or by the persons' hashes, for `sessions::bucket`.
    let dense_buckets = types::env::pick("SS_BUCKETS", &["dense", "hash"]) == 0;
    // `SS_CLICKS=placed|listed|wait`: a click chunk taken once the links are joined writes
    // its clicks to their persons' buckets (`ingest::opaque::Placing`); `listed` writes
    // every click to its worker's list for the next stage to place, as before; `wait` has
    // every click chunk wait for the links, so that all are placed (for tests). Buckets
    // by hash are filled by the next stage only.
    let click_mode = types::env::pick("SS_CLICKS", &["placed", "listed", "wait"]);
    // (A placed click names its campaign in 16 bits, `ClickN`.)
    let place_clicks = dense_buckets && click_mode != 1 && ctx.campaigns.len() <= ClickN::CAMPAIGNS;
    plan.wait_for_placing = place_clicks && click_mode == 2;
    let plan = plan;
    // Each worker makes its lists, and before that puts its piece of the directory onto
    // whole 2 MB pages (`types::mem::touch`; `SS_TOUCH=off` leaves that to the first
    // lookups). All of it before the first lookup: a piece touched while another worker
    // was already looking users up was read first where that worker had been.
    let touch = types::env::pick("SS_TOUCH", &["on", "off"]) == 0;
    let mut parsed: Vec<Out> = par::init(ctx.workers, parallel_alloc, |worker| {
        if touch {
            plan.users.touch(worker, ctx.workers);
        }
        ingest.opaque_out(&plan, ctx.workers)
    });
    types::tl::mark("lists");
    // The links' chunks first, then one task that joins them into persons once they are
    // all read, beside the tasks that parse the events and the clicks. (Read and joined by
    // one task, the links took longer than everything else in the round: 345 ms of one
    // thread at 20M events, against 150 for the parse.)
    let link_tasks = ingest.link_tasks();
    let links: Vec<OnceLock<Vec<LinkRec>>> = (0..link_tasks).map(|_| OnceLock::new()).collect();
    let persons = OnceLock::new();
    par::run(&mut parsed, link_tasks + 1 + ingest.tasks(), |_, out, task| {
        if task < link_tasks {
            drop(links[task].set(ingest.opaque_links(&plan, task, out)));
        } else if task == link_tasks {
            // The links' tasks were taken before this one; the last of them are still read.
            while links.iter().any(|read| read.get().is_none()) {
                std::hint::spin_loop();
            }
            types::tl::mark("links read");
            // The lists as they are: copied into one `Vec` each they were 21 MB at 20M events
            // and 64 MB at 60M, written to fresh pages by the one task the persons wait for.
            let read: Vec<&[LinkRec]> = links.iter().map(|read| read.get().map_or(&[][..], Vec::as_slice)).collect();
            drop(persons.set(Persons::new(&plan.users, &read)));
            types::tl::mark("links named");
            if place_clicks && let Some(persons) = persons.get() {
                // The buckets by the users numbered so far: the linked ones and those of
                // the lines read until now. The rest only add buckets of the same width
                // (there are at most twice as many as `SS_BUCKET_BITS` asks for). A bucket
                // is no wider than a placed click can name a person of.
                let count = plan.users.count().max(1);
                let shift = (usize::BITS - (count - 1).leading_zeros()).saturating_sub(ctx.bucket_bits).min(ClickN::SPAN_BITS);
                // SAFETY: the array has an entry a number of the directory, the unions are
                // done, and it lives in `Names` to the end of the run.
                drop(plan.placing.set(unsafe { Placing::new(persons.up.as_ptr(), shift, ((plan.users.bound() - 1) >> shift) + 1) }));
            }
        } else {
            ingest.opaque_parse(ctx, &plan, task - link_tasks - 1, out);
        }
    });
    if plan.users.overflowed() {
        return None;
    }
    let persons = persons.into_inner().expect("task 0 ran");
    lap("parse");

    let (parts, long_parts, buckets) = (1usize << plan.part_bits, 1usize << plan.long_bits, 1usize << ctx.bucket_bits);
    // (A wider row is two of the narrow lists' records, and a pad is none.)
    let events: usize = parsed.iter().map(|out| out.narrow.len() - out.narrow.extra() + out.events.len() + out.long_events.len()).sum();
    // A worker's `user_daily` rows get their room once (`types::rows::expect`): generated
    // data has a row for every 8 or 9 events, so a quarter of an even share of the events
    // is twice a worker's rows.
    types::rows::expect(events / ctx.workers / 4);
    // The clicks still to place, and those the parse placed.
    let clicks: usize = parsed.iter().map(|out| out.clicks.len()).sum();
    let placed: usize = parsed.iter().map(|out| out.placed.as_ref().map_or(0, Chunks::len)).sum();
    let room = |rows: usize| rows / ctx.workers / buckets * 9 / 8 + 64;
    if std::env::var_os("SS_TIMING").is_some() {
        let long: usize = parsed.iter().map(|out| out.long_events.len()).sum();
        let links: usize = links.iter().filter_map(OnceLock::get).map(Vec::len).sum();
        let slow: usize = parsed.iter().map(|out| out.slow).sum();
        eprintln!("opaque: {events} event rows ({long} with a long event_id), {} clicks ({placed} placed by the parse), {links} links, {} lists, {} users, {slow} lines through the row code", clicks + placed, parts + long_parts, plan.users.count());
    }
    // The buckets as the parse placed its clicks, if it did.
    let dense = dense_buckets.then(|| {
        let count = plan.users.count().max(1);
        let shift = plan.placing.get().map_or((usize::BITS - (count - 1).leading_zeros()).saturating_sub(ctx.bucket_bits), |placing| placing.shift);
        (shift, ((count - 1) >> shift) + 1)
    });
    let mut scattered: Vec<Scattered> = par::init(ctx.workers, parallel_alloc, |_| match dense {
        Some((_, lists)) => Scattered {
            events: Parts::new(1, 64, scatter_mode),
            clicks: Parts::new(1, 64, scatter_mode),
            scratch: Default::default(),
            opaque: Default::default(),
            dense: Some(DenseBuckets::new(lists, events, clicks, ctx.workers)),
            placed: None,
        },
        None => Scattered {
            events: Parts::new(buckets, room(events), scatter_mode),
            clicks: Parts::new(buckets, room(clicks), scatter_mode),
            scratch: Default::default(),
            opaque: Default::default(),
            dense: None,
            placed: None,
        },
    });
    // The clicks the parse placed stay with the buckets: the sessions stage reads them.
    for (mine, out) in scattered.iter_mut().zip(parsed.iter_mut()) {
        mine.placed = out.placed.take();
    }
    // `SS_RECYCLE=off|on`. `on`: the buckets' lists written into the chunks of the parse's
    // lists as the dedup has read them (`Chunks::give`), for buckets that are ranges of
    // numbers. `off`, the pick: into fresh memory, and the parse's lists handed back to the
    // kernel after the stage. On 4 KB pages here `on` is 4% faster; on the scorer it was not
    // (0.6122 s against 0.6076 without, one submission each). The likely reason: a list is
    // written with streaming stores, and a chunk the task has just read is in its caches,
    // which an Intel core has to empty of the line first.
    let recycle = dense.is_some() && types::env::pick("SS_RECYCLE", &["off", "on"]) == 1;
    {
        let (parsed, persons) = (&parsed[..], &persons);
        // The clicks' tasks are taken first: each is a worker's whole list, as long as
        // hundreds of the lists' tasks, and taken last they were what the round ended on,
        // one thread at the longest while seven had nothing left (up to 7 ms of 120 at 20M
        // events).
        let workers = parsed.len();
        par::run(&mut scattered, workers + parts + long_parts, |_, mine, task| {
            let task = if task < workers { parts + long_parts + task } else { task - workers };
            let Scattered { events, clicks, opaque: Scratch { table, kept, kept_narrow, kept_long }, dense: ranges, .. } = mine;
            if let (Some(ranges), Some((shift, _))) = (ranges, dense) {
                if task < parts {
                    dedup_short(parsed, task, &plan, table, kept_narrow, kept);
                    if recycle {
                        give_chunks(parsed, |out| &out.narrow, task, ranges);
                    }
                    scatter_dense(kept_narrow, persons, shift, ranges);
                    scatter_dense(kept, persons, shift, ranges);
                } else if task < parts + long_parts {
                    dedup(parsed.iter().map(|out| &out.long_events), task - parts, plan.long_bits, table, kept_long);
                    if recycle {
                        give_chunks(parsed, |out| &out.long_events, task - parts, ranges);
                    }
                    scatter_dense(kept_long, persons, shift, ranges);
                } else {
                    scatter_clicks_dense(&parsed[task - parts - long_parts], persons, shift, recycle, ranges);
                }
            } else if task < parts {
                dedup_short(parsed, task, &plan, table, kept_narrow, kept);
                if events.combining() {
                    scatter::<_, Combine>(kept_narrow, persons, ctx.bucket_bits, events);
                    scatter::<_, Combine>(kept, persons, ctx.bucket_bits, events);
                } else {
                    scatter::<_, Plain>(kept_narrow, persons, ctx.bucket_bits, events);
                    scatter::<_, Plain>(kept, persons, ctx.bucket_bits, events);
                }
            } else if task < parts + long_parts {
                dedup(parsed.iter().map(|out| &out.long_events), task - parts, plan.long_bits, table, kept_long);
                if events.combining() { scatter::<_, Combine>(kept_long, persons, ctx.bucket_bits, events) } else { scatter::<_, Plain>(kept_long, persons, ctx.bucket_bits, events) }
            } else {
                let from = &parsed[task - parts - long_parts];
                if clicks.combining() { scatter_clicks::<Combine>(from, persons, ctx.bucket_bits, clicks) } else { scatter_clicks::<Plain>(from, persons, ctx.bucket_bits, clicks) }
            }
        });
    }
    lap("dedup");
    // The parse's lists are dead. Where their chunks went to the buckets' lists they stay,
    // with those, to the end of the run; else each worker hands its own back now.
    if types::mem::free_early() && !recycle {
        par::each(&mut parsed, |_, out| out.release());
    }
    // The exit path skips destructors (`fast-exit`), and these are large.
    std::mem::forget(parsed);
    Some(Staged { scattered, names: Names { plan, persons }, dense })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Every combination of the switches.
    fn ways() -> Vec<Way> {
        let mut all = Vec::new();
        for tag in [false, true] {
            for room in [1, 2, 4] {
                for read in [0, 3, 200] {
                    for ahead in [0, 8] {
                        all.push(Way { ahead, room, read, tag });
                    }
                }
            }
        }
        all
    }

    /// `rows` dealt to `workers` lists in turn, deduplicated every way: each the same as the
    /// rows' first copy of each key, with the smallest `ingest`, in order of first copy.
    fn check<R: Row + PartialEq + std::fmt::Debug>(rows: &[R], key: impl Fn(&R) -> (u64, u64)) {
        for workers in [1, 3] {
            let mut lists: Vec<Chunks<R>> = (0..workers).map(|_| Chunks::new(1, 1 << 16)).collect();
            for (n, row) in rows.iter().enumerate() {
                lists[n % workers].push(0, *row);
            }
            let (mut want, mut at) = (Vec::new(), HashMap::new());
            for chunks in &lists {
                chunks.for_each(0, |piece| {
                    for row in piece {
                        match at.get(&key(row)) {
                            None => {
                                at.insert(key(row), want.len());
                                want.push(*row);
                            }
                            Some(&k) => {
                                let first: &mut R = &mut want[k];
                                if (row.words().1 as u32) < (first.words().1 as u32) {
                                    *first = *row;
                                }
                            }
                        }
                    }
                });
            }
            let (mut table, mut kept) = (Vec::new(), Vec::new());
            for way in ways() {
                // The table and the rows kept as they were left by the list before.
                dedup_as(way, lists.iter(), 0, 0, &mut table, &mut kept);
                assert_eq!(kept, want, "{} rows over {workers} workers, room {}, read {}, ahead {}, tag {}", rows.len(), way.room, way.read, way.ahead, way.tag);
            }
        }
    }

    /// A row of `key` whose `ingest` is `ingest`, told apart from the key's other copies by `n`.
    fn ev(key: u64, ingest: u32, n: u64) -> Ev {
        Ev { key, user_ts: n << 32 | 7, ingest_pay: u64::from(ingest) | n << 40 }
    }

    /// A key whose `hash` is `hash`.
    fn key_of(hash: u64) -> u64 {
        // The inverse of the odd constant modulo 2^64, by Newton's iteration.
        let mut inverse = types::hash::C2;
        for _ in 0..6 {
            inverse = inverse.wrapping_mul(2u64.wrapping_sub(types::hash::C2.wrapping_mul(inverse)));
        }
        assert_eq!(inverse.wrapping_mul(types::hash::C2), 1);
        hash.wrapping_mul(inverse)
    }

    #[test]
    fn every_way_keeps_the_first_copy_with_the_smallest_ingest() {
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            state >> 16
        };
        for (rows, keys) in [(1usize, 1u64), (2, 1), (300, 40), (5_000, 3_000), (50_000, 45_000), (50_000, 900)] {
            let picks: Vec<u64> = (0..keys).map(|_| next()).collect();
            let short: Vec<Ev> = (0..rows as u64).map(|n| ev(picks[(next() % keys) as usize], (next() % 50) as u32, n)).collect();
            check(&short, |row| (row.key, 0));
            let long: Vec<EvL> = short.iter().map(|row| EvL { hi: row.key ^ 0x5555, lo: !row.key, user_ts: row.user_ts, ingest_pay: row.ingest_pay }).collect();
            check(&long, |row| (row.hi, row.lo));
        }
    }

    /// Keys made to share a place, and a place and a tag: runs of taken slots from a place
    /// (past the table's last slot to its first), and the tagged slot of another key.
    #[test]
    fn crowded_places_and_shared_tags() {
        // At most 256 rows a list: 1,024 slots at every room, a place the top 10 bits of the
        // hash, the tag the 24 bits below them.
        let mut rows = Vec::new();
        for copy in 0..3u64 {
            for j in 0..40u64 {
                // Place 1023, the last; tags apart.
                rows.push(ev(key_of(0x3FF << 54 | j << 31), (40 - j as u32 + copy as u32 * 7) % 11, copy * 100 + j));
                // Place 5, one tag: keys apart only below it.
                rows.push(ev(key_of(5 << 54 | 9 << 31 | j), (j as u32 * 3 + copy as u32) % 5, copy * 100 + j));
            }
            // Place 0, which the probes from place 1023 run on into; a tag of 0.
            rows.push(ev(key_of(77), 9 - copy as u32, copy));
        }
        check(&rows, |row| (row.key, 0));
    }
}
