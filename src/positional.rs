//! Slot `pipe-positional`: the pipeline for data that carries the generator's numbering.
//!
//! The generator numbers events `0..` in (person, time) order and a user ID is
//! `person << 9 | alias`, both scrambled under keys `Ingest::pos_plan` finds at run time
//! (`types::scramble`). Once every event row sits at its number in one table, the table is
//! deduplicated (a slot keeps the copy with the smallest `ingest`), grouped by person and in
//! time order, with no hash table, no link lookup per row and no sort:
//!
//! 1. Parse (`ingest::pos`): a 16-byte record per event line into the list of its number's
//!    partition; a click at its own number in the click table (the clicks are numbered a
//!    person's one after another, under a third key); the links beside it.
//! 2. Clicks: one scan of the click table finds each person's stretch of it (`ClickIndex`).
//! 3. Place: each partition writes its rows into its stretch of the table and lists the
//!    stretches of one person ("runs").
//! 4. Sessions: a person's run goes straight through sessions, credit and counters.
//!
//! Nothing is taken on trust. A person takes the fast lane only when what was read shows it is
//! one: a single run, times in order, every ID it uses in one link component that holds no
//! other person's IDs (`Links`), no row of it outside the numbering. Everyone else (people
//! merged by a shared device or split by a lost link, IDs spelled another way, numbers past the
//! arrays) goes through the classic code, `sessions::bucket`, with the person the links give.
//! So a wrong key, or data that is not the generator's, costs time and never a row.

use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};

use ingest::Ingest;
use ingest::pos::{ClickTable, Link, PosOut, PosPlan};
use sessions::{Agg, Click};
use types::hash::person_hash;
use types::par;
use types::pos::{ExtraClick, POS_ODD_USER, POS_VALID, PosClick, PosEvent, click_key_id};
use types::record::{ClickRec, PAY_CATEGORY_SHIFT, PAY_PRICE_MASK, PersonEvent, VALID};
use types::rows::NO_CATEGORY;
use types::rows::UserDaily;
use types::scramble::{self, ALIAS_BITS, ALIAS_MASK};
use types::{Ctx, Id};

/// Aliases a person's masks tell apart; a higher alias sends the person down the classic lane.
const MASK_ALIASES: u32 = 64;
/// Classic-lane buckets, as bits.
const SLOW_BITS: u32 = 6;
/// `flags` of a person.
const SEEN: u8 = 1;
/// More than one run, a run out of time order, an alias over the masks, or events outside
/// the numbering and no run to add them to: the classic lane.
const UNFIT: u8 = 2;
/// Has events outside the numbering (`Extra`).
const EXTRA: u8 = 4;
/// Has clicks outside the click table (`ClickIndex::extras`).
const XCLICK: u8 = 8;
/// Persons a task looks at for the classic lane's clicks.
const CLICK_TASK_PERSONS: usize = 1 << 12;

/// A surviving event whose `event_id` is outside the numbering and whose user is a number:
/// it joins its person's table events before the sessions are cut.
#[derive(Clone, Copy)]
struct Extra {
    person: u32,
    u: u32,
    ts: u32,
    pay: u32,
}

/// A raw pointer tasks share to write disjoint stretches of one array.
struct Shared<T>(*mut T);
impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Shared<T> {}
// SAFETY: every use writes a stretch no other task touches (stated at each use).
unsafe impl<T: Send> Send for Shared<T> {}
unsafe impl<T: Send> Sync for Shared<T> {}

/// What the links say about each person number.
struct Links {
    /// `NONE`: no link names any of its IDs. `ONE`: the links among its IDs join them into one
    /// component and no link ties any of them to anything else. `MIXED`: everything else.
    state: Vec<u8>,
    /// `ONE`: the aliases in the component.
    mask: Vec<u64>,
    /// `ONE`: the component's name, the smallest of its IDs.
    name: Vec<Id>,
    /// The classic table over every pair that touches a `MIXED` person or an ID that is not a
    /// number: all a classic-lane lookup can need.
    general: links::LinkTable,
    user_key: u64,
}

const NONE: u8 = 0;
const ONE: u8 = 1;
const MIXED: u8 = 2;

/// A link between two aliases of one person.
#[derive(Clone, Copy)]
struct Inner {
    person: u32,
    /// The two aliases, a bit each.
    aliases: u64,
    ids: [Id; 2],
}

impl Links {
    fn build(pairs: &[Link], plan: &PosPlan) -> Links {
        let persons = plan.persons as usize;
        let number = |scrambled: u64| {
            (scrambled != Link::ODD)
                .then(|| scramble::number(scrambled, plan.user_key))
                .filter(|&number| number >> ALIAS_BITS < persons as u64)
                .map(|number| number as u32)
        };
        let mut state = vec![NONE; persons];
        let (mut mask, mut name) = (vec![0u64; persons], vec![0 as Id; persons]);
        let mut inner: Vec<Inner> = Vec::with_capacity(pairs.len());
        let mut general: Vec<[Id; 2]> = Vec::new();
        // A person's mask grows from its first link by every link that touches it. A link that
        // touches none of it yet is set apart: a later one may join the two.
        let mut apart: Vec<u32> = Vec::new();
        for link in pairs {
            let pair = link.ids;
            let [x, y] = link.scrambled.map(number);
            match (x, y) {
                (Some(x), Some(y))
                    if x >> ALIAS_BITS == y >> ALIAS_BITS && (x & ALIAS_MASK) < MASK_ALIASES && (y & ALIAS_MASK) < MASK_ALIASES =>
                {
                    let person = (x >> ALIAS_BITS) as usize;
                    let aliases = 1u64 << (x & ALIAS_MASK) | 1u64 << (y & ALIAS_MASK);
                    if mask[person] == 0 || mask[person] & aliases != 0 {
                        mask[person] |= aliases;
                    } else {
                        apart.push(inner.len() as u32);
                    }
                    // The component's name, if it is one: the smallest of the IDs linked.
                    for id in pair {
                        if name[person] == 0 || types::id::less(id, name[person]) {
                            name[person] = id;
                        }
                    }
                    inner.push(Inner { person: person as u32, aliases, ids: pair });
                }
                _ => {
                    for side in [x, y].into_iter().flatten() {
                        state[(side >> ALIAS_BITS) as usize] = MIXED;
                    }
                    general.push(pair);
                }
            }
        }
        // Until none joins: what then stays apart is a second component of its person.
        loop {
            let before = apart.len();
            apart.retain(|&at| {
                let link = &inner[at as usize];
                let joined = mask[link.person as usize] & link.aliases != 0;
                if joined {
                    mask[link.person as usize] |= link.aliases;
                }
                !joined
            });
            if apart.len() == before {
                break;
            }
        }
        for &at in &apart {
            state[inner[at as usize].person as usize] = MIXED;
        }
        // A person whose links are one component and tie it to nothing else is `ONE`; the
        // links of every other linked person go to the general table.
        for link in &inner {
            let person = link.person as usize;
            if state[person] == MIXED {
                general.push(link.ids);
            } else {
                state[person] = ONE;
            }
        }
        Links { state, mask, name, general: links::build(&general), user_key: plan.user_key }
    }

    /// The person a user number belongs to, by the rules: the name of its link component.
    fn person(&self, u: u32) -> Id {
        let (person, alias) = ((u >> ALIAS_BITS) as usize, u & ALIAS_MASK);
        let own = || scramble::id_of(u64::from(u), self.user_key, b'u');
        match self.state[person] {
            ONE if alias < MASK_ALIASES && self.mask[person] >> alias & 1 == 1 => self.name[person],
            MIXED => self.general.person(own()),
            _ => own(),
        }
    }
}

/// A stretch of the event table whose events are one person's, with no other person's between.
#[derive(Clone, Copy)]
struct Run {
    person: u32,
    /// Event numbers `first..end`; both ends are events.
    first: u32,
    end: u32,
    first_ts: u32,
    last_ts: u32,
    /// The aliases its rows use (an alias over the masks sets `unfit`).
    aliases: u64,
    /// Out of time order, or an alias over the masks.
    unfit: bool,
}

/// The run being read while a partition's slots are scanned: `Run` with the positions still
/// slots of the partition, and `NO_PERSON` before the first event.
struct Open {
    person: u32,
    first: u32,
    end: u32,
    first_ts: u32,
    last_ts: u32,
    aliases: u64,
    unfit: bool,
}

/// No user number has this person: a person is below `PosPlan::persons`.
const NO_PERSON: u32 = u32::MAX;

impl Open {
    fn none() -> Open {
        Open { person: NO_PERSON, first: 0, end: 0, first_ts: 0, last_ts: 0, aliases: 0, unfit: false }
    }

    /// The next event in slot order: it continues the run, or ends it and starts its
    /// person's.
    #[inline(always)]
    fn event(&mut self, slot: usize, ts: u32, u: u32, base: usize, runs: &mut Vec<Run>) {
        let person = u >> ALIAS_BITS;
        if person != self.person {
            self.close(base, runs);
            *self = Open { person, first: slot as u32, end: slot as u32, first_ts: ts, last_ts: ts, aliases: 0, unfit: false };
        }
        let alias = u & ALIAS_MASK;
        self.unfit |= (ts < self.last_ts) | (alias >= MASK_ALIASES);
        self.aliases |= 1 << (alias & (MASK_ALIASES - 1));
        (self.last_ts, self.end) = (ts, slot as u32 + 1);
    }

    /// The run, if one is open, as event numbers.
    #[inline(always)]
    fn close(&self, base: usize, runs: &mut Vec<Run>) {
        if self.person != NO_PERSON {
            runs.push(Run {
                person: self.person,
                first: base as u32 + self.first,
                end: base as u32 + self.end,
                first_ts: self.first_ts,
                last_ts: self.last_ts,
                aliases: self.aliases,
                unfit: self.unfit,
            });
        }
    }
}

/// One event partition placed.
#[derive(Default)]
struct Placed {
    /// The partition's runs in table order. The last may reach into later partitions once
    /// the runs are joined (`run`); the first is then not this partition's (`skip`).
    runs: Vec<Run>,
    /// 1 when the first run continues an earlier partition's last, which holds it whole.
    skip: usize,
    /// `(event number, pay)` of the kept purchases, ascending.
    pays: Vec<(u32, u32)>,
}

impl Placed {
    /// The runs that start in this partition.
    fn own(&self) -> &[Run] {
        &self.runs[self.skip..]
    }
}

/// Where each person's clicks are in the click table (`ingest::pos::ClickTable`), and the few
/// clicks that are not in it.
///
/// The generator numbers a person's clicks one after another, person after person, so the
/// table holds each person's clicks as one stretch and nothing has to be grouped: `scan_clicks`
/// reads the table once, in pieces side by side, notes where each person's stretch begins and
/// ends, and checks that the persons come in ascending order, which is what makes a stretch
/// one person's alone. (Grouped by a counting scatter from lists per range of persons, the
/// clicks were a stage of their own: 14 ms of a 200 ms run at 20M events.) Where the order
/// does not hold (a wrong key, clicks numbered another way), `regroup` sorts the clicks by
/// person into a table of its own, and everything after reads that one.
struct ClickIndex {
    /// Per person: its stretch of the table, first slot and one past the last; `0..0` for
    /// none. A slot inside it is a click of the person or empty.
    stretches: Vec<[u32; 2]>,
    /// The clicks sorted by person, when the click table's are not.
    regrouped: Option<ClickTable>,
    /// The clicks outside the table (`PosOut::extra_clicks`), by person.
    extras: Vec<ExtraClick>,
}

/// How the clicks are found by person, the pick first (`SS_POS_CLICKS`): where the click
/// table has them, or sorted by person whatever the table's order (what `table` falls back
/// to).
const CLICK_ORDERS: &[&str] = &["table", "regroup"];

