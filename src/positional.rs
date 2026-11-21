//! Slot `pipe-positional`: the pipeline for data that carries the generator's numbering.
//!
//! The generator numbers events `0..` in (person, time) order and a user ID is
//! `person << 9 | alias`, both scrambled under keys `Ingest::pos_plan` finds at run time
//! (`types::scramble`). Once every event row sits at its number in one table, the table is
//! deduplicated (a slot keeps the copy with the smallest `ingest`), grouped by person and in
//! time order, with no hash table, no link lookup per row and no sort:
//!
//! 1. Parse (`ingest::pos`): a 16-byte record per event line into the list of its number's
//!    partition; clicks into the list of their person's range; the links beside it.
//! 2. Place: each partition writes its rows into its stretch of the table and lists the
//!    stretches of one person ("runs"); the clicks of each range are grouped by person with a
//!    counting scatter.
//! 3. Sessions: a person's run goes straight through sessions, credit and counters.
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
use ingest::pos::{Link, PosOut, PosPlan};
use sessions::{Agg, Click};
use types::hash::person_hash;
use types::par;
use types::pos::{POS_ODD_USER, POS_VALID, PosEvent};
use types::record::{ClickRec, PersonEvent, VALID};
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

/// Every click grouped by person, range after range, in one array.
///
/// One allocation for all ranges, made before their tasks run. A task that allocated its own
/// arrays grew its thread's heap (`mprotect`, which takes the address-space lock for writing)
/// while the other tasks took page faults (the same lock, for reading): at 20M a click cost 50
/// ns with 8 workers against 12 ns on one, and the stage was 28 ms for 66 ms of work.
struct Grouped {
    /// Each range's first position in `clicks`, and one more: the total.
    bases: Vec<u32>,
    /// One past each person's last click in `clicks`, for the persons of a range with clicks:
    /// an entry per person number, written by the range's task.
    ends: Shared<u32>,
    clicks: Shared<Click>,
    /// Each click's alias, beside `clicks`.
    aliases: Shared<u16>,
    click_shift: u32,
    /// Own the three arrays, which are read and written through the pointers only (the
    /// lengths of the first two stay 0).
    _owners: (Vec<Click>, Vec<u16>, Vec<u32>),
}

impl Grouped {
    /// Room for the clicks of `parsed`, nothing grouped yet.
    fn new(plan: &PosPlan, parsed: &[PosOut]) -> Grouped {
        let mut bases = Vec::with_capacity(plan.click_parts + 1);
        let mut total = 0usize;
        for range in 0..plan.click_parts {
            bases.push(total as u32);
            total += parsed.iter().map(|out| out.clicks.len_of(range)).sum::<usize>();
        }
        assert!(total <= u32::MAX as usize, "{total} clicks: positions are 32 bits");
        bases.push(total as u32);
        let (mut clicks, mut aliases): (Vec<Click>, Vec<u16>) = (Vec::with_capacity(total), Vec::with_capacity(total));
        // Zeroed lazily by the kernel; a range's task counts in its own stretch.
        let mut ends = vec![0u32; plan.persons as usize];
        Grouped {
            bases,
            ends: Shared(ends.as_mut_ptr()),
            clicks: Shared(clicks.as_mut_ptr()),
            aliases: Shared(aliases.as_mut_ptr()),
            click_shift: plan.click_shift,
            _owners: (clicks, aliases, ends),
        }
    }

    /// The clicks in range `range`.
    fn total(&self, range: usize) -> usize {
        (self.bases[range + 1] - self.bases[range]) as usize
    }

