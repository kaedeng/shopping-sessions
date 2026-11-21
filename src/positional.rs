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

use ingest::Ingest;
use ingest::pos::{PosOut, PosPlan};
use sessions::{Agg, Click};
use types::hash::person_hash;
use types::par;
use types::pos::{POS_ODD_USER, POS_VALID, PosEvent};
use types::record::{ClickRec, PersonEvent, VALID};
use types::scramble::{self, ALIAS_BITS, ALIAS_MASK};
use types::{Ctx, Id};

/// Aliases a person's masks tell apart; a higher alias sends the person down the classic lane.
const MASK_ALIASES: u32 = 64;
/// Runs per sessions task.
const RUNS_PER_TASK: usize = 2048;
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
    fn build(pairs: &[[Id; 2]], plan: &PosPlan) -> Links {
        let persons = plan.persons as usize;
        let number = |id: Id| {
            scramble::canonical_packed(id, b'u')
                .map(|scrambled| scramble::number(scrambled, plan.user_key))
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
        for &pair in pairs {
            if pair.contains(&0) {
                continue;
            }
            let [x, y] = pair.map(number);
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

/// One event partition placed.
#[derive(Default)]
struct Placed {
    runs: Vec<Run>,
    /// `(event number, pay)` of the kept purchases, ascending.
    pays: Vec<(u32, u32)>,
}

/// One click range grouped by person.
struct Grouped {
    /// One past each person's last click in `clicks`, by person number less the range's first.
    ends: Vec<u32>,
    clicks: Shared<Click>,
    /// Each click's alias, beside `clicks`.
    aliases: Vec<u16>,
    /// Owns `clicks`.
    _owner: Vec<Click>,
}

impl Grouped {
    /// The positions of a person's clicks (`local`: person number less the range's first).
    fn range(&self, local: usize) -> std::ops::Range<usize> {
        let start = if local == 0 { 0 } else { self.ends[local - 1] as usize };
        start..self.ends[local] as usize
    }
}

/// A worker's buffers for the placing stage.
#[derive(Default)]
struct Placing {
    /// The partition's slots while its rows are placed: the kept copy of each event number.
    kept: Vec<PosEvent>,
    /// Kept-copy candidates whose user is not a number: (slot, ingest, worker, index).
    odd: Vec<(u32, u32, u32, u32)>,
    slow_events: Vec<PersonEvent>,
}

/// A worker's state for the sessions stage.
struct Work {
    agg: Agg,
    slow_events: Vec<PersonEvent>,
    slow_clicks: Vec<ClickRec>,
}

/// Runs the whole pipeline after the inputs stage. `lap` marks a stage for `SS_TIMING`.
pub fn run(ctx: &Ctx, ingest: &Ingest, plan: &PosPlan, out: &Path, parallel_alloc: bool, lap: &mut dyn FnMut(&str)) -> Result<(), String> {
    let free_early = types::mem::free_early();

    // Stage 1. Task 0 reads the links; the others parse.
    let mut parsed: Vec<PosOut> = par::init(ctx.workers, parallel_alloc, |_| ingest.pos_out(ctx, plan));
    let links = OnceLock::new();
    par::run(&mut parsed, ingest.tasks() + 1, |_, out, task| match task {
        0 => drop(links.set(Links::build(&ingest.links(), plan))),
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

    // Stage 2. Each event partition placed into its stretch of the table; each click range
    // grouped by person.
    let (events, persons) = (plan.events(), plan.persons as usize);
    // Zeroed lazily by the kernel: a slot nobody writes reads 0, "no event" (a valid `ts` is
    // at least `event_lo`, 7 days).
    let (mut times, mut users) = (vec![0u32; events], vec![0u32; events]);
    let mut click_aliases = vec![0u64; persons];
    let mut flags = vec![0u8; persons];
    let (times_at, users_at) = (Shared(times.as_mut_ptr()), Shared(users.as_mut_ptr()));
    let (click_aliases_at, flags_at) = (Shared(click_aliases.as_mut_ptr()), Shared(flags.as_mut_ptr()));
    let placed: Vec<OnceLock<Placed>> = (0..plan.parts).map(|_| OnceLock::new()).collect();
    let grouped: Vec<OnceLock<Grouped>> = (0..plan.click_parts).map(|_| OnceLock::new()).collect();
    let mut placing: Vec<Placing> = par::init(ctx.workers, parallel_alloc, |_| Placing::default());
    par::run(&mut placing, plan.parts + plan.click_parts, |_, work, task| {
        if task < plan.parts {
            // SAFETY: partition `task` owns event numbers `task << shift ..`, and only this
            // task writes them.
            if let Some(result) = unsafe { place(task, plan, &parsed, links, times_at, users_at, work) } {
                drop(placed[task].set(result));
            }
        } else {
            let range = task - plan.parts;
            // SAFETY: range `range` owns its persons' entries of both arrays in this stage.
            if let Some(result) = unsafe { group_clicks(range, plan, &parsed, click_aliases_at, flags_at) } {
                drop(grouped[range].set(result));
            }
        }
    });
    for work in &mut placing {
        slow_events.append(&mut work.slow_events);
    }
    // The clicks whose user is not a number: classic lane, by the general table.
    let mut slow_clicks: Vec<ClickRec> = Vec::new();
    for out in &parsed {
        slow_clicks.extend(out.odd_clicks.iter().map(|click| ClickRec { user: links.general.person(click.user), ..*click }));
    }
    if free_early {
        par::each(&mut parsed, |_, out| out.release());
    }
    lap("place");

    // The runs of the whole table in order, a run that crosses partitions joined, and what
    // they say about each person.
    let mut runs: Vec<Run> = Vec::with_capacity(placed.iter().filter_map(OnceLock::get).map(|part| part.runs.len()).sum());
    let mut pays: Vec<(u32, u32)> = Vec::with_capacity(placed.iter().filter_map(OnceLock::get).map(|part| part.pays.len()).sum());
    for part in placed.iter().filter_map(OnceLock::get) {
        pays.extend_from_slice(&part.pays);
        let mut rest = &part.runs[..];
        if let (Some(last), Some(next)) = (runs.last_mut(), rest.first())
            && last.person == next.person
        {
            last.unfit |= next.unfit | (next.first_ts < last.last_ts);
            (last.end, last.last_ts, last.aliases) = (next.end, next.last_ts, last.aliases | next.aliases);
            rest = &rest[1..];
        }
        runs.extend_from_slice(rest);
    }
    drop(placed);
    let mut event_aliases = vec![0u64; persons];
    for run in &runs {
        let person = run.person as usize;
        event_aliases[person] |= run.aliases;
        // A second run of a person: its events are not one stretch.
        flags[person] |= if flags[person] & SEEN != 0 || run.unfit { UNFIT | SEEN } else { SEEN };
    }
    extras.sort_unstable_by_key(|extra| extra.person);
    for extra in &extras {
        let (person, alias) = (extra.person as usize, extra.u & ALIAS_MASK);
        event_aliases[person] |= 1 << (alias & (MASK_ALIASES - 1));
        // No run to join: the person's events are these alone, and the classic lane takes them.
        flags[person] |= if flags[person] & SEEN == 0 || alias >= MASK_ALIASES { UNFIT | EXTRA } else { EXTRA };
    }
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
    for extra in &extras {
        if !fit(extra.person as usize) {
            slow_events.push(PersonEvent { person: links.person(extra.u), ts: extra.ts, pay: extra.pay });
        }
    }
    lap("runs");

    // Stage 3. Runs in table order, a fixed count per task; then the click ranges, for the
    // clicks of the classic lane's persons.
    let run_tasks = runs.len().div_ceil(RUNS_PER_TASK);
    let mut works: Vec<Work> =
        par::init(ctx.workers, parallel_alloc, |_| Work { agg: Agg::new(ctx), slow_events: Vec::new(), slow_clicks: Vec::new() });
    let (table, table_users) = (&times[..], &users[..]);
    par::run(&mut works, run_tasks + plan.click_parts, |_, work, task| {
        let (times, users) = (table, table_users);
        if task < run_tasks {
            let chunk = &runs[task * RUNS_PER_TASK..runs.len().min((task + 1) * RUNS_PER_TASK)];
            let mut next_pay = pays.partition_point(|&(number, _)| number < chunk[0].first);
            for run in chunk {
                let person = run.person as usize;
                let stretch = run.first as usize..run.end as usize;
                // Purchases of events that left the table (none in practice) or of a skipped run.
                while pays.get(next_pay).is_some_and(|&(number, _)| number < run.first) {
                    next_pay += 1;
                }
                if fit(person) {
                    let name = match links.state[person] {
                        ONE => links.name[person],
                        _ => scramble::id_of(u64::from(users[stretch.start]), plan.user_key, b'u'),
                    };
                    let range = person >> plan.click_shift;
                    let clicks: &mut [Click] = match grouped[range].get() {
                        // SAFETY: a fit person has one run, so only this task takes its clicks,
                        // and the click tasks below read only the clicks of unfit persons.
                        Some(group) => unsafe {
                            let at = group.range(person - (range << plan.click_shift));
                            std::slice::from_raw_parts_mut(group.clicks.0.add(at.start), at.len())
                        },
                        None => &mut [],
                    };
                    if flags[person] & EXTRA == 0 {
                        sessions::positional::person(ctx, name, &times[stretch], run.first, &pays, &mut next_pay, clicks, &mut work.agg);
                    } else {
                        // The table's events, which are in time order, with the extras
                        // merged in: a few rows, sorted alone. (Sorting them all together was
                        // 1.4% of a run's instructions at 20M: the whales have extras.)
                        let from = extras.partition_point(|extra| (extra.person as usize) < person);
                        let mut own: Vec<(u32, u32)> =
                            extras[from..].iter().take_while(|extra| extra.person as usize == person).map(|extra| (extra.ts, extra.pay)).collect();
                        own.sort_unstable_by_key(|&(ts, _)| ts);
                        let mut next_own = 0;
                        let mut all: Vec<(u32, u32)> = Vec::with_capacity(stretch.len() + own.len());
                        for number in stretch {
                            if times[number] != 0 {
                                let pay = match pays.get(next_pay) {
                                    Some(&(at, pay)) if at as usize == number => {
                                        next_pay += 1;
                                        pay
                                    }
                                    _ => 0,
                                };
                                while own.get(next_own).is_some_and(|&(ts, _)| ts <= times[number]) {
                                    all.push(own[next_own]);
                                    next_own += 1;
                                }
                                all.push((times[number], pay));
                            }
                        }
                        all.extend_from_slice(&own[next_own..]);
                        sessions::positional::person_pairs(ctx, name, &all, clicks, &mut work.agg);
                    }
                } else {
                    for number in stretch {
                        if times[number] != 0 {
                            let pay = match pays.get(next_pay) {
                                Some(&(at, pay)) if at as usize == number => {
                                    next_pay += 1;
                                    pay
                                }
                                _ => 0,
                            };
                            work.slow_events.push(PersonEvent { person: links.person(users[number]), ts: times[number], pay });
                        }
                    }
                }
            }
        } else if let Some(group) = grouped[task - run_tasks].get() {
            let first = (task - run_tasks) << plan.click_shift;
            for local in 0..group.ends.len() {
                let at = group.range(local);
                if !at.is_empty() && !fit(first + local) {
                    for i in at {
                        // SAFETY: an unfit person's clicks are read here only, and written nowhere.
                        let click = unsafe { *group.clicks.0.add(i) };
                        let u = ((first + local) as u32) << ALIAS_BITS | u32::from(group.aliases[i]);
                        work.slow_clicks.push(ClickRec { user: links.person(u), click: click.id, ts: click.ts, campaign: click.campaign });
                    }
                }
            }
        }
    });
    lap("sessions");

    // The classic lane: its events and clicks by person hash into a few buckets, each through
    // the classic sessions code, into the same counters.
    for work in &mut works {
        slow_events.append(&mut work.slow_events);
        slow_clicks.append(&mut work.slow_clicks);
    }
    let slow = (slow_events.len(), slow_clicks.len());
    if !slow_events.is_empty() {
        let bucket_of = |person: Id| (person_hash(person) >> (64 - SLOW_BITS)) as usize;
        let mut event_buckets: Vec<Vec<PersonEvent>> = vec![Vec::new(); 1 << SLOW_BITS];
        let mut click_buckets: Vec<Vec<ClickRec>> = vec![Vec::new(); 1 << SLOW_BITS];
        for event in slow_events {
            event_buckets[bucket_of(event.person)].push(event);
        }
        for click in slow_clicks {
            click_buckets[bucket_of(click.user)].push(click);
        }
        par::run(&mut works, 1 << SLOW_BITS, |_, work, bucket| {
            if !event_buckets[bucket].is_empty() {
                sessions::bucket(ctx, std::iter::once(&event_buckets[bucket][..]), std::iter::once(&click_buckets[bucket][..]), &mut work.agg);
            }
        });
    }
    lap("classic");

    let (user_folder, campaign_folder) = (out.join("user_daily"), out.join("campaign_daily"));
    for folder in [&user_folder, &campaign_folder] {
        std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    }
    let failure = OnceLock::new();
    par::each(&mut works, |worker, work| {
        let rows = std::mem::take(&mut work.agg.user_daily);
        if let Err(error) = output::user_daily(&user_folder, worker, ctx, rows) {
            drop(failure.set(error));
        }
    });
    if let Some(error) = failure.into_inner() {
        return Err(error);
    }
    let aggs: Vec<Agg> = works.into_iter().map(|work| work.agg).collect();
    output::campaign_daily(&campaign_folder, ctx, sessions::campaign_daily(ctx, &aggs))?;
    lap("output");
    if std::env::var_os("SS_TIMING").is_some() {
        let unfit = runs.iter().filter(|run| !fit(run.person as usize)).count();
        // Why, in events: flagged (split, out of order, wide alias, outside the numbering),
        // mixed links, an ID outside the one component, several unlinked IDs.
        let mut why = [0u64; 4];
        for run in runs.iter().filter(|run| !fit(run.person as usize)) {
            let person = run.person as usize;
            let reason = if flags[person] & UNFIT != 0 {
                0
            } else {
                match links.state[person] {
                    MIXED => 1,
                    ONE => 2,
                    _ => 3,
                }
            };
            why[reason] += u64::from(run.end - run.first);
        }
        eprintln!("unfit slots by reason: flagged {}, mixed links {}, outside the component {}, unlinked IDs {}", why[0], why[1], why[2], why[3]);
        eprintln!(
            "positional: {} runs, {unfit} in the classic lane with {} events and {} clicks; {odd_rows} rows outside the numbering",
            runs.len(),
            slow.0,
            slow.1
        );
    }
    // The exit path skips destructors (`fast-exit`), and these are large.
    std::mem::forget((times, users, runs, pays, grouped, aggs));
    Ok(())
}

/// Writes partition `part`'s rows into its stretch of the table: a slot keeps the copy with the
/// smallest `ingest` (copies that tie are identical, rules "Events" step 2), its `ts` when
/// that copy is valid and 0 when it is not, so an invalid kept copy leaves no event. Then
/// lists the kept purchases and the runs. `None` for a partition without rows.
///
/// # Safety
/// No other thread touches event numbers `part << shift .. (part + 1) << shift` of the arrays.
unsafe fn place(
    part: usize,
    plan: &PosPlan,
    parsed: &[PosOut],
    links: &Links,
    times: Shared<u32>,
    users: Shared<u32>,
    work: &mut Placing,
) -> Option<Placed> {
    if parsed.iter().all(|out| out.events.len_of(part) == 0) {
        return None;
    }
    let (slots, base) = (1usize << plan.shift, part << plan.shift);
    // SAFETY: the caller's contract; the arrays hold `parts << shift` slots.
    let (times, users) = unsafe { (std::slice::from_raw_parts_mut(times.0.add(base), slots), std::slice::from_raw_parts_mut(users.0.add(base), slots)) };
    let Placing { kept, odd, slow_events } = work;
    // One 16-byte slot per event number while placing: a row reads and writes one cache line
    // of a table that fits L2. Three arrays (ingest, time, user) were three lines a row, and
    // those accesses were two thirds of this stage.
    kept.clear();
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
    let mut result = Placed::default();
    for out in parsed {
        for piece in out.pays.pieces(part) {
            for pay in piece {
                let slot = pay.slot as usize & (slots - 1);
                // The kept copy has this `ingest`, so it is this row or its twin.
                if kept[slot].ingest() == pay.ingest && valid(&kept[slot]) {
                    result.pays.push(((base + slot) as u32, pay.pay));
                }
            }
        }
    }
    result.pays.sort_unstable();
    result.pays.dedup_by_key(|&mut (number, _)| number);
    // A kept copy whose user is not a number leaves the table for the classic lane.
    if !odd.is_empty() {
        for &(slot, row_ingest, worker, index) in odd.iter() {
            let row = kept[slot as usize];
            if row.flags() & POS_ODD_USER != 0 && row.ingest() == row_ingest && valid(&row) {
                let number = base as u32 + slot;
                let pay = result.pays.binary_search_by_key(&number, |&(at, _)| at).map_or(0, |at| result.pays[at].1);
                let person = links.general.person(parsed[worker as usize].odd_users[index as usize]);
                slow_events.push(PersonEvent { person, ts: row.ts(), pay });
                kept[slot as usize] = PosEvent::new(0, 0, row_ingest, slot as u16, 0);
            }
        }
        result.pays.retain(|&(number, _)| valid(&kept[number as usize - base]));
    }
    // The table's two columns: the `ts` of a valid kept copy, 0 for anything else, and the user.
    for (slot, row) in kept.iter().enumerate() {
        times[slot] = if valid(row) { row.ts() } else { 0 };
        users[slot] = row.u();
    }

    // The runs: one pass with the run's state in registers; a person's first event opens one.
    let mut slot = 0;
    loop {
        while slot < slots && times[slot] == 0 {
            slot += 1;
        }
        if slot == slots {
            break;
        }
        let (person, first, first_ts) = (users[slot] >> ALIAS_BITS, slot, times[slot]);
        let (mut end, mut last_ts, mut aliases, mut unfit) = (slot, first_ts, 0u64, false);
        while slot < slots {
            // SAFETY: `slot < slots`, the length of both.
            let (ts, u) = unsafe { (*times.get_unchecked(slot), *users.get_unchecked(slot)) };
            if ts != 0 {
                if u >> ALIAS_BITS != person {
                    break;
                }
                let alias = u & ALIAS_MASK;
                unfit |= (ts < last_ts) | (alias >= MASK_ALIASES);
                aliases |= 1 << (alias & (MASK_ALIASES - 1));
                (last_ts, end) = (ts, slot + 1);
            }
            slot += 1;
        }
        result.runs.push(Run { person, first: (base + first) as u32, end: (base + end) as u32, first_ts, last_ts, aliases, unfit });
    }
    Some(result)
}

#[cold]
#[inline(never)]
fn odd_user(row: &PosEvent, worker: usize, odd: &mut Vec<(u32, u32, u32, u32)>) {
    odd.push((u32::from(row.slot()), row.ingest(), worker as u32, row.u()));
}

/// Groups click range `range` by person with a counting scatter, and notes each person's click
/// aliases. `None` for a range without clicks.
///
/// # Safety
/// No other thread touches the entries of `click_aliases` and `flags` for persons
/// `range << click_shift .. (range + 1) << click_shift`.
unsafe fn group_clicks(range: usize, plan: &PosPlan, parsed: &[PosOut], click_aliases: Shared<u64>, flags: Shared<u8>) -> Option<Grouped> {
    let total: usize = parsed.iter().map(|out| out.clicks.len_of(range)).sum();
    if total == 0 {
        return None;
    }
    let first = range << plan.click_shift;
    let persons = (1usize << plan.click_shift).min(plan.persons as usize - first);
    // SAFETY: the caller's contract; both arrays hold `plan.persons` entries.
    let (click_aliases, flags) =
        unsafe { (std::slice::from_raw_parts_mut(click_aliases.0.add(first), persons), std::slice::from_raw_parts_mut(flags.0.add(first), persons)) };
    let mut ends = vec![0u32; persons + 1];
    for out in parsed {
        for piece in out.clicks.pieces(range) {
            for click in piece {
                let (local, alias) = ((click.u() >> ALIAS_BITS) as usize - first, click.u() & ALIAS_MASK);
                ends[local + 1] += 1;
                click_aliases[local] |= 1 << (alias & (MASK_ALIASES - 1));
                if alias >= MASK_ALIASES {
                    flags[local] |= UNFIT;
                }
            }
        }
    }
    for local in 1..=persons {
        ends[local] += ends[local - 1];
    }
    // `ends[local]` is the person's first position, and the scatter moves it to one past the last.
    let mut clicks: Vec<Click> = vec![Click::default(); total];
    let mut aliases = vec![0u16; total];
    for out in parsed {
        for piece in out.clicks.pieces(range) {
            for click in piece {
                let at = &mut ends[(click.u() >> ALIAS_BITS) as usize - first];
                clicks[*at as usize] = Click { ts: click.ts(), campaign: click.campaign(), id: click.click };
                aliases[*at as usize] = (click.u() & ALIAS_MASK) as u16;
                *at += 1;
            }
        }
    }
    ends.truncate(persons);
    Some(Grouped { ends, clicks: Shared(clicks.as_mut_ptr()), aliases, _owner: clicks })
}