impl ClickIndex {
    /// The index of `plan.clicks`, once the parse is over. Sets `click_aliases` and the
    /// `UNFIT` and `XCLICK` flags of each person from its clicks.
    fn new(plan: &PosPlan, parsed: &mut [PosOut], works: &mut [Work], click_aliases: &mut [u64], flags: &mut [u8]) -> ClickIndex {
        // Zeroed lazily by the kernel; a piece's task writes its own persons' entries. Room
        // for 3 million entries at least: a block that large is fresh memory, where a smaller
        // one may be a freed block the allocator clears on this thread (0.8 ms at 20M).
        let mut stretches = vec![[0u32; 2]; (plan.persons as usize).max(3 << 20)];
        assert!(plan.clicks.slots() <= u32::MAX as usize, "{} click slots: positions are 32 bits", plan.clicks.slots());
        // Past the last click stored the table is untouched memory: not read.
        let end = parsed.iter().map(|out| out.click_end).max().unwrap_or(0);
        let ordered = AtomicBool::new(CLICK_ORDERS[types::env::pick("SS_POS_CLICKS", CLICK_ORDERS)] == "table");
        if ordered.load(Relaxed) {
            let shared = (Shared(stretches.as_mut_ptr()), Shared(click_aliases.as_mut_ptr()), Shared(flags.as_mut_ptr()));
            // Pieces of some 16,000 slots or more: a few a worker, so none waits long for the
            // one with the whales.
            let pieces = (end >> 14).clamp(1, 64 * works.len());
            par::run(works, pieces, |_, _, piece| {
                // SAFETY: the arrays hold an entry per person, a click's person is below
                // `plan.persons` (the parse stores no other), and a person's entries are
                // written by the one piece its stretch begins in.
                if ordered.load(Relaxed) && !unsafe { scan_clicks(&plan.clicks, end, piece, pieces, shared) } {
                    ordered.store(false, Relaxed);
                }
            });
        }
        let mut regrouped = None;
        if !ordered.load(Relaxed) {
            if std::env::var_os("SS_TIMING").is_some() {
                eprintln!("positional: the clicks are not in person order, sorted by person");
            }
            regrouped = Some(regroup(&plan.clicks, end, &mut stretches, click_aliases, flags));
        }
        let mut extras: Vec<ExtraClick> = Vec::new();
        for out in parsed.iter_mut() {
            extras.append(&mut out.extra_clicks);
        }
        extras.sort_unstable_by_key(|click| click.u >> ALIAS_BITS);
        for click in &extras {
            let (person, alias) = ((click.u >> ALIAS_BITS) as usize, click.u & ALIAS_MASK);
            click_aliases[person] |= 1 << (alias & (MASK_ALIASES - 1));
            flags[person] |= if alias >= MASK_ALIASES { XCLICK | UNFIT } else { XCLICK };
        }
        ClickIndex { stretches, regrouped, extras }
    }

    /// The table a person's stretch is of.
    #[inline(always)]
    fn table<'a>(&'a self, plan: &'a PosPlan) -> &'a ClickTable {
        self.regrouped.as_ref().unwrap_or(&plan.clicks)
    }

    /// The slots of `person`'s clicks; `person` is below `PosPlan::persons`.
    #[inline(always)]
    fn stretch(&self, person: usize) -> std::ops::Range<usize> {
        let [first, end] = self.stretches[person];
        first as usize..end as usize
    }

    /// The clicks of `person` outside the table, for a person flagged `XCLICK`.
    fn extras_of(&self, person: usize) -> &[ExtraClick] {
        let from = self.extras.partition_point(|click| ((click.u >> ALIAS_BITS) as usize) < person);
        let count = self.extras[from..].iter().take_while(|click| (click.u >> ALIAS_BITS) as usize == person).count();
        &self.extras[from..from + count]
    }

    /// A person's clicks as the attribution reads them, gathered into `room`: the clicks of
    /// its stretch, whose `id` is the `click_id`'s key (keys compare as the IDs do), and, for
    /// a person with clicks outside the table, those too, every `id` then the ID itself.
    #[inline(always)]
    fn gather<'r>(&self, plan: &PosPlan, person: usize, flags: u8, room: &'r mut Vec<Click>) -> &'r mut [Click] {
        let (stretch, table) = (self.stretch(person), self.table(plan));
        room.clear();
        room.reserve(stretch.len() + 1);
        let (out, mut count) = (room.as_mut_ptr(), 0);
        for slot in stretch {
            // SAFETY: a stretch is inside the table; every slot is written to the room
            // reserved, and counted when it holds a click.
            unsafe {
                let click = table.get(slot);
                out.add(count).write(Click { ts: click.ts(), campaign: click.campaign(), id: click.key() });
                count += usize::from(click.is_some());
            }
        }
        // SAFETY: `count` clicks were written, within the room reserved.
        unsafe { room.set_len(count) };
        if flags & XCLICK != 0 {
            self.with_extras(person, room);
        }
        &mut room[..]
    }

    #[cold]
    #[inline(never)]
    fn with_extras(&self, person: usize, room: &mut Vec<Click>) {
        for click in room.iter_mut() {
            click.id = click_key_id(click.id);
        }
        room.extend(self.extras_of(person).iter().map(|click| Click { ts: click.ts, campaign: click.campaign, id: click.click }));
    }

    /// Every click of `person`, in the table or not, as the classic lane takes it.
    fn classic(&self, plan: &PosPlan, links: &Links, person: usize, flags: u8, out: &mut Vec<ClickRec>) {
        let table = self.table(plan);
        for slot in self.stretch(person) {
            // SAFETY: a stretch is inside the table.
            let click = unsafe { table.get(slot) };
            if click.is_some() {
                out.push(ClickRec { user: links.person(click.u()), click: click_key_id(click.key()), ts: click.ts(), campaign: click.campaign() });
            }
        }
        if flags & XCLICK != 0 {
            out.extend(self.extras_of(person).iter().map(|click| ClickRec { user: links.person(click.u), click: click.click, ts: click.ts, campaign: click.campaign }));
        }
    }
}

/// Piece `piece` of `pieces` of the scan of the click table's first `slots` slots: for every
/// person whose first click is in the piece, its stretch, the aliases its clicks use (`click_aliases`)
/// and `UNFIT` in `flags` for an alias over the masks. A person's stretch is followed past
/// the piece's end, and the clicks at the piece's start that continue the stretch of the
/// person before it are left to the piece that stretch began in. Returns whether the persons
/// come in ascending order from the last click before the piece on: if they do in every
/// piece, each person's clicks are one stretch of the table.
///
/// # Safety
/// `slots` is at most the table's; the three arrays hold an entry for every person a click of
/// the table names, and no other thread writes the entries of the persons whose first click
/// is in this piece.
unsafe fn scan_clicks(table: &ClickTable, slots: usize, piece: usize, pieces: usize, (stretches, click_aliases, flags): (Shared<[u32; 2]>, Shared<u64>, Shared<u8>)) -> bool {
    let (from, to) = (slots * piece / pieces, slots * (piece + 1) / pieces);
    // SAFETY: every slot read is below `slots`; the entries written are this piece's persons'
    // (the caller's contract).
    unsafe {
        // The person of the last click before the piece.
        let mut before = NO_PERSON;
        let mut at = from;
        while at > 0 && before == NO_PERSON {
            at -= 1;
            let click = table.get(at);
            if click.is_some() {
                before = click.u() >> ALIAS_BITS;
            }
        }
        // The person in hand, the aliases of its clicks so far, and its last click's slot.
        let (mut person, mut aliases, mut wide, mut last) = (before, 0u64, false, 0usize);
        // Whether the person in hand is this piece's: not the one before it.
        let mut own = false;
        let close = |person: u32, aliases: u64, wide: bool, last: usize| {
            (*stretches.0.add(person as usize))[1] = last as u32 + 1;
            *click_aliases.0.add(person as usize) |= aliases;
            if wide {
                *flags.0.add(person as usize) |= UNFIT;
            }
        };
        let mut at = from;
        while at < slots {
            let click = table.get(at);
            if click.is_some() {
                let (next, alias) = (click.u() >> ALIAS_BITS, click.u() & ALIAS_MASK);
                if next != person {
                    if own {
                        close(person, aliases, wide, last);
                    }
                    // A stretch that begins past the piece is the next piece's.
                    if at >= to {
                        return true;
                    }
                    if person != NO_PERSON && next < person {
                        return false;
                    }
                    (person, aliases, wide, own) = (next, 0, false, true);
                    (*stretches.0.add(next as usize))[0] = at as u32;
                }
                aliases |= 1 << (alias & (MASK_ALIASES - 1));
                wide |= alias >= MASK_ALIASES;
                last = at;
            }
            at += 1;
        }
        if own {
            close(person, aliases, wide, last);
        }
    }
    true
}

/// The clicks of `table` sorted by person into a table of their own, for clicks that are not
/// in person order: a count, then a scatter. Sets the stretches and the persons' aliases as
/// `scan_clicks` does. `slots`: the slots of `table` that may hold a click.
fn regroup(table: &ClickTable, slots: usize, stretches: &mut [[u32; 2]], click_aliases: &mut [u64], flags: &mut [u8]) -> ClickTable {
    // What an abandoned scan left.
    stretches.fill([0; 2]);
    click_aliases.fill(0);
    // SAFETY: slots of the table.
    let clicks = || (0..slots).map(|slot| unsafe { table.get(slot) }).filter(PosClick::is_some);
    for click in clicks() {
        let (person, alias) = ((click.u() >> ALIAS_BITS) as usize, click.u() & ALIAS_MASK);
        stretches[person][1] += 1;
        click_aliases[person] |= 1 << (alias & (MASK_ALIASES - 1));
        if alias >= MASK_ALIASES {
            flags[person] |= UNFIT;
        }
    }
    // Each person's first slot; the scatter moves `end` from there to one past its last.
    let mut total = 0u32;
    for [first, end] in stretches.iter_mut() {
        (*first, total) = (total, total + *end);
        *end = *first;
    }
    let sorted = ClickTable::new(total as usize);
    for click in clicks() {
        let at = &mut stretches[(click.u() >> ALIAS_BITS) as usize][1];
        let (low, rest) = click.words();
        // SAFETY: the counts above are of these same clicks, so the slots are a permutation
        // of the new table's.
        let stored = unsafe { sorted.put(*at as usize, low, rest) };
        debug_assert!(stored);
        *at += 1;
    }
    sorted
}

/// A worker's buffers for placing a partition.
#[derive(Default)]
struct Placing {
    /// The partition's slots while its rows are placed: the kept copy of each event number.
    kept: Vec<PosEvent>,
    /// Kept-copy candidates whose user is not a number: (slot, ingest, worker, index).
    odd: Vec<(u32, u32, u32, u32)>,
    slow_events: Vec<PersonEvent>,
    /// The slots with a kept purchase, a bit each, and the `pay` of each such slot.
    paid: Vec<u64>,
    pay_at: Vec<u32>,
}

/// In `Cut::s_ord`: the session's first event is also its run's first.
const RUN: u32 = 1 << 31;
/// In `Cut::s_camp`: not a fast-lane session (its run is an edge's, the classic lane's, or
/// has extras), so nothing here counts it.
const SKIP: u32 = u32::MAX;