    /// The positions of a person's clicks, once its range is grouped. `person` is below
    /// `PosPlan::persons`.
    fn range(&self, person: usize) -> std::ops::Range<usize> {
        let range = person >> self.click_shift;
        if self.total(range) == 0 {
            return 0..0;
        }
        assert!(person < self._owners.2.len());
        // SAFETY: an entry per person number, checked above; the range's task wrote them all.
        unsafe {
            let start = if person == range << self.click_shift { self.bases[range] } else { *self.ends.0.add(person - 1) };
            start as usize..*self.ends.0.add(person) as usize
        }
    }
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

/// A worker's share of a click range that all workers group together (`group_clicks_wide`),
/// per person of the range.
#[derive(Default)]
struct Share {
    /// How many clicks of the person the worker's list holds; then where its next one goes.
    counts: Vec<u32>,
    /// The aliases of those clicks, and whether one is over the masks.
    masks: Vec<u64>,
    over: Vec<u8>,
}

/// A worker's state for the stages after the parse.
struct Work {
    agg: Agg,
    slow_events: Vec<PersonEvent>,
    slow_clicks: Vec<ClickRec>,
    placing: Placing,
    share: Share,
    /// The worker's classic-lane events and clicks, by bucket.
    lane: (Vec<Vec<PersonEvent>>, Vec<Vec<ClickRec>>),
    /// The fused stages' partition, once placed: its two columns (`scan`) and its runs.
    times: Vec<u32>,
    users: Vec<u32>,
    runs: Vec<Run>,
    /// The fused stages' count of runs taken and of those sent to the classic lane, then the
    /// latter's slots by reason (`Fused::take`).
    stats: [u64; 6],
}

impl Work {
    /// `rows`: the `user_daily` rows to make room for. The six columns grow together, and
    /// doubling them a row at a time meant some 800 reallocations a run on the workers'
    /// threads, each a copy into fresh pages; reserved, a page is touched when a row lands.
    fn new(ctx: &Ctx, rows: usize) -> Work {
        let mut agg = Agg::new(ctx);
        agg.user_daily.reserve(rows);
        Work {
            agg,
            slow_events: Vec::new(),
            slow_clicks: Vec::new(),
            placing: Placing::default(),
            share: Share::default(),
            lane: Default::default(),
            times: Vec::new(),
            users: Vec::new(),
            runs: Vec::new(),
            stats: [0; 6],
        }
    }
}

/// How the stages after the parse run, the pick first (`SS_POS_STAGES`): a partition placed and
/// its sessions cut in one task (`fused_stages`), or the table placed whole and then read
/// (`split_stages`), which is also what `fused` falls back to.
const STAGES: &[&str] = &["fused", "split"];

/// Runs the whole pipeline after the inputs stage. `lap` marks a stage for `SS_TIMING`.
pub fn run(ctx: &Ctx, ingest: &Ingest, plan: &PosPlan, out: &Path, parallel_alloc: bool, lap: &mut dyn FnMut(&str)) -> Result<(), String> {
    let free_early = types::mem::free_early();

    // Stage 1. Task 0 reads the links; the others parse.
    let mut parsed: Vec<PosOut> = par::init(ctx.workers, parallel_alloc, |_| ingest.pos_out(ctx, plan));
    let links = OnceLock::new();
    par::run(&mut parsed, ingest.tasks() + 1, |_, out, task| match task {
        0 => drop(links.set(Links::build(&ingest.pos_links(), plan))),
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

    // Stage 2. Each click range grouped by person: a person's sessions need its clicks.
    let persons = plan.persons as usize;
    let mut click_aliases = vec![0u64; persons];
    let mut flags = vec![0u8; persons];
    for extra in &extras {
        flags[extra.person as usize] |= EXTRA;
    }
    let (click_aliases_at, flags_at) = (Shared(click_aliases.as_mut_ptr()), Shared(flags.as_mut_ptr()));
    let grouped = Grouped::new(plan, &parsed);
    let ends_at = grouped.ends;
    // A person-day with a session is a row: at 20M one for 9 events. Room for one per 5.
    let rows = plan.events() / ctx.workers / 5 + 1024;
    let mut works: Vec<Work> = par::init(ctx.workers, parallel_alloc, |_| Work::new(ctx, rows));
    // Persons are numbered largest first, so the first range holds a large share of all the
    // clicks (the whales are in it): as one task it kept one worker busy long after the others
    // had finished (the stage was 28 ms on 8 threads for 66 ms of work at 20M, and 100 to 140
    // ms at 60M). A range with more than a 16th of the clicks is grouped by all workers
    // together, after the others.
    let wide = (grouped.bases[plan.click_parts] as usize / 16).max(types::env::number("SS_POS_CLICK_WIDE", 1 << 16));
    par::run(&mut works, plan.click_parts, |_, _, range| {
        if grouped.total(range) > 0 && grouped.total(range) <= wide {
            // SAFETY: range `range` owns its persons' entries of the three arrays in this
            // stage, and its stretch of the clicks.
            unsafe { group_clicks(range, plan, &parsed, click_aliases_at, flags_at, ends_at, &grouped) };
        }
    });
    for range in (0..plan.click_parts).filter(|&range| grouped.total(range) > wide) {
        // SAFETY: as above, and nothing else runs.
        unsafe { group_clicks_wide(range, plan, &parsed, click_aliases_at, flags_at, ends_at, &grouped, &mut works) };
    }
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
        let shared = Fused { ctx, plan, parsed: &parsed, links, extras: &extras, grouped: &grouped, click_aliases: &click_aliases, flags: &flags, state: &state, conflict: &conflict };
        if fused_stages(&shared, &mut works, &mut slow_events, lap) {
            let sum = |at: usize| works.iter().map(|work| work.stats[at]).sum::<u64>();
            stats = Some((sum(0) as usize, sum(1) as usize, [sum(2), sum(3), sum(4), sum(5)]));
            if free_early {
                par::each(&mut parsed, |_, out| out.release());
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
        stats = Some(split_stages(ctx, plan, &mut parsed, links, &extras, &grouped, &click_aliases, &mut flags, &mut works, &mut slow_events, free_early, timing, lap));
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
    // `campaign_daily` is summed and written by a thread of its own while the workers write
    // their `user_daily` files (after them, on this thread, it was 2 ms at the end of the run).
    let mut tables: Vec<UserDaily> = works.iter_mut().map(|work| std::mem::take(&mut work.agg.user_daily)).collect();
    let aggs: Vec<Agg> = works.into_iter().map(|work| work.agg).collect();
    let failure = OnceLock::new();
    let campaigns = std::thread::scope(|scope| {
        let campaigns = scope.spawn(|| output::campaign_daily(&campaign_folder, ctx, sessions::campaign_daily(ctx, &aggs)));
        par::each(&mut tables, |worker, table| {
            if let Err(error) = output::user_daily(&user_folder, worker, ctx, std::mem::take(table)) {
                drop(failure.set(error));
            }
        });
        campaigns.join().expect("the campaign_daily thread panicked")
    });
    if let Some(error) = failure.into_inner() {
        return Err(error);
    }
    campaigns?;
    lap("output");
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
    std::mem::forget((grouped, aggs));
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
    grouped: &'a Grouped,
    click_aliases: &'a [u64],
    /// `UNFIT` for a person with a click alias over the masks (`group_clicks`), `EXTRA` for
    /// one with extras.
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

    /// A person's clicks.
    ///
    /// # Safety
    /// The caller took the person (`take`), so nothing else reads or writes its clicks.
    #[allow(clippy::mut_from_ref)]
    unsafe fn clicks(&self, person: usize) -> &'a mut [Click] {
        let at = self.grouped.range(person);
        // SAFETY: the positions are the person's, inside the clicks.
        unsafe { std::slice::from_raw_parts_mut(self.grouped.clicks.0.add(at.start), at.len()) }
    }

    /// Places partition `part` in the worker's own slots and, while they are in cache, takes
    /// every run but the first and the last through its sessions. Returns those two.
    fn partition(&self, part: usize, work: &mut Work) -> Vec<Edge> {
        let Work { agg, slow_events, placing, times, users, runs, stats, .. } = work;
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
            // SAFETY: taken just above.
            let clicks = unsafe { self.clicks(person) };
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
        let Work { agg, slow_events, stats, .. } = work;
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
        // SAFETY: taken just above.
        let clicks = unsafe { self.clicks(person) };
        let table = events.map(|(ts, pay, _)| (ts, pay));
        let all: Vec<(u32, u32)> = if extra { with_extras(table, self.extras_of(person), 0) } else { table.collect() };
        sessions::positional::person_pairs(self.ctx, name, &all, clicks, agg);
    }
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
fn fused_stages(shared: &Fused, works: &mut Vec<Work>, slow_events: &mut Vec<PersonEvent>, lap: &mut dyn FnMut(&str)) -> bool {
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
    par::run(works, plan.click_parts, |_, work, range| {
        let group = shared.grouped;
        if group.total(range) == 0 {
            return;
        }
        let first = range << plan.click_shift;
        for person in first..(first + (1 << plan.click_shift)).min(plan.persons as usize) {
            let at = group.range(person);
            if !at.is_empty() && !shared.fit(person) {
                for i in at {
                    // SAFETY: an unfit person's clicks are read here only, and written nowhere.
                    let (click, alias) = unsafe { (*group.clicks.0.add(i), *group.aliases.0.add(i)) };
                    let u = (person as u32) << ALIAS_BITS | u32::from(alias);
                    work.slow_clicks.push(ClickRec { user: shared.links.person(u), click: click.id, ts: click.ts, campaign: click.campaign });
                }
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
    grouped: &Grouped,
    click_aliases: &[u64],
    flags: &mut [u8],
    works: &mut Vec<Work>,
    slow_events: &mut Vec<PersonEvent>,
    free_early: bool,
    timing: bool,
    lap: &mut dyn FnMut(&str),
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

    // A partition's runs per task, with its purchases; then the click ranges, for the clicks
    // of the classic lane's persons.
    let run_tasks = placed.len();
    let (table, table_users) = (&times[..], &users[..]);
    par::run(works, run_tasks + plan.click_parts, |_, work, task| {
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
                    let at = grouped.range(person);
                    // SAFETY: a fit person has one run, so only this task takes its clicks,
                    // and the click tasks below read only the clicks of unfit persons.
                    let clicks: &mut [Click] = unsafe { std::slice::from_raw_parts_mut(grouped.clicks.0.add(at.start), at.len()) };
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
        } else if grouped.total(task - run_tasks) > 0 {
            let first = (task - run_tasks) << plan.click_shift;
            for person in first..(first + (1 << plan.click_shift)).min(plan.persons as usize) {
                let at = grouped.range(person);
                if !at.is_empty() && !fit(person) {
                    for i in at {
                        // SAFETY: an unfit person's clicks are read here only, and written nowhere.
                        let (click, alias) = unsafe { (*grouped.clicks.0.add(i), *grouped.aliases.0.add(i)) };
                        let u = (person as u32) << ALIAS_BITS | u32::from(alias);
                        work.slow_clicks.push(ClickRec { user: links.person(u), click: click.id, ts: click.ts, campaign: click.campaign });
                    }
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
        for piece in out.events.pieces(part) {
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
        }
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

/// `group_clicks` for a range with a large share of the clicks, by all workers together: each
/// counts the clicks of its own list per person, the counts become each worker's positions
/// (a person's clicks from worker 0's list first, as `group_clicks` leaves them), and each
/// scatters its own list. The range has clicks.
///
/// # Safety
/// As `group_clicks`, and worker `w` of `works` is the one that parsed `parsed[w]`.
#[allow(clippy::too_many_arguments)]
unsafe fn group_clicks_wide(
    range: usize,
    plan: &PosPlan,
    parsed: &[PosOut],
    click_aliases: Shared<u64>,
    flags: Shared<u8>,
    ends: Shared<u32>,
    grouped: &Grouped,
    works: &mut [Work],
) {
    assert_eq!(parsed.len(), works.len());
    let first = range << plan.click_shift;
    let persons = (1usize << plan.click_shift).min(plan.persons as usize - first);
    par::each(works, |worker, work| {
        let Share { counts, masks, over } = &mut work.share;
        counts.clear();
        counts.resize(persons, 0);
        masks.clear();
        masks.resize(persons, 0);
        over.clear();
        over.resize(persons, 0);
        for piece in parsed[worker].clicks.pieces(range) {
            for click in piece {
                let (local, alias) = ((click.u() >> ALIAS_BITS) as usize - first, click.u() & ALIAS_MASK);
                counts[local] += 1;
                masks[local] |= 1 << (alias & (MASK_ALIASES - 1));
                over[local] |= u8::from(alias >= MASK_ALIASES);
            }
        }
    });
    // SAFETY: the caller's contract; the three arrays hold `plan.persons` entries.
    let (click_aliases, flags, ends) = unsafe {
        (
            std::slice::from_raw_parts_mut(click_aliases.0.add(first), persons),
            std::slice::from_raw_parts_mut(flags.0.add(first), persons),
            std::slice::from_raw_parts_mut(ends.0.add(first), persons),
        )
    };
    let mut at = grouped.bases[range];
    for local in 0..persons {
        for work in works.iter_mut() {
            let share = &mut work.share;
            click_aliases[local] |= share.masks[local];
            if share.over[local] != 0 {
                flags[local] |= UNFIT;
            }
            (share.counts[local], at) = (at, at + share.counts[local]);
        }
        ends[local] = at;
    }
    debug_assert_eq!(at, grouped.bases[range + 1]);
    let (click_room, alias_room) = (grouped.clicks, grouped.aliases);
    par::each(works, |worker, work| {
        // The wrappers whole, not their pointers: those are what may cross threads.
        let (click_room, alias_room) = (click_room, alias_room);
        let counts = &mut work.share.counts;
        for piece in parsed[worker].clicks.pieces(range) {
            for click in piece {
                let at = &mut counts[(click.u() >> ALIAS_BITS) as usize - first];
                // SAFETY: a worker's positions for a person are its own stretch of the
                // person's, by the counts of these same clicks: all workers' together are a
                // permutation of the range's positions, inside the reserved room.
                unsafe {
                    click_room.0.add(*at as usize).write(Click { ts: click.ts(), campaign: click.campaign(), id: click.click });
                    alias_room.0.add(*at as usize).write((click.u() & ALIAS_MASK) as u16);
                }
                *at += 1;
            }
        }
    });
}

#[cold]
#[inline(never)]
fn odd_user(row: &PosEvent, worker: usize, odd: &mut Vec<(u32, u32, u32, u32)>) {
    odd.push((u32::from(row.slot()), row.ingest(), worker as u32, row.u()));
}

/// Groups click range `range` by person with a counting scatter into its stretch of
/// `grouped`'s arrays, and notes each person's click aliases. `ends` is `grouped.ends`, to
/// write: the range's entries are zero on entry and each person's end on return.
///
/// # Safety
/// No other thread touches the entries of `click_aliases`, `flags` and `ends` for persons
/// `range << click_shift .. (range + 1) << click_shift`, nor the range's positions of the
/// clicks; `parsed` is what `grouped` was made for.
unsafe fn group_clicks(range: usize, plan: &PosPlan, parsed: &[PosOut], click_aliases: Shared<u64>, flags: Shared<u8>, ends: Shared<u32>, grouped: &Grouped) {
    let first = range << plan.click_shift;
    let persons = (1usize << plan.click_shift).min(plan.persons as usize - first);
    // SAFETY: the caller's contract; the three arrays hold `plan.persons` entries.
    let (click_aliases, flags, ends) = unsafe {
        (
            std::slice::from_raw_parts_mut(click_aliases.0.add(first), persons),
            std::slice::from_raw_parts_mut(flags.0.add(first), persons),
            std::slice::from_raw_parts_mut(ends.0.add(first), persons),
        )
    };
    for out in parsed {
        for piece in out.clicks.pieces(range) {
            for click in piece {
                let (local, alias) = ((click.u() >> ALIAS_BITS) as usize - first, click.u() & ALIAS_MASK);
                ends[local] += 1;
                click_aliases[local] |= 1 << (alias & (MASK_ALIASES - 1));
                if alias >= MASK_ALIASES {
                    flags[local] |= UNFIT;
                }
            }
        }
    }
    // Each person's first position; the scatter moves it to one past the last.
    let mut at = grouped.bases[range];
    for end in ends.iter_mut() {
        (*end, at) = (at, at + *end);
    }
    debug_assert_eq!(at, grouped.bases[range + 1]);
    let (click_room, alias_room) = (grouped.clicks.0, grouped.aliases.0);
    for out in parsed {
        for piece in out.clicks.pieces(range) {
            for click in piece {
                let at = &mut ends[(click.u() >> ALIAS_BITS) as usize - first];
                // SAFETY: the counts above are of these same clicks, so the positions are a
                // permutation of the range's, inside the room reserved for all the clicks.
                unsafe {
                    click_room.add(*at as usize).write(Click { ts: click.ts(), campaign: click.campaign(), id: click.click });
                    alias_room.add(*at as usize).write((click.u() & ALIAS_MASK) as u16);
                }
                *at += 1;
            }
        }
    }
}
