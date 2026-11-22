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
use ingest::opaque::{Ev, EvL, LinkRec, Out, Placing, Plan};
use types::chunks::Chunks;
use types::hash::{hash, person_hash};
use types::parts::{Combine, Mode, Parts, Plain};
use types::record::{ClickRec, PersonEvent};
use types::mem::Zeroed;
use types::users::{Users, mix_long};
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
    kept_long: Vec<EvL>,
}

/// Slots per row, rounded up to a power of two: the table ends a quarter full or less.
const ROOM: usize = 4;
/// Rows `dedup` asks for the slot of ahead of the row it is at: `SS_DEDUP_AHEAD`, 8 unless
/// set, 0 for none. A row asked for is 11 instructions more (2% of a run's), for 6% of the
/// stage's time at 21,000 rows a list (a table of half a megabyte) on a machine with a core
/// a worker; see `SS_ASK` in `ingest::opaque::kern`.
fn ahead_slots() -> usize {
    static AHEAD: OnceLock<usize> = OnceLock::new();
    *AHEAD.get_or_init(|| types::env::number("SS_DEDUP_AHEAD", 8).min(64))
}

/// List `list` of every worker deduplicated into `kept`: one record per `event_id`, the copy
/// with the smallest `ingest`. `bits` are the hash bits that chose the list.
fn dedup<'p, R: Row + 'p>(lists: impl Iterator<Item = &'p Chunks<R>> + Clone, list: usize, bits: u32, table: &mut Vec<u32>, kept: &mut Vec<R>) {
    kept.clear();
    let rows: usize = lists.clone().map(|chunks| chunks.len_of(list)).sum();
    if rows == 0 {
        return;
    }
    let slot_bits = (ROOM * rows).next_power_of_two().max(1024).trailing_zeros();
    assert!(bits + slot_bits <= 64 && rows < u32::MAX as usize);
    let (shift, mask) = (64 - bits - slot_bits, (1usize << slot_bits) - 1);
    table.clear();
    table.resize(mask + 1, 0);
    kept.reserve(rows);
    let ahead = ahead_slots();
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
    pub fn texts(&self, keys: &[Id], out: &mut Vec<u8>) {
        let users = &self.plan.users;
        let mut named = [0u32; NAMES_HELD];
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
                let at = out.len();
                out.extend_from_slice(&[0; 4]);
                users.text(name, out);
                let len = (out.len() - at - 4) as u32;
                out[at..at + 4].copy_from_slice(&len.to_le_bytes());
            }
        }
    }
}

/// Names `Names::texts` reads together.
const NAMES_HELD: usize = 32;

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
    let place_clicks = dense_buckets && click_mode != 1;
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
                // (there are at most twice as many as `SS_BUCKET_BITS` asks for).
                let count = plan.users.count().max(1);
                let shift = (usize::BITS - (count - 1).leading_zeros()).saturating_sub(ctx.bucket_bits);
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
    let events: usize = parsed.iter().map(|out| out.events.len() + out.long_events.len()).sum();
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
            let Scattered { events, clicks, opaque: Scratch { table, kept, kept_long }, dense: ranges, .. } = mine;
            if let (Some(ranges), Some((shift, _))) = (ranges, dense) {
                if task < parts {
                    dedup(parsed.iter().map(|out| &out.events), task, plan.part_bits, table, kept);
                    if recycle {
                        give_chunks(parsed, |out| &out.events, task, ranges);
                    }
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
                dedup(parsed.iter().map(|out| &out.events), task, plan.part_bits, table, kept);
                if events.combining() { scatter::<_, Combine>(kept, persons, ctx.bucket_bits, events) } else { scatter::<_, Plain>(kept, persons, ctx.bucket_bits, events) }
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