/// A worker's arrays for a partition cut into sessions (`cut`, `Fused::partition_cut`): what
/// the passes after the scan read instead of the events. Reused for every partition.
#[derive(Default)]
struct Cut {
    /// Per session, in table order: its first event's ordinal among the partition's events
    /// (with `RUN`), that event's `ts` and user, its day, its credited campaign (or `SKIP`),
    /// its revenue and its row.
    s_ord: Vec<u32>,
    s_ts: Vec<u32>,
    s_u: Vec<u32>,
    s_day: Vec<u32>,
    s_camp: Vec<u32>,
    s_row: Vec<u32>,
    s_rev: Vec<u64>,
    /// Per 16 slots: the sessions before them, and the slots where one starts, a bit each.
    blk_ns: Vec<u32>,
    starts: Vec<u16>,
    /// Per run, beside `Work::runs`: its first session; then the number of sessions.
    run_s0: Vec<u32>,
    /// Per `user_daily` row (a run's sessions of one day): the day, the first session, the
    /// first event's ordinal, the revenue and the top category.
    row_day: Vec<u32>,
    row_s0: Vec<u32>,
    row_e0: Vec<u32>,
    row_rev: Vec<u64>,
    row_cat: Vec<u8>,
    /// The partition's fast-lane runs: the person's name and its sessions.
    fit: Vec<(Id, u32, u32)>,
    /// The fast-lane sessions with revenue.
    paid: Vec<u32>,
    /// Per category: the row its purchases were last summed for, and that sum.
    cat_row: Vec<u32>,
    cat_sum: Vec<u64>,
}

/// How a placed partition becomes sessions, the pick first (`SS_POS_CUT`): one vector pass
/// over the slots that emits a record per session (`cut`), or two columns and then a walk of
/// every slot of every run (`scan`, `sessions::positional::person`).
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512dq",
    target_feature = "avx512vl",
    target_feature = "bmi2"
))]
const CUTS: &[&str] = &["vector", "slots"];
#[cfg(not(all(
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512dq",
    target_feature = "avx512vl",
    target_feature = "bmi2"
)))]
const CUTS: &[&str] = &["slots"];

/// A worker's state for the stages after the parse.
struct Work {
    agg: Agg,
    cut: Cut,
    slow_events: Vec<PersonEvent>,
    slow_clicks: Vec<ClickRec>,
    /// The clicks of the person in hand (`ClickIndex::gather`).
    clicks: Vec<Click>,
    placing: Placing,
    /// The worker's classic-lane events and clicks, by bucket.
    lane: (Vec<Vec<PersonEvent>>, Vec<Vec<ClickRec>>),
    /// The fused stages' partition, once placed: its two columns (`scan`) and its runs.
    times: Vec<u32>,
    users: Vec<u32>,
    runs: Vec<Run>,
    /// The fused stages' count of runs taken and of those sent to the classic lane, then the
    /// latter's slots by reason (`Fused::take`).
    stats: [u64; 6],
    /// Where the fused stages' time goes (`SS_TIMING=2`): `PHASES`.
    phases: types::tl::Phases,
}

/// The phases of a partition's task, as `Work::phases` counts them.
const PHASES: [&str; 8] = ["place", "cut", "days and rows", "runs and credit", "purchases", "counters", "rows out", "edges"];

impl Work {
    /// `rows`: the `user_daily` rows to make room for. The six columns grow together, and
    /// doubling them a row at a time meant some 800 reallocations a run on the workers'
    /// threads, each a copy into fresh pages; reserved, a page is touched when a row lands.
    fn new(ctx: &Ctx, rows: usize) -> Work {
        let mut agg = Agg::new(ctx);
        agg.user_daily.reserve(rows);
        Work {
            agg,
            cut: Cut::default(),
            slow_events: Vec::new(),
            slow_clicks: Vec::new(),
            clicks: Vec::new(),
            placing: Placing::default(),
            lane: Default::default(),
            times: Vec::new(),
            users: Vec::new(),
            runs: Vec::new(),
            stats: [0; 6],
            phases: types::tl::Phases::new(),
        }
    }
}

/// How the stages after the parse run, the pick first (`SS_POS_STAGES`): a partition placed and
/// its sessions cut in one task (`fused_stages`), or the table placed whole and then read
/// (`split_stages`), which is also what `fused` falls back to.
const STAGES: &[&str] = &["fused", "split"];

/// Runs the whole pipeline after the inputs stage. `lap` marks a stage for `SS_TIMING`.
pub fn run(
    ctx: &Ctx,
    ingest: &Ingest,
    plan: &PosPlan,
    mut parsed: Vec<PosOut>,
    out: &Path,
    parallel_alloc: bool,
    lap: &mut dyn FnMut(&'static str),
) -> Result<(), String> {
    let free_early = types::mem::free_early();

    // Stage 1. Task 0 reads the links; the others parse, each worker into the lists the
    // inputs stage built for it (`ingest::open`).
    let links = OnceLock::new();
    par::run(&mut parsed, ingest.tasks() + 1, |_, out, task| match task {
        0 => {
            let pairs = ingest.pos_links();
            types::tl::mark("links read");
            drop(links.set(Links::build(&pairs, plan)));
            types::tl::mark("links built");
        }
        _ => ingest.pos_parse(ctx, plan, task - 1, out),
    });
    let links = links.get().expect("task 0 ran");
    lap("parse");

    // The rows outside the numbering, deduplicated among themselves (every copy of an
    // `event_id` is among them or none is). A survivor whose user is a number joins that
    // person's events (`Extra`); any other is a classic-lane event.
    let mut slow_events: Vec<PersonEvent> = Vec::new();
    let mut extras: Vec<Extra> = Vec::new();
    let odd_rows: usize = parsed.iter().map(|out| out.odd_events.len()).sum();
    if odd_rows > 0 {
        let mut scratch = dedup::Scratch::default();
        for kept in dedup::partition(parsed.iter().map(|out| &out.odd_events[..]), 0, &mut scratch) {
            if kept.flags & VALID != 0 {
                let number = scramble::canonical_packed(kept.user, b'u')
                    .map(|scrambled| scramble::number(scrambled, plan.user_key))
                    .filter(|&number| number >> ALIAS_BITS < u64::from(plan.persons));
                match number {
                    Some(number) => extras.push(Extra { person: (number >> ALIAS_BITS) as u32, u: number as u32, ts: kept.ts, pay: kept.pay }),
                    None => slow_events.push(PersonEvent { person: links.general.person(kept.user), ts: kept.ts, pay: kept.pay }),
                }
            }
        }
    }
    extras.sort_unstable_by_key(|extra| extra.person);
    types::tl::mark("extras");

    // Stage 2. Where each person's clicks are in the click table: a person's sessions need
    // its clicks.
    let persons = plan.persons as usize;
    let mut click_aliases = vec![0u64; persons];
    let mut flags = vec![0u8; persons];
    for extra in &extras {
        flags[extra.person as usize] |= EXTRA;
    }
    // A person-day with a session is a row: at 20M one for 9 events. Room for one per 5.
    let rows = plan.events() / ctx.workers / 5 + 1024;
    let mut works: Vec<Work> = par::init(ctx.workers, parallel_alloc, |_| Work::new(ctx, rows));
    types::tl::mark("works");
    let index = ClickIndex::new(plan, &mut parsed, &mut works, &mut click_aliases, &mut flags);
    // The clicks whose user is not a number: classic lane, by the general table.
    let mut slow_clicks: Vec<ClickRec> = Vec::new();
    for out in &parsed {
        slow_clicks.extend(out.odd_clicks.iter().map(|click| ClickRec { user: links.general.person(click.user), ..*click }));
    }
    lap("clicks");

    // Stages 3 and 4. Fused where every person turns out to have one run; the split stages
    // otherwise, from the same lists.
    let timing = std::env::var_os("SS_TIMING").is_some();
    let mut stats = None;
    if STAGES[types::env::pick("SS_POS_STAGES", STAGES)] == "fused" {
        let state: Vec<AtomicU8> = (0..persons).map(|_| AtomicU8::new(0)).collect();
        let conflict = AtomicBool::new(false);
        let vector = CUTS[types::env::pick("SS_POS_CUT", CUTS)] == "vector";
        let shared = Fused { ctx, plan, parsed: &parsed, links, extras: &extras, index: &index, vector, click_aliases: &click_aliases, flags: &flags, state: &state, conflict: &conflict };
        if fused_stages(&shared, &mut works, &mut slow_events, lap) {
            let sum = |at: usize| works.iter().map(|work| work.stats[at]).sum::<u64>();
            stats = Some((sum(0) as usize, sum(1) as usize, [sum(2), sum(3), sum(4), sum(5)]));
            if free_early {
                let workers = parsed.len();
                par::each(&mut parsed, |worker, out| {
                    out.release();
                    plan.clicks.release(worker, workers);
                });
            }
        } else {
            // A person with two runs: the first is in some worker's counters already.
            if timing {
                eprintln!("positional: a person's events are not one stretch, the split stages take over");
            }
            works = par::init(ctx.workers, parallel_alloc, |_| Work::new(ctx, rows));
        }
        std::mem::forget(state);
    }
    if stats.is_none() {
        stats = Some(split_stages(ctx, plan, &mut parsed, links, &extras, &index, &click_aliases, &mut flags, &mut works, &mut slow_events, free_early, timing, lap));
    }

    // The classic lane: its events and clicks by person hash into a few buckets, each through
    // the classic sessions code, into the same counters.
    // Each worker puts its own into buckets (one thread doing it for all was 2 ms of a 250 ms
    // run at 20M); what this thread gathered goes with worker 0's.
    works[0].slow_events.append(&mut slow_events);
    works[0].slow_clicks.append(&mut slow_clicks);
    let slow = (works.iter().map(|work| work.slow_events.len()).sum::<usize>(), works.iter().map(|work| work.slow_clicks.len()).sum::<usize>());
    if slow.0 > 0 {
        let bucket_of = |person: Id| (person_hash(person) >> (64 - SLOW_BITS)) as usize;
        par::each(&mut works, |_, work| {
            let (mut events, mut clicks) = (vec![Vec::new(); 1 << SLOW_BITS], vec![Vec::new(); 1 << SLOW_BITS]);
            for event in std::mem::take(&mut work.slow_events) {
                events[bucket_of(event.person)].push(event);
            }
            for click in std::mem::take(&mut work.slow_clicks) {
                clicks[bucket_of(click.user)].push(click);
            }
            work.lane = (events, clicks);
        });
        let lanes: Vec<(Vec<Vec<PersonEvent>>, Vec<Vec<ClickRec>>)> = works.iter_mut().map(|work| std::mem::take(&mut work.lane)).collect();
        par::run(&mut works, 1 << SLOW_BITS, |_, work, bucket| {
            let events = lanes.iter().map(|lane| &lane.0[bucket][..]);
            if events.clone().any(|piece| !piece.is_empty()) {
                sessions::bucket_stretch(ctx, events, lanes.iter().map(|lane| &lane.1[bucket][..]), &mut work.agg);
            }
        });
        // The exit path skips destructors (`fast-exit`).
        std::mem::forget(lanes);
    }
    lap("classic");

    let (user_folder, campaign_folder) = (out.join("user_daily"), out.join("campaign_daily"));
    for folder in [&user_folder, &campaign_folder] {
        std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    }
    types::tl::mark("folders");
    // Each worker writes its `user_daily` file, then a part of `campaign_daily`: an even share
    // of the cells, summed over every worker's counters. (Summed and written whole by a thread
    // of its own, through arrow-rs, that table was 2 ms of a run of 21 at 1M events: a thread
    // to start, 4.5 MB of counters read cold, a megabyte of sums on fresh pages.)
    let mut phase_ns = [0u64; 12];
    for work in &works {
        for (sum, ns) in phase_ns.iter_mut().zip(work.phases.ns) {
            *sum += ns;
        }
    }
    let mut tables: Vec<UserDaily> = works.iter_mut().map(|work| std::mem::take(&mut work.agg.user_daily)).collect();
    let aggs: Vec<Agg> = works.into_iter().map(|work| work.agg).collect();
    let failure = OnceLock::new();
    let parts = tables.len();
    par::each(&mut tables, |worker, table| {
        let written = output::user_daily(&user_folder, worker, ctx, std::mem::take(table))
            .and_then(|()| output::campaign_daily(&campaign_folder, worker, ctx, sessions::campaign_daily_part(ctx, &aggs, worker, parts)));
        if let Err(error) = written {
            drop(failure.set(error));
        }
        types::tl::mark("worker wrote");
    });
    if let Some(error) = failure.into_inner() {
        return Err(error);
    }
    lap("output");
    if types::tl::on() {
        let line: Vec<String> = PHASES.iter().enumerate().map(|(at, name)| format!("{name} {:.1}", phase_ns[at] as f64 / 1e6)).collect();
        eprintln!("fused stages, thread-ms: {}", line.join(", "));
    }
    if timing && let Some((runs, unfit, why)) = stats {
        // Why, in slots: flagged (split, out of order, wide alias, outside the numbering),
        // mixed links, an ID outside the one component, several unlinked IDs.
        eprintln!("unfit slots by reason: flagged {}, mixed links {}, outside the component {}, unlinked IDs {}", why[0], why[1], why[2], why[3]);
        eprintln!(
            "positional: {runs} runs, {unfit} in the classic lane with {} events and {} clicks; {odd_rows} rows outside the numbering",
            slow.0, slow.1
        );
    }
    // The exit path skips destructors (`fast-exit`), and these are large.
    std::mem::forget((index, aggs));
    Ok(())
}

/// What the fused stages' tasks share.
struct Fused<'a> {
    ctx: &'a Ctx,
    plan: &'a PosPlan,
    parsed: &'a [PosOut],
    links: &'a Links,
    /// By person.
    extras: &'a [Extra],
    index: &'a ClickIndex,
    /// Whether a partition is cut by the vector pass (`CUTS`).
    vector: bool,
    click_aliases: &'a [u64],
    /// `UNFIT` for a person with a click alias over the masks (`ClickIndex::new`), `EXTRA`
    /// for one with extras, `XCLICK` for one with clicks outside the click table.
    flags: &'a [u8],
    /// Per person: `SEEN` once a run of it is taken, `UNFIT` once that run is found to be the
    /// classic lane's.
    state: &'a [AtomicU8],
    /// A second run of some person was met. Its first is in a worker's counters by then, so
    /// everything these stages did is void.
    conflict: &'a AtomicBool,
}

