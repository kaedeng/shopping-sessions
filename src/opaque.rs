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
use ingest::opaque::{Ev, EvL, LinkRec, Out, Plan};
use types::chunks::Chunks;
use types::hash::{hash, person_hash};
use types::parts::{Combine, Mode, Parts, Plain};
use types::record::{ClickRec, PersonEvent};
use types::users::{Users, mix_long};
use types::{Ctx, Id, par};

use crate::Scattered;

/// Links `Persons::new` asks for the entries of ahead of.
const AHEAD: usize = 16;

/// Which person each user is, and what a person is called.
pub struct Persons {
    /// Per user's number: 1 + the user this one hangs under, which once the links are joined
    /// is its component's root. 0 for a root, and for a user no link names: its own person,
    /// called by its own ID.
    up: Vec<u32>,
    /// Per root: 1 + the user with the component's byte-wise smallest ID (0 before any link
    /// named the root), and `LinkRec::order` of that ID.
    names: Vec<(u32, u64)>,
}

impl Persons {
    /// The persons the links make: a union-find over the users' numbers, each root keeping
    /// its component's smallest ID as the links come.
    pub fn new(users: &Users, links: &[Vec<LinkRec>]) -> Persons {
        // Zeroed lazily by the kernel, and touched only where a user's number is: the
        // numbers are dense, so that is the arrays' first few megabytes. Room for 4 and 2
        // million entries at least: a block that large is fresh memory, where a smaller one
        // may be a freed block the allocator clears first.
        let mut up = vec![0u32; users.bound().max(4 << 20)];
        let mut names = vec![(0u32, 0u64); users.bound().max(2 << 20)];
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
        let all = || links.iter().flatten();
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
    for chunks in lists {
        chunks.for_each(list, |piece| {
            for row in piece {
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

/// The valid ones of `kept` to their persons' buckets.
#[inline(always)]
fn scatter<R: Row, M: Mode>(kept: &[R], persons: &Persons, bucket_bits: u32, out: &mut Parts<PersonEvent>) {
    let shift = 64 - bucket_bits;
    for row in kept {
        persons.prefetch(row.words().0 as u32);
    }
    for row in kept {
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
}

/// What a person's place is called.
pub struct Names {
    plan: Plan,
    persons: Persons,
}

impl Names {
    /// Appends the name of the person whose key is `key`: its root's number and one.
    pub fn text(&self, key: Id, out: &mut Vec<u8>) {
        self.plan.users.text(self.persons.name(key as u32 - 1), out);
    }
}

/// Stages 1 and 2. `None` when the directory was too small for the input's users: the
/// caller starts over with a larger `grow`. `whole_clicks`: `ingest::opaque::click_id_at`.
pub fn stages(ctx: &Ctx, ingest: &Ingest, grow: u32, whole_clicks: bool, parallel_alloc: bool, scatter_mode: Option<usize>, lap: &mut dyn FnMut(&'static str)) -> Option<Staged> {
    let plan = ingest.opaque_plan(grow, whole_clicks);
    let mut parsed: Vec<Out> = par::init(ctx.workers, parallel_alloc, |_| ingest.opaque_out(&plan, ctx.workers));
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
            let read: Vec<Vec<LinkRec>> = links.iter().map(|read| read.get().cloned().unwrap_or_default()).collect();
            drop(persons.set(Persons::new(&plan.users, &read)));
            types::tl::mark("links named");
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
    let clicks: usize = parsed.iter().map(|out| out.clicks.len()).sum();
    let room = |rows: usize| rows / ctx.workers / buckets * 9 / 8 + 64;
    if std::env::var_os("SS_TIMING").is_some() {
        let long: usize = parsed.iter().map(|out| out.long_events.len()).sum();
        let links: usize = links.iter().filter_map(OnceLock::get).map(Vec::len).sum();
        let slow: usize = parsed.iter().map(|out| out.slow).sum();
        eprintln!("opaque: {events} event rows ({long} with a long event_id), {clicks} clicks, {links} links, {} lists, {} users, {slow} lines through the row code", parts + long_parts, plan.users.count());
    }
    let mut scattered: Vec<Scattered> = par::init(ctx.workers, parallel_alloc, |_| Scattered {
        events: Parts::new(buckets, room(events), scatter_mode),
        clicks: Parts::new(buckets, room(clicks), scatter_mode),
        scratch: Default::default(),
        opaque: Default::default(),
    });
    {
        let (parsed, persons) = (&parsed[..], &persons);
        par::run(&mut scattered, parts + long_parts + parsed.len(), |_, mine, task| {
            let Scattered { events, clicks, opaque: Scratch { table, kept, kept_long }, .. } = mine;
            if task < parts {
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
    if types::mem::free_early() {
        par::each(&mut parsed, |_, out| out.release());
    }
    // The exit path skips destructors (`fast-exit`), and these are large.
    std::mem::forget(parsed);
    Some(Staged { scattered, names: Names { plan, persons } })
}