/// A partition's first or last run, set aside with its events: it may be a piece of a run that
/// crosses partitions (`fused_stages` joins them).
struct Edge {
    run: Run,
    /// Whether it is its partition's first run, and its last.
    first: bool,
    last: bool,
    /// `(ts, pay, u)` of its events, in table order.
    events: Vec<(u32, u32, u32)>,
}

impl<'a> Fused<'a> {
    fn extras_of(&self, person: usize) -> &'a [Extra] {
        // Nearly nobody has any, and the search was a dozen steps for every person.
        if self.flags[person] & EXTRA == 0 {
            return &[];
        }
        let from = self.extras.partition_point(|extra| (extra.person as usize) < person);
        let count = self.extras[from..].iter().take_while(|extra| extra.person as usize == person).count();
        &self.extras[from..from + count]
    }

    /// Takes `person` for its run, whose rows use `aliases` and which is `unfit` if out of
    /// time order or with an alias over the masks. `None` when a run of the person was taken
    /// before (`conflict`). Otherwise whether it is one fast-lane person (as `fit` in
    /// `split_stages`: nothing disqualifies it, and every ID its rows use is in its one link
    /// component, or it has no links and uses a single ID), and whether it has extras.
    fn take(&self, person: usize, aliases: u64, unfit: bool, slots: u64, stats: &mut [u64; 6]) -> Option<(bool, bool)> {
        if self.state[person].fetch_or(SEEN, Relaxed) & SEEN != 0 {
            self.conflict.store(true, Relaxed);
            return None;
        }
        let own = self.extras_of(person);
        let (mut ids, mut flagged) = (aliases | self.click_aliases[person], unfit | (self.flags[person] & UNFIT != 0));
        for extra in own {
            let alias = extra.u & ALIAS_MASK;
            ids |= 1 << (alias & (MASK_ALIASES - 1));
            flagged |= alias >= MASK_ALIASES;
        }
        let state = self.links.state[person];
        let fit = !flagged
            && match state {
                ONE => ids & !self.links.mask[person] == 0,
                NONE => ids.count_ones() <= 1,
                _ => false,
            };
        stats[0] += 1;
        if !fit {
            self.state[person].fetch_or(UNFIT, Relaxed);
            stats[1] += 1;
            let reason = match state {
                _ if flagged => 0,
                MIXED => 1,
                ONE => 2,
                _ => 3,
            };
            stats[2 + reason] += slots;
        }
        Some((fit, !own.is_empty()))
    }

    /// Whether `person` is a fast-lane person, once every run is taken: for its clicks and its
    /// extras.
    fn fit(&self, person: usize) -> bool {
        let state = self.state[person].load(Relaxed);
        if state & SEEN != 0 {
            return state & UNFIT == 0;
        }
        // No run. With extras, those are its events and the classic lane's; without, only
        // its clicks' IDs are there to judge.
        let ids = self.click_aliases[person];
        self.flags[person] & UNFIT == 0
            && self.extras_of(person).is_empty()
            && match self.links.state[person] {
                ONE => ids & !self.links.mask[person] == 0,
                NONE => ids.count_ones() <= 1,
                _ => false,
            }
    }

    /// A fast-lane person's name: its component's, or the one ID it uses.
    fn name(&self, person: usize, u: u32) -> Id {
        match self.links.state[person] {
            ONE => self.links.name[person],
            _ => scramble::id_of(u64::from(u), self.plan.user_key, b'u'),
        }
    }

    /// A person's clicks, gathered into `room`.
    #[inline(always)]
    fn clicks<'r>(&self, person: usize, room: &'r mut Vec<Click>) -> &'r mut [Click] {
        self.index.gather(self.plan, person, self.flags[person], room)
    }

    /// Places partition `part` in the worker's own slots and, while they are in cache, takes
    /// every run but the first and the last through its sessions. Returns those two.
    fn partition(&self, part: usize, work: &mut Work) -> Vec<Edge> {
        #[cfg(all(
            target_arch = "x86_64",
            target_feature = "avx512f",
            target_feature = "avx512bw",
            target_feature = "avx512dq",
            target_feature = "avx512vl",
            target_feature = "bmi2"
        ))]
        if self.vector {
            return self.partition_cut(part, work);
        }
        self.partition_slots(part, work)
    }

    /// `partition` with the sessions cut by the vector pass. After it no event is looked at
    /// again but the purchases: a session is a record (`Cut`), and the days and rows, the
    /// credit, the purchases and the output are each a pass over records of one kind. (Walked
    /// slot by slot and session by session, each with its day, its purchases, its credit and
    /// its counters in turn, the sessions were a third of the cycles after the parse.)
    #[cfg(all(
        target_arch = "x86_64",
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512dq",
        target_feature = "avx512vl",
        target_feature = "bmi2"
    ))]
    fn partition_cut(&self, part: usize, work: &mut Work) -> Vec<Edge> {
        let Work { agg, slow_events, placing, runs, stats, cut: c, phases, clicks: click_room, .. } = work;
        phases.start();
        let Some(pays) = place_rows(part, self.plan, self.parsed, self.links, placing) else { return Vec::new() };
        phases.lap(0);
        let (ctx, base, kept) = (self.ctx, part << self.plan.shift, &placing.kept[..]);
        runs.clear();
        // SAFETY: the slots are a multiple of 16 (a partition is at least 2^10 numbers).
        let (events, ns) = unsafe { cut(kept, base, c, runs) };
        phases.lap(1);

        // Each session's day, and the rows: a run's sessions of one day.
        let organic = ctx.organic();
        let nrow = days_and_rows(ctx, c, ns, organic);
        (c.row_s0[nrow], c.row_e0[nrow]) = (ns as u32, events);
        c.s_rev[..ns].fill(0);
        c.row_rev[..nrow].fill(0);
        c.row_cat[..nrow].fill(NO_CATEGORY);
        phases.lap(2);

        // The runs: who is a fast-lane person, and the credit of those with clicks.
        let valid = |slot: usize| kept[slot].flags() & POS_VALID != 0;
        // The `pay` of the event in `slot`, 0 for none; the purchases come in slot order.
        let pay_of = |slot: usize, next_pay: &mut usize| match pays.get(*next_pay) {
            Some(&(number, pay)) if number as usize == base + slot => {
                *next_pay += 1;
                pay
            }
            _ => 0,
        };
        c.fit.clear();
        let (mut edges, mut next_pay) = (Vec::new(), 0);
        for (at, run) in runs.iter().enumerate() {
            let person = run.person as usize;
            let (first, end) = (run.first as usize - base, run.end as usize - base);
            let (s0, s1) = (c.run_s0[at] as usize, c.run_s0[at + 1] as usize);
            let edge = at == 0 || at + 1 == runs.len();
            let (fit, extra) = if edge {
                (false, false)
            } else {
                // A second run of the person: everything here is void (`fused_stages`).
                let Some(taken) = self.take(person, run.aliases, run.unfit, (end - first) as u64, stats) else { return Vec::new() };
                taken
            };
            if fit && !extra {
                let name = self.name(person, kept[first].u());
                let clicks = self.clicks(person, click_room);
                if !clicks.is_empty() {
                    agg.attribute(clicks, &c.s_ts[s0..s1], &c.s_day[s0..s1], organic, &mut c.s_camp[s0..s1]);
                }
                c.fit.push((name, s0 as u32, s1 as u32));
                continue;
            }
            // Everyone else leaves with its events, as before the sessions were records.
            c.s_camp[s0..s1].fill(SKIP);
            // Purchases of fast-lane runs passed, and of events that left the table.
            while pays.get(next_pay).is_some_and(|&(number, _)| number < run.first) {
                next_pay += 1;
            }
            let events = (first..end).filter(|&slot| valid(slot)).map(|slot| (kept[slot].ts(), pay_of(slot, &mut next_pay), kept[slot].u()));
            if edge {
                edges.push(Edge { run: *run, first: at == 0, last: at + 1 == runs.len(), events: events.collect() });
            } else if !fit {
                slow_events.extend(events.map(|(ts, pay, u)| PersonEvent { person: self.links.person(u), ts, pay }));
            } else {
                let name = self.name(person, kept[first].u());
                let clicks = self.clicks(person, click_room);
                let all = with_extras(events.map(|(ts, pay, _)| (ts, pay)), self.extras_of(person), end - first);
                sessions::positional::person_pairs(ctx, name, &all, clicks, agg);
            }
        }

        phases.lap(3);
        // The purchases, the only events looked at again: each to its session (the starts at
        // or before its slot, counted) and its row.
        c.cat_row.clear();
        c.cat_row.resize(256, u32::MAX);
        c.cat_sum.resize(256, 0);
        c.paid.clear();
        let (mut row_now, mut best) = (u32::MAX, 0u64);
        for &(number, pay) in &pays {
            let slot = number as usize - base;
            let before = u32::from(c.starts[slot >> 4]) & ((2u32 << (slot & 15)) - 1);
            let session = (c.blk_ns[slot >> 4] + before.count_ones() - 1) as usize;
            if c.s_camp[session] == SKIP {
                continue;
            }
            let (price, category) = (u64::from(pay & PAY_PRICE_MASK), (pay >> PAY_CATEGORY_SHIFT) as usize);
            let row = c.s_row[session];
            if c.s_rev[session] == 0 {
                c.paid.push(session as u32);
            }
            c.s_rev[session] += price;
            c.row_rev[row as usize] += price;
            // The row's top category so far: the largest sum, the smallest index among equals
            // (`agg_dense::Day::flush`). The purchases come row after row.
            let sum = if c.cat_row[category] == row { c.cat_sum[category] } else { 0 } + price;
            (c.cat_row[category], c.cat_sum[category]) = (row, sum);
            if row != row_now {
                (row_now, best) = (row, 0);
            }
            let key = sum << 8 | (255 - category as u64);
            if key > best {
                best = key;
                c.row_cat[row as usize] = category as u8;
            }
        }

        phases.lap(4);
        // The credit of every fast-lane session, then the rows. A session is one more in its
        // campaign's count for its day, the sessions that are not the fast lane's (`SKIP`)
        // in a row no campaign has; revenue is added for the few sessions that have any.
        // (Each session's cell of sessions, purchases and revenue at once was a cache line
        // of a 450 KB table a session, 22 thread-ms at 20M.)
        let trash = ctx.campaigns.len() as u32;
        if let Some(counted) = agg.counted() {
            debug_assert!(counted.len() >= (trash as usize + 1) * 16);
            for s in 0..ns {
                // SAFETY: a campaign is below `trash`, a day below 16 (the table is there
                // only for windows of at most 16 days).
                unsafe { *counted.get_unchecked_mut(c.s_camp.get_unchecked(s).min(&trash).to_owned() as usize * 16 + *c.s_day.get_unchecked(s) as usize) += 1 };
            }
            for &session in &c.paid {
                let session = session as usize;
                agg.paid(c.s_camp[session], c.s_day[session] as usize, c.s_rev[session]);
            }
        } else {
            for &(_, s0, s1) in &c.fit {
                for s in s0 as usize..s1 as usize {
                    agg.credit(c.s_camp[s], c.s_day[s] as usize, c.s_rev[s]);
                }
            }
        }
        phases.lap(5);
        // Room for every row of the partition once, then plain stores: extended run by run,
        // the six columns were 4 million calls for 2.2 million rows.
        let out = &mut agg.user_daily;
        // 16 rows at a time, so up to 15 past the last.
        out.reserve(nrow + 16);
        let mut at = out.person.len();
        debug_assert!([out.day.len(), out.sessions.len(), out.events.len(), out.revenue_cents.len(), out.top_category.len()].iter().all(|&len| len == at));
        let (person, day, sessions, events, revenue, category) =
            (out.person.as_mut_ptr(), out.day.as_mut_ptr(), out.sessions.as_mut_ptr(), out.events.as_mut_ptr(), out.revenue_cents.as_mut_ptr(), out.top_category.as_mut_ptr());
        // The fast-lane runs' rows are stretches of the partition's rows, most of them long
        // (a run that is not the fast lane's ends one): a stretch's five columns of numbers
        // are copied 16 rows a step, and the names filled run by run. (Row by row this was
        // 46 thread-ms at 20M.)
        //
        // SAFETY: the fast-lane runs' rows are among the partition's `nrow`, each written
        // once, inside the room reserved above in every column; a step may write up to 15
        // rows past a stretch, which later stretches overwrite or the lengths leave out, and
        // read as far past it in the rows' arrays, which have that room (`cut`).
        unsafe {
            use core::arch::x86_64::*;
            let day0 = _mm512_set1_epi32(ctx.day0);
            let (row_day, row_s0, row_e0, row_rev, row_cat) = (c.row_day.as_ptr(), c.row_s0.as_ptr(), c.row_e0.as_ptr(), c.row_rev.as_ptr(), c.row_cat.as_ptr());
            let stretch = |from: usize, to: usize, at: usize| {
                let mut row = from;
                while row < to {
                    let o = at + (row - from);
                    _mm512_storeu_si512(day.add(o).cast(), _mm512_add_epi32(_mm512_loadu_si512(row_day.add(row).cast()), day0));
                    let counts = _mm512_sub_epi32(_mm512_loadu_si512(row_s0.add(row + 1).cast()), _mm512_loadu_si512(row_s0.add(row).cast()));
                    _mm512_storeu_si512(sessions.add(o).cast(), _mm512_cvtepu32_epi64(_mm512_castsi512_si256(counts)));
                    _mm512_storeu_si512(sessions.add(o + 8).cast(), _mm512_cvtepu32_epi64(_mm512_extracti64x4_epi64::<1>(counts)));
                    let counts = _mm512_sub_epi32(_mm512_loadu_si512(row_e0.add(row + 1).cast()), _mm512_loadu_si512(row_e0.add(row).cast()));
                    _mm512_storeu_si512(events.add(o).cast(), _mm512_cvtepu32_epi64(_mm512_castsi512_si256(counts)));
                    _mm512_storeu_si512(events.add(o + 8).cast(), _mm512_cvtepu32_epi64(_mm512_extracti64x4_epi64::<1>(counts)));
                    _mm512_storeu_si512(revenue.add(o).cast(), _mm512_loadu_si512(row_rev.add(row).cast()));
                    _mm512_storeu_si512(revenue.add(o + 8).cast(), _mm512_loadu_si512(row_rev.add(row + 8).cast()));
                    _mm_storeu_si128(category.add(o).cast(), _mm_loadu_si128(row_cat.add(row).cast()));
                    row += 16;
                }
            };
            // The stretch being gathered, and where the next name goes.
            let (mut from, mut to, mut named) = (0usize, 0usize, at);
            for &(name, s0, s1) in &c.fit {
                let (r0, r1) = (c.s_row[s0 as usize] as usize, if (s1 as usize) < ns { c.s_row[s1 as usize] as usize } else { nrow });
                if r0 != to {
                    stretch(from, to, at);
                    at += to - from;
                    from = r0;
                }
                to = r1;
                for row in 0..r1 - r0 {
                    person.add(named + row).write(name);
                }
                named += r1 - r0;
            }
            stretch(from, to, at);
            at += to - from;
            debug_assert_eq!(at, named);
        }
        // SAFETY: rows `..at` of every column are written, and the six had one length.
        unsafe {
            out.person.set_len(at);
            out.day.set_len(at);
            out.sessions.set_len(at);
            out.events.set_len(at);
            out.revenue_cents.set_len(at);
            out.top_category.set_len(at);
        }
        phases.lap(6);
        edges
    }

    /// `partition` with the slots read into two columns and every run walked slot by slot.
    fn partition_slots(&self, part: usize, work: &mut Work) -> Vec<Edge> {
        let Work { agg, slow_events, placing, times, users, runs, stats, clicks: click_room, .. } = work;
        let Some(pays) = place_rows(part, self.plan, self.parsed, self.links, placing) else { return Vec::new() };
        let (slots, base) = (1usize << self.plan.shift, part << self.plan.shift);
        // `scan` writes every slot.
        times.resize(slots, 0);
        users.resize(slots, 0);
        runs.clear();
        scan(&placing.kept, times, users, base, runs);
        let (times, users) = (&times[..], &users[..]);
        // The `pay` of the event in `slot`, 0 for none; the purchases come in slot order.
        let pay_of = |slot: usize, next_pay: &mut usize| match pays.get(*next_pay) {
            Some(&(number, pay)) if number as usize == base + slot => {
                *next_pay += 1;
                pay
            }
            _ => 0,
        };
        let (mut edges, mut next_pay) = (Vec::new(), 0);
        for (at, run) in runs.iter().enumerate() {
            let person = run.person as usize;
            let (first, end) = (run.first as usize - base, run.end as usize - base);
            // Purchases of events that left the table (none in practice).
            while pays.get(next_pay).is_some_and(|&(number, _)| number < run.first) {
                next_pay += 1;
            }
            if at == 0 || at + 1 == runs.len() {
                let events = (first..end).filter(|&slot| times[slot] != 0).map(|slot| (times[slot], pay_of(slot, &mut next_pay), users[slot])).collect();
                edges.push(Edge { run: *run, first: at == 0, last: at + 1 == runs.len(), events });
                continue;
            }
            let Some((fit, extra)) = self.take(person, run.aliases, run.unfit, (end - first) as u64, stats) else { break };
            if !fit {
                for slot in first..end {
                    if times[slot] != 0 {
                        let pay = pay_of(slot, &mut next_pay);
                        slow_events.push(PersonEvent { person: self.links.person(users[slot]), ts: times[slot], pay });
                    }
                }
                continue;
            }
            let name = self.name(person, users[first]);
            let clicks = self.clicks(person, click_room);
            if !extra {
                sessions::positional::person(self.ctx, name, &times[first..end], run.first, &pays, &mut next_pay, clicks, agg);
            } else {
                let table = (first..end).filter(|&slot| times[slot] != 0).map(|slot| (times[slot], pay_of(slot, &mut next_pay)));
                let all = with_extras(table, self.extras_of(person), end - first);
                sessions::positional::person_pairs(self.ctx, name, &all, clicks, agg);
            }
        }
        edges
    }

    /// One run from the pieces set aside at partitions' edges.
    fn joined(&self, pieces: &[Edge], work: &mut Work) {
        let Work { agg, slow_events, stats, clicks: click_room, .. } = work;
        let mut run = pieces[0].run;
        for next in &pieces[1..] {
            run.unfit |= next.run.unfit | (next.run.first_ts < run.last_ts);
            (run.last_ts, run.aliases) = (next.run.last_ts, run.aliases | next.run.aliases);
        }
        let person = run.person as usize;
        let slots: u64 = pieces.iter().map(|piece| u64::from(piece.run.end - piece.run.first)).sum();
        let Some((fit, extra)) = self.take(person, run.aliases, run.unfit, slots, stats) else { return };
        let events = pieces.iter().flat_map(|piece| piece.events.iter().copied());
        if !fit {
            slow_events.extend(events.map(|(ts, pay, u)| PersonEvent { person: self.links.person(u), ts, pay }));
            return;
        }
        let name = self.name(person, pieces[0].events[0].2);
        let clicks = self.clicks(person, click_room);
        let table = events.map(|(ts, pay, _)| (ts, pay));
        let all: Vec<(u32, u32)> = if extra { with_extras(table, self.extras_of(person), 0) } else { table.collect() };
        sessions::positional::person_pairs(self.ctx, name, &all, clicks, agg);
    }
}

/// How many of a byte's bits are set at or below each bit: eight counts, a byte each.
static UP_TO: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut byte = 0;
    while byte < 256 {
        let (mut bit, mut count) = (0, 0u64);
        while bit < 8 {
            count += (byte >> bit & 1) as u64;
            table[byte] |= count << (8 * bit);
            bit += 1;
        }
        byte += 1;
    }
    table
};

/// Each session's day (`c.s_day`) and its row (`c.s_row`), the rows themselves (a run's
/// sessions of one day: `c.row_day`, `c.row_s0`, `c.row_e0`), and `organic` as every
/// session's campaign so far. Returns the rows.
///
/// 16 sessions a step: the day is a multiply for a division, a row starts where a run does
/// or the day changes, and the rows' three columns are those lanes packed. (Session by
/// session, with a division and a branch each, this was 35 thread-ms at 20M.)
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512dq",
    target_feature = "avx512vl",
    target_feature = "bmi2"
))]
fn days_and_rows(ctx: &Ctx, c: &mut Cut, ns: usize, organic: u32) -> usize {
    use core::arch::x86_64::*;
    let (origin_day, in_day) = ctx.day_split();
    let mut nrow = 0usize;
    // A `ts` and the origin's time of day are milliseconds that must add up in 32 bits.
    if u64::from(ctx.ts_end) + in_day >= 1 << 32 {
        let mut last_day = u32::MAX;
        for s in 0..ns {
            let day = (ctx.day_of(c.s_ts[s]) - ctx.day0) as u32;
            c.s_day[s] = day;
            if c.s_ord[s] & RUN != 0 || day != last_day {
                (c.row_day[nrow], c.row_s0[nrow], c.row_e0[nrow]) = (day, s as u32, c.s_ord[s] & !RUN);
                nrow += 1;
            }
            c.s_row[s] = nrow as u32 - 1;
            last_day = day;
        }
        c.s_camp[..ns].fill(organic);
        return nrow;
    }
    let (s_ts, s_ord, s_day, s_row, s_camp) = (c.s_ts.as_ptr(), c.s_ord.as_ptr(), c.s_day.as_mut_ptr(), c.s_row.as_mut_ptr(), c.s_camp.as_mut_ptr());
    let (row_day, row_s0, row_e0) = (c.row_day.as_mut_ptr(), c.row_s0.as_mut_ptr(), c.row_e0.as_mut_ptr());
    // SAFETY: every column has room for 16 entries past the partition's sessions (`cut`),
    // and a step reads and writes at most that far.
    unsafe {
        // A day is 2^10 * 84375 ms, and for x below 2^22, x / 84375 is x * 6515625 >> 39.
        let (add, magic) = (_mm512_set1_epi32(in_day as i32), _mm512_set1_epi64(6_515_625));
        // `day_of` less `day0`: the days from the origin's to the window's first.
        let behind = _mm512_set1_epi32((i64::from(ctx.day0) - origin_day) as i32);
        let lanes = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        let (run_flag, organic) = (_mm512_set1_epi32(RUN as i32), _mm512_set1_epi32(organic as i32));
        // The day of the session before, in the last lane.
        let mut before = _mm512_set1_epi32(-1);
        let mut s = 0;
        while s < ns {
            let live = if ns - s >= 16 { 0xFFFF } else { (1u16 << (ns - s)) - 1 };
            let x = _mm512_srli_epi32::<10>(_mm512_add_epi32(_mm512_loadu_si512(s_ts.add(s).cast()), add));
            let even = _mm512_srli_epi64::<39>(_mm512_mul_epu32(x, magic));
            let odd = _mm512_srli_epi64::<39>(_mm512_mul_epu32(_mm512_srli_epi64::<32>(x), magic));
            let day = _mm512_sub_epi32(_mm512_or_si512(even, _mm512_slli_epi64::<32>(odd)), behind);
            _mm512_storeu_si512(s_day.add(s).cast(), day);
            let ord = _mm512_loadu_si512(s_ord.add(s).cast());
            // A row starts with a run (`RUN` is the sign bit) and where the day changes.
            let starts = (_mm512_movepi32_mask(ord) | _mm512_cmpneq_epi32_mask(day, _mm512_alignr_epi32::<15>(day, before))) & live;
            _mm512_storeu_si512(row_day.add(nrow).cast(), _mm512_maskz_compress_epi32(starts, day));
            _mm512_storeu_si512(row_s0.add(nrow).cast(), _mm512_maskz_compress_epi32(starts, _mm512_add_epi32(_mm512_set1_epi32(s as i32), lanes)));
            _mm512_storeu_si512(row_e0.add(nrow).cast(), _mm512_maskz_compress_epi32(starts, _mm512_andnot_si512(run_flag, ord)));
            // A session's row: the rows before the step, and the starts up to its lane.
            let (low, high) = (starts as u8, (starts >> 8) as u8);
            let up_to = _mm512_cvtepu8_epi32(_mm_set_epi64x(UP_TO[high as usize] as i64, UP_TO[low as usize] as i64));
            let row = _mm512_add_epi32(_mm512_set1_epi32(nrow as i32 - 1), up_to);
            _mm512_storeu_si512(s_row.add(s).cast(), _mm512_mask_add_epi32(row, 0xFF00, row, _mm512_set1_epi32(low.count_ones() as i32)));
            _mm512_storeu_si512(s_camp.add(s).cast(), organic);
            nrow += starts.count_ones() as usize;
            before = day;
            s += 16;
        }
    }
    nrow
}

/// A person's table events (in time order) with its extras merged in: a few rows, sorted
/// alone. (Sorting them all together was 1.4% of a run's instructions at 20M: the whales have
/// extras.)
fn with_extras(table: impl Iterator<Item = (u32, u32)>, extras: &[Extra], room: usize) -> Vec<(u32, u32)> {
    let mut own: Vec<(u32, u32)> = extras.iter().map(|extra| (extra.ts, extra.pay)).collect();
    own.sort_unstable_by_key(|&(ts, _)| ts);
    let mut next_own = 0;
    let mut all = Vec::with_capacity(room + own.len());
    for (ts, pay) in table {
        while own.get(next_own).is_some_and(|&(extra_ts, _)| extra_ts <= ts) {
            all.push(own[next_own]);
            next_own += 1;
        }
        all.push((ts, pay));
    }
    all.extend_from_slice(&own[next_own..]);
    all
}

/// Slot `pos-stages-fused`: each partition placed in the worker's own slots and its runs taken
/// straight through their sessions, while the slots are in cache. There is no table of all
/// event numbers (two columns of 4 bytes a number, zeroed by the kernel, written once and read
/// once), no list of all runs and no serial pass over it.
///
/// What a task cannot see is a person's run in another partition. A run that only crosses
/// into the next partition is its partition's last and the next one's first: those two runs
/// of every partition are set aside (`Edge`) and joined afterwards. A person whose events are
/// two stretches with other persons' between is found when its second run is taken; the
/// first is in a worker's counters by then, so this returns `false` with nothing of the
/// caller's changed but `works` and the laps, and the split stages run instead. On generated
/// data a person is one stretch: no seed tried has taken that way out.
fn fused_stages(shared: &Fused, works: &mut Vec<Work>, slow_events: &mut Vec<PersonEvent>, lap: &mut dyn FnMut(&'static str)) -> bool {
    let plan = shared.plan;
    let over = || shared.conflict.load(Relaxed);
    let edges: Vec<OnceLock<Vec<Edge>>> = (0..plan.parts).map(|_| OnceLock::new()).collect();
    par::run(works, plan.parts, |_, work, part| {
        if !over() {
            drop(edges[part].set(shared.partition(part, work)));
        }
    });
    if over() {
        return false;
    }
    lap("sessions");

    // The edge runs in table order; one that is its partition's last joins the next
    // partition's first when they are one person's.
    let edges: Vec<Edge> = edges.into_iter().filter_map(OnceLock::into_inner).flatten().collect();
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut at = 0;
    while at < edges.len() {
        let mut end = at + 1;
        while end < edges.len() && edges[end - 1].last && edges[end].first && edges[end].run.person == edges[at].run.person {
            end += 1;
        }
        groups.push((at, end));
        at = end;
    }
    par::run(works, groups.len(), |_, work, group| {
        if !over() {
            let (from, to) = groups[group];
            shared.joined(&edges[from..to], work);
        }
    });
    if over() {
        return false;
    }
    // The clicks and the extras of the classic lane's persons, now that every run is taken.
    let persons = plan.persons as usize;
    par::run(works, persons.div_ceil(CLICK_TASK_PERSONS), |_, work, task| {
        let index = shared.index;
        for person in task * CLICK_TASK_PERSONS..((task + 1) * CLICK_TASK_PERSONS).min(persons) {
            let flags = shared.flags[person];
            if (!index.stretch(person).is_empty() || flags & XCLICK != 0) && !shared.fit(person) {
                index.classic(plan, shared.links, person, flags, &mut work.slow_clicks);
            }
        }
    });
    for extra in shared.extras {
        if !shared.fit(extra.person as usize) {
            slow_events.push(PersonEvent { person: shared.links.person(extra.u), ts: extra.ts, pay: extra.pay });
        }
    }
    for work in works.iter_mut() {
        slow_events.append(&mut work.placing.slow_events);
    }
    lap("edges");
    true
}

/// The stages after the clicks as three: every partition placed into one table of all event
/// numbers, the runs of the whole table joined and judged on one thread, then the sessions,
/// partition by partition. A person with two runs is simply the classic lane's here, so this
/// is also what `fused_stages` falls back to. Returns the runs, those in the classic lane and
/// their slots by reason.
#[allow(clippy::too_many_arguments)]
fn split_stages(
    ctx: &Ctx,
    plan: &PosPlan,
    parsed: &mut Vec<PosOut>,
    links: &Links,
    extras: &[Extra],
    index: &ClickIndex,
    click_aliases: &[u64],
    flags: &mut [u8],
    works: &mut Vec<Work>,
    slow_events: &mut Vec<PersonEvent>,
    free_early: bool,
    timing: bool,
    lap: &mut dyn FnMut(&'static str),
) -> (usize, usize, [u64; 4]) {
    let (events, persons) = (plan.events(), plan.persons as usize);
    // Zeroed lazily by the kernel: a slot nobody writes reads 0, "no event" (a valid `ts` is
    // at least `event_lo`, 7 days).
    let (mut times, mut users) = (vec![0u32; events], vec![0u32; events]);
    let (times_at, users_at) = (Shared(times.as_mut_ptr()), Shared(users.as_mut_ptr()));
    let placed: Vec<OnceLock<Placed>> = (0..plan.parts).map(|_| OnceLock::new()).collect();
    {
        let parsed = &parsed[..];
        par::run(works, plan.parts, |_, work, task| {
            // SAFETY: partition `task` owns event numbers `task << shift ..`, and only this
            // task writes them.
            if let Some(result) = unsafe { place(task, plan, parsed, links, times_at, users_at, &mut work.placing) } {
                drop(placed[task].set(result));
            }
        });
    }
    for work in works.iter_mut() {
        slow_events.append(&mut work.placing.slow_events);
    }
    if free_early {
        par::each(parsed, |_, out| out.release());
    }
    lap("place");

    // The runs stay with their partitions (copying them all into one list was most of this
    // serial part). A run that crosses partitions is joined into the one it starts in, and
    // skipped where it continues; then what the runs say about each person.
    let mut placed: Vec<Placed> = placed.into_iter().map(|part| part.into_inner().unwrap_or_default()).collect();
    // The partition that holds the table's last run so far.
    let mut open: Option<usize> = None;
    for at in 0..placed.len() {
        let Some(&next) = placed[at].runs.first() else { continue };
        if let Some(from) = open
            && let Some(last) = placed[from].runs.last_mut()
            && last.person == next.person
        {
            last.unfit |= next.unfit | (next.first_ts < last.last_ts);
            (last.end, last.last_ts, last.aliases) = (next.end, next.last_ts, last.aliases | next.aliases);
            placed[at].skip = 1;
            // A partition that is all one run leaves that run open where it began.
            if placed[at].runs.len() == 1 {
                continue;
            }
        }
        open = Some(at);
    }
    let placed = placed;
    let mut event_aliases = vec![0u64; persons];
    for run in placed.iter().flat_map(Placed::own) {
        let person = run.person as usize;
        event_aliases[person] |= run.aliases;
        // A second run of a person: its events are not one stretch.
        flags[person] |= if flags[person] & SEEN != 0 || run.unfit { UNFIT | SEEN } else { SEEN };
    }
    for extra in extras {
        let (person, alias) = (extra.person as usize, extra.u & ALIAS_MASK);
        event_aliases[person] |= 1 << (alias & (MASK_ALIASES - 1));
        // No run to join: the person's events are these alone, and the classic lane takes them.
        flags[person] |= if flags[person] & SEEN == 0 || alias >= MASK_ALIASES { UNFIT | EXTRA } else { EXTRA };
    }
    let flags = &flags[..];
    // A person is one fast-lane person when nothing above disqualifies it and every ID its
    // rows use is in its one link component, or it has no links and uses a single ID.
    let fit = |person: usize| {
        let ids = event_aliases[person] | click_aliases[person];
        flags[person] & UNFIT == 0
            && match links.state[person] {
                ONE => ids & !links.mask[person] == 0,
                NONE => ids.count_ones() <= 1,
                _ => false,
            }
    };
    for extra in extras {
        if !fit(extra.person as usize) {
            slow_events.push(PersonEvent { person: links.person(extra.u), ts: extra.ts, pay: extra.pay });
        }
    }
    lap("runs");

    // A partition's runs per task, with its purchases; then the persons in ranges, for the
    // clicks of the classic lane's.
    let run_tasks = placed.len();
    let (table, table_users) = (&times[..], &users[..]);
    par::run(works, run_tasks + persons.div_ceil(CLICK_TASK_PERSONS), |_, work, task| {
        let (times, users) = (table, table_users);
        if task < run_tasks {
            let part = &placed[task];
            // The purchases of a run that reaches past this partition, from each it passes.
            let mut passing: Vec<(u32, u32)> = Vec::new();
            let mut own_next = 0;
            for run in part.own() {
                let person = run.person as usize;
                let stretch = run.first as usize..run.end as usize;
                // Only the partition's last run can reach past it.
                let crossing = stretch.end > (task + 1) << plan.shift;
                if crossing {
                    let from = part.pays.partition_point(|&(number, _)| number < run.first);
                    passing.extend_from_slice(&part.pays[from..]);
                    for (_, more) in placed.iter().enumerate().skip(task + 1).take_while(|&(later, _)| later << plan.shift < stretch.end) {
                        passing.extend(more.pays.iter().take_while(|&&(number, _)| number < run.end));
                    }
                }
                let pays: &[(u32, u32)] = if crossing { &passing } else { &part.pays };
                let mut next_pay = if crossing { 0 } else { own_next };
                // Purchases of events that left the table (none in practice) or of a skipped run.
                while pays.get(next_pay).is_some_and(|&(number, _)| number < run.first) {
                    next_pay += 1;
                }
                let pay_of = |number: usize, next_pay: &mut usize| match pays.get(*next_pay) {
                    Some(&(at, pay)) if at as usize == number => {
                        *next_pay += 1;
                        pay
                    }
                    _ => 0,
                };
                if fit(person) {
                    let name = match links.state[person] {
                        ONE => links.name[person],
                        _ => scramble::id_of(u64::from(users[stretch.start]), plan.user_key, b'u'),
                    };
                    let clicks = index.gather(plan, person, flags[person], &mut work.clicks);
                    if flags[person] & EXTRA == 0 {
                        sessions::positional::person(ctx, name, &times[stretch], run.first, pays, &mut next_pay, clicks, &mut work.agg);
                    } else {
                        let from = extras.partition_point(|extra| (extra.person as usize) < person);
                        let count = extras[from..].iter().take_while(|extra| extra.person as usize == person).count();
                        let room = stretch.len();
                        let table = stretch.filter(|&number| times[number] != 0).map(|number| (times[number], pay_of(number, &mut next_pay)));
                        let all = with_extras(table, &extras[from..from + count], room);
                        sessions::positional::person_pairs(ctx, name, &all, clicks, &mut work.agg);
                    }
                } else {
                    for number in stretch {
                        if times[number] != 0 {
                            let pay = pay_of(number, &mut next_pay);
                            work.slow_events.push(PersonEvent { person: links.person(users[number]), ts: times[number], pay });
                        }
                    }
                }
                if !crossing {
                    own_next = next_pay;
                }
            }
        } else {
            let range = task - run_tasks;
            for person in range * CLICK_TASK_PERSONS..((range + 1) * CLICK_TASK_PERSONS).min(persons) {
                if (!index.stretch(person).is_empty() || flags[person] & XCLICK != 0) && !fit(person) {
                    index.classic(plan, links, person, flags[person], &mut work.slow_clicks);
                }
            }
        }
    });
    lap("sessions");

    let mut stats = (0, 0, [0u64; 4]);
    if timing {
        // Why, in slots: flagged (split, out of order, wide alias, outside the numbering),
        // mixed links, an ID outside the one component, several unlinked IDs.
        for run in placed.iter().flat_map(Placed::own) {
            let person = run.person as usize;
            stats.0 += 1;
            if !fit(person) {
                stats.1 += 1;
                let reason = match links.state[person] {
                    _ if flags[person] & UNFIT != 0 => 0,
                    MIXED => 1,
                    ONE => 2,
                    _ => 3,
                };
                stats.2[reason] += u64::from(run.end - run.first);
            }
        }
    }
    // The exit path skips destructors (`fast-exit`), and these are large.
    std::mem::forget((times, users, placed));
    stats
}

/// Places partition `part`'s rows in the worker's slots, `work.kept`: a slot keeps the copy
/// with the smallest `ingest` (copies that tie are identical, rules "Events" step 2). Returns
/// the kept purchases as `(event number, pay)`, ascending; `None` for a partition without
/// rows. A kept copy whose user is not a number leaves the slots for the classic lane
/// (`work.slow_events`).
fn place_rows(part: usize, plan: &PosPlan, parsed: &[PosOut], links: &Links, work: &mut Placing) -> Option<Vec<(u32, u32)>> {
    if parsed.iter().all(|out| out.events.len_of(part) == 0) {
        return None;
    }
    let (slots, base) = (1usize << plan.shift, part << plan.shift);
    let Placing { kept, odd, slow_events, paid, pay_at } = work;
    // One 16-byte slot per event number while placing: a row reads and writes one cache line
    // of a table that fits L2. Three arrays (ingest, time, user) were three lines a row, and
    // those accesses were two thirds of this stage.
    kept.clear();
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    // Emptied two slots a store, in 256-bit registers: left to the compiler this fill used
    // 512-bit ones, the stage's only use of them (see the scan below for why that matters).
    //
    // SAFETY: room for `slots` records, an even number, each written before the length is
    // set; a `PosEvent` is two words (`repr(C)`).
    unsafe {
        use core::arch::x86_64::*;
        kept.reserve(slots);
        let (low, high) = PosEvent::EMPTY.words();
        let empty = _mm256_setr_epi64x(low as i64, high as i64, low as i64, high as i64);
        debug_assert!(slots % 2 == 0);
        for at in (0..slots).step_by(2) {
            _mm256_storeu_si256(kept.as_mut_ptr().add(at).cast(), empty);
        }
        kept.set_len(slots);
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    kept.resize(slots, PosEvent::EMPTY);
    odd.clear();
    for (worker, out) in parsed.iter().enumerate() {
        out.events.for_each(part, |piece| {
            for row in piece {
                // SAFETY: the mask keeps the slot below `slots`, the table's length.
                let slot = unsafe { kept.get_unchecked_mut(row.slot() as usize & (slots - 1)) };
                if row.ingest() < slot.ingest() {
                    *slot = *row;
                    if row.flags() & POS_ODD_USER != 0 {
                        odd_user(row, worker, odd);
                    }
                }
            }
        });
    }
    let valid = |row: &PosEvent| row.flags() & POS_VALID != 0;
    // The purchases come in any order and leave in slot order: a bit per slot that has one
    // and its `pay` beside, then the bits read in order. (Sorted as pairs they were 108
    // instructions a purchase.) Twins give one entry, with the smaller `pay` should they differ.
    paid.clear();
    paid.resize(slots.div_ceil(64), 0);
    pay_at.resize(slots, 0);
    let mut count = 0;
    for out in parsed {
        for piece in out.pays.pieces(part) {
            for pay in piece {
                let slot = pay.slot as usize & (slots - 1);
                // The kept copy has this `ingest`, so it is this row or its twin.
                if kept[slot].ingest() == pay.ingest && valid(&kept[slot]) {
                    let (word, bit) = (&mut paid[slot / 64], 1u64 << (slot % 64));
                    pay_at[slot] = if *word & bit == 0 { pay.pay } else { pay_at[slot].min(pay.pay) };
                    count += usize::from(*word & bit == 0);
                    *word |= bit;
                }
            }
        }
    }
    let mut pays: Vec<(u32, u32)> = Vec::with_capacity(count);
    for (at, &word) in paid.iter().enumerate() {
        let mut rest = word;
        while rest != 0 {
            let slot = at * 64 + rest.trailing_zeros() as usize;
            rest &= rest - 1;
            pays.push(((base + slot) as u32, pay_at[slot]));
        }
    }
    // A kept copy whose user is not a number leaves the table for the classic lane.
    if !odd.is_empty() {
        for &(slot, row_ingest, worker, index) in odd.iter() {
            let row = kept[slot as usize];
            if row.flags() & POS_ODD_USER != 0 && row.ingest() == row_ingest && valid(&row) {
                let number = base as u32 + slot;
                let pay = pays.binary_search_by_key(&number, |&(at, _)| at).map_or(0, |at| pays[at].1);
                let person = links.general.person(parsed[worker as usize].odd_users[index as usize]);
                slow_events.push(PersonEvent { person, ts: row.ts(), pay });
                kept[slot as usize] = PosEvent::new(0, 0, row_ingest, slot as u16, 0);
            }
        }
        pays.retain(|&(number, _)| valid(&kept[number as usize - base]));
    }
    Some(pays)
}

/// A placed partition's two columns (the `ts` of a valid kept copy, 0 for anything else, so an
/// invalid kept copy leaves no event; and the user) and its runs, in one pass over the slots.
/// `times` and `users` are the partition's stretch of the table, or a worker's own.
fn scan(kept: &[PosEvent], times: &mut [u32], users: &mut [u32], base: usize, runs: &mut Vec<Run>) {
    let slots = kept.len();
    assert!(times.len() == slots && users.len() == slots);
    #[cfg_attr(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512dq", target_feature = "avx512vl"), allow(unused_variables))]
    let valid = |row: &PosEvent| row.flags() & POS_VALID != 0;
    // The table's two columns (the `ts` of a valid kept copy, 0 for anything else, and the
    // user) and the runs, in one pass over the slots: a person's first event opens a run.
    let mut open = Open::none();
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512dq", target_feature = "avx512vl"))]
    // Four slots a step. When all four are events of the open run, which is most steps, the
    // run takes them from the registers: their order, their aliases, the last `ts`. Any other
    // step (a slot without an event, another person) goes event by event. One slot at a time
    // this pass was 18 instructions a slot, about 4% of a run at 20M.
    //
    // In 256-bit registers on purpose (AVX-512VL for the masks and the two-source permute):
    // on Skylake and Cascade Lake Xeons even light 512-bit instructions lower the core's
    // clock, and this stage has no other use for them. Eight slots a step measured the same.
    //
    // SAFETY: `kept`, `times` and `users` hold `slots` entries, a multiple of 4, and a
    // `PosEvent` is two words (`repr(C)`).
    unsafe {
        use core::arch::x86_64::*;
        debug_assert!(slots % 4 == 0 && kept.len() == slots);
        let all = |value: u64| _mm256_set1_epi64x(value as i64);
        // The first and second word of 4 records, from two loads of 2.
        let (first_words, second_words) = (_mm256_setr_epi64x(0, 2, 4, 6), _mm256_setr_epi64x(1, 3, 5, 7));
        for at in (0..slots).step_by(4) {
            let rows = kept.as_ptr().add(at).cast::<__m256i>();
            // The second load through `black_box`: seen as the 32 bytes after the first, the
            // two become one 512-bit load and the permutes below 512-bit ones.
            let (a, b) = (_mm256_loadu_si256(rows), _mm256_loadu_si256(std::hint::black_box(rows.add(1))));
            // `PosEvent`: u | ts << 32, then ingest | slot << 32 | flags << 48.
            let (low, high) = (_mm256_permutex2var_epi64(a, first_words, b), _mm256_permutex2var_epi64(a, second_words, b));
            let events = _mm256_test_epi64_mask(high, all(u64::from(POS_VALID) << 48));
            let ts = _mm256_maskz_srli_epi64::<32>(events, low);
            let u = _mm256_and_si256(low, all(0xFFFF_FFFF));
            _mm_storeu_si128(times.as_mut_ptr().add(at).cast(), _mm256_cvtepi64_epi32(ts));
            _mm_storeu_si128(users.as_mut_ptr().add(at).cast(), _mm256_cvtepi64_epi32(u));
            let same = _mm256_cmpeq_epi64_mask(_mm256_srli_epi64::<{ ALIAS_BITS as i32 }>(u), all(u64::from(open.person)));
            if events & same == 0xF {
                // Each `ts` against the one before it, the run's last before the first.
                let before = _mm256_alignr_epi64::<3>(ts, all(u64::from(open.last_ts)));
                let alias = _mm256_and_si256(u, all(u64::from(ALIAS_MASK)));
                let unfit = _mm256_cmplt_epu64_mask(ts, before) | _mm256_cmpge_epu64_mask(alias, all(u64::from(MASK_ALIASES)));
                let bits = _mm256_sllv_epi64(all(1), _mm256_and_si256(alias, all(u64::from(MASK_ALIASES - 1))));
                // The four lanes' bits together.
                let halves = _mm_or_si128(_mm256_castsi256_si128(bits), _mm256_extracti128_si256::<1>(bits));
                open.unfit |= unfit != 0;
                open.aliases |= (_mm_cvtsi128_si64(halves) | _mm_extract_epi64::<1>(halves)) as u64;
                (open.last_ts, open.end) = (*times.get_unchecked(at + 3), at as u32 + 4);
            } else {
                let mut rest = events;
                while rest != 0 {
                    let slot = at + rest.trailing_zeros() as usize;
                    rest &= rest - 1;
                    open.event(slot, *times.get_unchecked(slot), *users.get_unchecked(slot), base, runs);
                }
            }
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512dq", target_feature = "avx512vl")))]
    for (slot, row) in kept.iter().enumerate() {
        times[slot] = if valid(row) { row.ts() } else { 0 };
        users[slot] = row.u();
        if times[slot] != 0 {
            open.event(slot, times[slot], users[slot], base, runs);
        }
    }
    open.close(base, runs);
}

/// Writes partition `part`'s rows into its stretch of the table (`place_rows`, then `scan`)
/// and lists the kept purchases and the runs. `None` for a partition without rows.
///
/// # Safety
/// No other thread touches event numbers `part << shift .. (part + 1) << shift` of the arrays.
unsafe fn place(part: usize, plan: &PosPlan, parsed: &[PosOut], links: &Links, times: Shared<u32>, users: Shared<u32>, work: &mut Placing) -> Option<Placed> {
    let pays = place_rows(part, plan, parsed, links, work)?;
    let (slots, base) = (1usize << plan.shift, part << plan.shift);
    // SAFETY: the caller's contract; the arrays hold `parts << shift` slots.
    let (times, users) = unsafe { (std::slice::from_raw_parts_mut(times.0.add(base), slots), std::slice::from_raw_parts_mut(users.0.add(base), slots)) };
    let mut runs = Vec::new();
    scan(&work.kept, times, users, base, &mut runs);
    Some(Placed { runs, skip: 0, pays })
}

/// Cuts a placed partition into sessions, 16 slots a step: the kept events' `ts` and users are
/// packed to the front of two registers, each is compared with the event before it (the last
/// of the step before, carried), and for every event that starts a session its ordinal, `ts`
/// and user are stored: `c.s_ord`, `c.s_ts`, `c.s_u`, with `c.blk_ns` and `c.starts` to find
/// a slot's session again. The runs are listed as `scan` lists them, in `runs`, each with its
/// first session in `c.run_s0`; only a step where a run starts, an alias changes or a `ts`
/// goes back leaves the registers. Returns the events and the sessions.
///
/// # Safety
/// `kept.len()` is a multiple of 16.
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512dq",
    target_feature = "avx512vl",
    target_feature = "bmi2"
))]
unsafe fn cut(kept: &[PosEvent], base: usize, c: &mut Cut, runs: &mut Vec<Run>) -> (u32, usize) {
    use core::arch::x86_64::*;
    let slots = kept.len();
    debug_assert!(slots % 16 == 0);
    let blocks = slots / 16;
    // A step stores 16 lanes wherever its sessions begin, and the passes after it one entry
    // past the last: room for both.
    for column in [&mut c.s_ord, &mut c.s_ts, &mut c.s_u, &mut c.s_day, &mut c.s_camp, &mut c.s_row, &mut c.row_day, &mut c.row_s0, &mut c.row_e0] {
        column.resize(slots + 32, 0);
    }
    c.s_rev.resize(slots + 32, 0);
    c.row_rev.resize(slots + 32, 0);
    c.row_cat.resize(slots + 32, 0);
    c.blk_ns.resize(blocks + 1, 0);
    c.starts.resize(blocks + 1, 0);
    c.run_s0.clear();
    let (s_ord, s_ts, s_u, blk_ns, starts) = (c.s_ord.as_mut_ptr(), c.s_ts.as_mut_ptr(), c.s_u.as_mut_ptr(), c.blk_ns.as_mut_ptr(), c.starts.as_mut_ptr());
    // SAFETY: every load is of 4 records inside `kept`; every store is at most 16 entries from
    // `ns <= slots`, inside the room made above.
    unsafe {
        let (first_words, second_words) = (_mm512_setr_epi64(0, 2, 4, 6, 8, 10, 12, 14), _mm512_setr_epi64(1, 3, 5, 7, 9, 11, 13, 15));
        let valid_bit = _mm512_set1_epi64((u64::from(POS_VALID) << 48) as i64);
        let lanes = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        let (person_bits, alias_bits) = (_mm512_set1_epi32(!ALIAS_MASK as i32), _mm512_set1_epi32(ALIAS_MASK as i32));
        let (gap, run_flag) = (_mm512_set1_epi32(sessions::SESSION_GAP as i32), _mm512_set1_epi32(RUN as i32));
        // The last kept event so far, in every lane, and as scalars with its slot.
        let (mut last_ts_lanes, mut last_u_lanes) = (_mm512_setzero_si512(), _mm512_setzero_si512());
        let (mut last_ts, mut last_slot) = (0u32, 0usize);
        // The open run.
        let mut open: Option<Run> = None;
        let (mut events, mut ns) = (0u32, 0usize);
        for block in 0..blocks {
            let rows = kept.as_ptr().add(block * 16).cast::<__m512i>();
            let (a, b, c2, d) = (_mm512_loadu_si512(rows), _mm512_loadu_si512(rows.add(1)), _mm512_loadu_si512(rows.add(2)), _mm512_loadu_si512(rows.add(3)));
            // `PosEvent`: u | ts << 32, then ingest | slot << 32 | flags << 48.
            let (low_0, high_0) = (_mm512_permutex2var_epi64(a, first_words, b), _mm512_permutex2var_epi64(a, second_words, b));
            let (low_1, high_1) = (_mm512_permutex2var_epi64(c2, first_words, d), _mm512_permutex2var_epi64(c2, second_words, d));
            let keep = u32::from(_mm512_test_epi64_mask(high_0, valid_bit)) | u32::from(_mm512_test_epi64_mask(high_1, valid_bit)) << 8;
            *blk_ns.add(block) = ns as u32;
            if keep == 0 {
                *starts.add(block) = 0;
                continue;
            }
            let ts = _mm512_inserti64x4::<1>(_mm512_castsi256_si512(_mm512_cvtepi64_epi32(_mm512_srli_epi64::<32>(low_0))), _mm512_cvtepi64_epi32(_mm512_srli_epi64::<32>(low_1)));
            let u = _mm512_inserti64x4::<1>(_mm512_castsi256_si512(_mm512_cvtepi64_epi32(low_0)), _mm512_cvtepi64_epi32(low_1));
            // The kept events to the front, in slot order.
            let (ts, u) = (_mm512_maskz_compress_epi32(keep as __mmask16, ts), _mm512_maskz_compress_epi32(keep as __mmask16, u));
            let width = keep.count_ones();
            let live = ((1u32 << width) - 1) as __mmask16;
            // Each event beside the one before it.
            let (before_ts, before_u) = (_mm512_alignr_epi32::<15>(ts, last_ts_lanes), _mm512_alignr_epi32::<15>(u, last_u_lanes));
            let delta = _mm512_xor_si512(u, before_u);
            // The partition's first event starts a run whatever the lanes held.
            let run = (_mm512_test_epi32_mask(delta, person_bits) | __mmask16::from(open.is_none())) & live;
            let start = run | (_mm512_cmpgt_epu32_mask(_mm512_sub_epi32(ts, before_ts), gap) & live);
            let changed = _mm512_test_epi32_mask(delta, alias_bits) & !run & live;
            let back = _mm512_cmplt_epu32_mask(ts, before_ts) & !run & live;

            let ordinals = _mm512_add_epi32(_mm512_set1_epi32(events as i32), lanes);
            let flagged = _mm512_mask_or_epi32(ordinals, run, ordinals, run_flag);
            _mm512_storeu_si512(s_ord.add(ns).cast(), _mm512_maskz_compress_epi32(start, flagged));
            _mm512_storeu_si512(s_ts.add(ns).cast(), _mm512_maskz_compress_epi32(start, ts));
            _mm512_storeu_si512(s_u.add(ns).cast(), _mm512_maskz_compress_epi32(start, u));
            *starts.add(block) = _pdep_u32(u32::from(start), keep) as u16;

            let odd = run | changed | back;
            if odd != 0 {
                let (mut ts_of, mut u_of) = ([0u32; 16], [0u32; 16]);
                _mm512_storeu_si512(ts_of.as_mut_ptr().cast(), ts);
                _mm512_storeu_si512(u_of.as_mut_ptr().cast(), u);
                let slot_of = |lane: usize| block * 16 + _pdep_u32(1 << lane, keep).trailing_zeros() as usize;
                let mut rest = u32::from(odd);
                while rest != 0 {
                    let lane = rest.trailing_zeros() as usize;
                    rest &= rest - 1;
                    let (ts_here, u_here) = (ts_of[lane], u_of[lane]);
                    let alias = u_here & ALIAS_MASK;
                    if run >> lane & 1 != 0 {
                        if let Some(mut done) = open.take() {
                            // Its last event is the one before this.
                            let (slot, ts_before) = if lane == 0 { (last_slot, last_ts) } else { (slot_of(lane - 1), ts_of[lane - 1]) };
                            (done.end, done.last_ts) = ((base + slot + 1) as u32, ts_before);
                            runs.push(done);
                        }
                        c.run_s0.push(ns as u32 + (u32::from(start) & ((1 << lane) - 1)).count_ones());
                        open = Some(Run {
                            person: u_here >> ALIAS_BITS,
                            first: (base + slot_of(lane)) as u32,
                            end: 0,
                            first_ts: ts_here,
                            last_ts: ts_here,
                            aliases: 1 << (alias & (MASK_ALIASES - 1)),
                            unfit: alias >= MASK_ALIASES,
                        });
                    } else if let Some(run) = open.as_mut() {
                        run.aliases |= 1 << (alias & (MASK_ALIASES - 1));
                        run.unfit |= alias >= MASK_ALIASES || back >> lane & 1 != 0;
                    }
                }
            }

            let last = _mm512_set1_epi32(width as i32 - 1);
            (last_ts_lanes, last_u_lanes) = (_mm512_permutexvar_epi32(last, ts), _mm512_permutexvar_epi32(last, u));
            last_ts = _mm_cvtsi128_si32(_mm512_castsi512_si128(last_ts_lanes)) as u32;
            last_slot = block * 16 + 31 - keep.leading_zeros() as usize;
            ns += start.count_ones() as usize;
            events += width;
        }
        if let Some(mut done) = open.take() {
            (done.end, done.last_ts) = ((base + last_slot + 1) as u32, last_ts);
            runs.push(done);
        }
        *blk_ns.add(blocks) = ns as u32;
        *s_ord.add(ns) = events | RUN;
        c.run_s0.push(ns as u32);
        (events, ns)
    }
}

#[cold]
#[inline(never)]
fn odd_user(row: &PosEvent, worker: usize, odd: &mut Vec<(u32, u32, u32, u32)>) {
    odd.push((u32::from(row.slot()), row.ingest(), worker as u32, row.u()));
}
