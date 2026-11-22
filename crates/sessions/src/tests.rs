//! A brute-force reference (rules, Events 5-7 and Clicks 4-7, straight from the text) against
//! the bucket pipeline on random buckets, and the fixtures of this stage by hand.

use std::collections::{HashMap, HashSet};

use types::hash::person_hash;
use types::id::{self, Id};
use types::record::{ClickRec, PAY_CATEGORY_SHIFT, PersonEvent};
use types::rows::NO_CATEGORY;
use types::time::{DAY, WEEK, civil_from_days, parse_f1};
use types::Ctx;

use crate::{Agg, bucket, campaign_daily};

type UserRow = (Id, i32, i64, i64, i64, u8);
type CampaignRow = (u32, i32, i64, i64, i64, i64);
/// One bucket: its event pieces and click pieces.
type Bucket = (Vec<Vec<PersonEvent>>, Vec<Vec<ClickRec>>);
/// (person, day) -> sessions, events, revenue, summed price per category.
type UserSums = HashMap<(Id, i32), (i64, i64, i64, HashMap<u8, i64>)>;

const GAP: u32 = 1_800_000;

fn reference(ctx: &Ctx, events: &[PersonEvent], clicks: &[ClickRec]) -> (Vec<UserRow>, Vec<CampaignRow>) {
    let mut by_person: HashMap<Id, Vec<PersonEvent>> = HashMap::new();
    for &event in events {
        by_person.entry(event.person).or_default().push(event);
    }
    let mut users: UserSums = HashMap::new();
    let mut campaigns: HashMap<(u32, i32), [i64; 4]> = HashMap::new();
    for (&person, rows) in &mut by_person {
        rows.sort_by_key(|event| event.ts);
        let mut sessions: Vec<Vec<PersonEvent>> = Vec::new();
        for &event in rows.iter() {
            match sessions.last_mut() {
                Some(session) if event.ts - session.last().unwrap().ts <= GAP => session.push(event),
                _ => sessions.push(vec![event]),
            }
        }
        for session in sessions {
            let start = session[0].ts;
            let day = (ctx.origin + i64::from(start)).div_euclid(DAY) as i32;
            let window: Vec<&ClickRec> = clicks
                .iter()
                .filter(|click| click.user == person && click.ts <= start && i64::from(start - click.ts) <= WEEK)
                .collect();
            let mut credit: Option<&ClickRec> = None;
            for &click in &window {
                if credit.is_none_or(|best| click.ts > best.ts || (click.ts == best.ts && id::less(click.click, best.click))) {
                    credit = Some(click);
                }
            }
            let revenue: i64 = session.iter().map(|event| i64::from(event.pay & 0xFF_FFFF)).sum();
            let user = users.entry((person, day)).or_default();
            user.0 += 1;
            user.1 += session.len() as i64;
            user.2 += revenue;
            for event in session.iter().filter(|event| event.pay != 0) {
                *user.3.entry((event.pay >> PAY_CATEGORY_SHIFT) as u8).or_default() += i64::from(event.pay & 0xFF_FFFF);
            }
            let cell = campaigns.entry((credit.map_or(ctx.organic(), |click| click.campaign), day)).or_default();
            cell[0] += 1;
            cell[1] += i64::from(revenue > 0);
            cell[2] += revenue;
            for campaign in window.iter().map(|click| click.campaign).collect::<HashSet<_>>() {
                campaigns.entry((campaign, day)).or_default()[3] += 1;
            }
        }
    }
    let mut user_rows: Vec<UserRow> = users
        .into_iter()
        .map(|((person, day), (sessions, events, revenue, by_category))| {
            let mut top = (NO_CATEGORY, 0);
            let mut categories: Vec<_> = by_category.into_iter().collect();
            categories.sort();
            for (category, sum) in categories {
                if sum > top.1 {
                    top = (category, sum);
                }
            }
            (person, day, sessions, events, revenue, top.0)
        })
        .collect();
    let mut campaign_rows: Vec<CampaignRow> =
        campaigns.into_iter().map(|((campaign, day), [s, p, r, a])| (campaign, day, s, p, r, a)).collect();
    user_rows.sort();
    campaign_rows.sort();
    (user_rows, campaign_rows)
}

/// Runs `buckets` (each a list of event pieces and click pieces) over `workers` aggs.
fn pipeline(
    ctx: &Ctx,
    buckets: &[Bucket],
    workers: usize,
    worker_of: impl Fn(usize) -> usize,
) -> (Vec<UserRow>, Vec<CampaignRow>) {
    let mut aggs: Vec<Agg> = (0..workers).map(|_| Agg::new(ctx)).collect();
    for (at, (events, clicks)) in buckets.iter().enumerate() {
        bucket(ctx, events.iter().map(Vec::as_slice), clicks.iter().map(Vec::as_slice), &mut aggs[worker_of(at)]);
    }
    let mut user_rows = Vec::new();
    for agg in &aggs {
        let rows = &agg.user_daily;
        for i in 0..rows.person.len() {
            user_rows.push((rows.person[i], rows.day[i], rows.sessions[i], rows.events[i], rows.revenue_cents[i], rows.top_category[i]));
        }
    }
    let rows = campaign_daily(ctx, &aggs);
    let mut campaign_rows: Vec<CampaignRow> = (0..rows.campaign.len())
        .map(|i| {
            (rows.campaign[i], rows.day[i], rows.sessions[i], rows.purchasing_sessions[i], rows.revenue_cents[i], rows.assisted_sessions[i])
        })
        .collect();
    user_rows.sort();
    campaign_rows.sort();
    (user_rows, campaign_rows)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len() as u64) as usize]
    }
}

/// A unique ID: short ones pack, long ones take the slow path, so `less` crosses both.
fn fresh_id(rng: &mut Rng, prefix: u8, used: &mut HashSet<Vec<u8>>) -> Id {
    loop {
        let len = if rng.below(4) == 0 { 10 + rng.below(4) } else { 1 + rng.below(6) } as usize;
        let mut bytes = vec![prefix];
        bytes.extend((0..len).map(|_| b"0az_"[rng.below(4) as usize]));
        if used.insert(bytes.clone()) {
            return id::pack(&bytes);
        }
    }
}

#[test]
fn matches_the_reference_on_random_buckets() {
    let midnight = parse_f1(b"2026-10-01T00:00:00Z").unwrap();
    for seed in 1..=3_000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        // Some windows start mid-day, so the first and last day are partial.
        let start = midnight + rng.below(400) as i64 * DAY + if rng.below(2) == 0 { 0 } else { rng.below(DAY as u64) as i64 };
        let end = start + 1 + rng.below(20 * DAY as u64) as i64;
        let names: Vec<Vec<u8>> = (0..1 + rng.below(5)).map(|n| format!("cat{n}").into_bytes()).collect();
        let products: Vec<(Id, &[u8])> =
            names.iter().enumerate().map(|(n, name)| (id::pack(format!("p{n}").as_bytes()), name.as_slice())).collect();
        let campaign_names: Vec<(Vec<u8>, Vec<u8>)> =
            (0..1 + rng.below(6)).map(|n| (format!("c_{n}").into_bytes(), b"search".to_vec())).collect();
        let campaigns: Vec<(&[u8], &[u8])> = campaign_names.iter().map(|(c, h)| (c.as_slice(), h.as_slice())).collect();
        let ctx = Ctx::new(start, end, &products, &campaigns).unwrap();
        let real_campaigns = ctx.organic() as u64;
        let categories = ctx.categories.len() as u64;
        let midnights: Vec<u32> = (0..=ctx.n_days as i64)
            .map(|k| (i64::from(ctx.day0) + k) * DAY - ctx.origin)
            .filter(|&ts| ts >= i64::from(ctx.event_lo) && ts < i64::from(ctx.ts_end))
            .map(|ts| ts as u32)
            .collect();

        let mut used = HashSet::new();
        let persons: Vec<Id> = (0..1 + rng.below(10)).map(|_| fresh_id(&mut rng, b'u', &mut used)).collect();
        let (mut events, mut clicks) = (Vec::new(), Vec::new());
        for &person in &persons {
            let span = u64::from(ctx.ts_end - ctx.event_lo);
            let mut ts = u64::from(ctx.event_lo) + rng.below(span);
            let mut mine = Vec::new();
            for _ in 0..rng.below(30) {
                if rng.below(8) == 0 && !midnights.is_empty() {
                    // At or just before a midnight, so sessions cross it.
                    let midnight = u64::from(rng.pick(&midnights));
                    ts = midnight.saturating_sub(rng.pick(&[0, 1, 1_000, 600_000, 1_799_999]));
                } else {
                    let far = rng.below(20_000_000);
                    ts += rng.pick(&[0, 0, 1, 999, 60_000, 1_799_999, 1_800_000, 1_800_001, 3_600_000, 86_400_000, far]);
                }
                if ts < u64::from(ctx.event_lo) || ts >= u64::from(ctx.ts_end) {
                    ts = u64::from(ctx.event_lo) + rng.below(span);
                    continue;
                }
                let pay = if rng.below(4) == 0 {
                    let any = 1 + rng.below(10_000_000);
                    let price = rng.pick(&[1, 500, 500, 1_000, 10_000_000, any]) as u32;
                    price | (rng.below(categories) as u32) << PAY_CATEGORY_SHIFT
                } else {
                    0
                };
                let event = PersonEvent { person, ts: ts as u32, pay };
                events.push(event);
                mine.push(event.ts);
                // The same ts again, possibly another type.
                if rng.below(6) == 0 {
                    events.push(PersonEvent { pay: 0, ..event });
                    mine.push(event.ts);
                }
            }
            for _ in 0..rng.below(12) {
                let base = if mine.is_empty() || rng.below(5) == 0 { rng.below(u64::from(ctx.ts_end)) as i64 } else { i64::from(rng.pick(&mine)) };
                let (in_week, near, after) =
                    (rng.below(WEEK as u64) as i64, rng.below(4_000_000) as i64, rng.below(DAY as u64) as i64);
                let ts = base + rng.pick(&[0, 1, -1, -WEEK, -WEEK - 1, -WEEK + 1, -in_week, -near, after]);
                if ts < 0 || ts >= i64::from(ctx.ts_end) {
                    continue;
                }
                let click = ClickRec { user: person, click: fresh_id(&mut rng, b'k', &mut used), ts: ts as u32, campaign: rng.below(real_campaigns) as u32 };
                clicks.push(click);
                if rng.below(5) == 0 {
                    // Another click at the same millisecond.
                    clicks.push(ClickRec { click: fresh_id(&mut rng, b'k', &mut used), campaign: rng.below(real_campaigns) as u32, ..click });
                }
                if rng.below(6) == 0 {
                    clicks.push(click); // a copy, as click dedup is skipped
                }
            }
        }
        // Clicks of a person with no events at all.
        for _ in 0..rng.below(3) {
            let user = fresh_id(&mut rng, b'v', &mut used);
            clicks.push(ClickRec { user, click: fresh_id(&mut rng, b'k', &mut used), ts: rng.below(u64::from(ctx.ts_end)) as u32, campaign: 0 });
        }

        // Every row of a person in one bucket, each bucket in a few pieces (some empty), shuffled.
        let n_buckets = 1 + rng.below(3) as usize;
        let mut buckets: Vec<Bucket> = (0..n_buckets)
            .map(|_| {
                let pieces = 1 + rng.below(4) as usize;
                (vec![Vec::new(); pieces], vec![Vec::new(); pieces])
            })
            .collect();
        let bucket_of = |person: Id| (person_hash(person) >> 32) as usize % n_buckets;
        for &event in &events {
            let (pieces, _) = &mut buckets[bucket_of(event.person)];
            let piece = rng.below(pieces.len() as u64) as usize;
            let at = rng.below(pieces[piece].len() as u64 + 1) as usize;
            pieces[piece].insert(at, event);
        }
        for &click in &clicks {
            let (_, pieces) = &mut buckets[bucket_of(click.user)];
            let piece = rng.below(pieces.len() as u64) as usize;
            let at = rng.below(pieces[piece].len() as u64 + 1) as usize;
            pieces[piece].insert(at, click);
        }
        let workers = 1 + rng.below(3) as usize;
        let want = reference(&ctx, &events, &clicks);
        let got = pipeline(&ctx, &buckets, workers, |at| at % workers);
        assert_eq!(got.0, want.0, "user_daily, seed {seed}");
        assert_eq!(got.1, want.1, "campaign_daily, seed {seed}");

        // The same rows through the dense bucket, the persons as numbers from some base on,
        // in every combination of its two choices.
        #[cfg(feature = "pipe-positional")]
        {
            let mut everyone: Vec<Id> = events.iter().map(|event| event.person).chain(clicks.iter().map(|click| click.user)).collect();
            everyone.sort_unstable();
            everyone.dedup();
            // Some numbers without a person between them, and room past the last.
            let step = 1 + rng.below(3) as usize;
            let base = 1 + rng.below(5_000) as u32;
            let span = everyone.len() * step + rng.below(4) as usize + 1;
            let number = |person: Id| base + (everyone.binary_search(&person).unwrap() * step) as u32;
            let mut keys: Vec<u64> = events.iter().map(|event| u64::from(number(event.person)) << 32 | u64::from(event.ts)).collect();
            let mut paid: Vec<crate::DensePay> =
                events.iter().filter(|event| event.pay != 0).map(|event| crate::DensePay { key: u64::from(number(event.person)) << 32 | u64::from(event.ts), pay: event.pay, pad: 0 }).collect();
            let mut numbered: Vec<ClickRec> = clicks.iter().map(|click| ClickRec { user: Id::from(number(click.user)), ..*click }).collect();
            // In another order than they were made in.
            for at in (1..keys.len()).rev() {
                keys.swap(at, rng.below(at as u64 + 1) as usize);
            }
            for at in (1..paid.len()).rev() {
                paid.swap(at, rng.below(at as u64 + 1) as usize);
            }
            for at in (1..numbered.len()).rev() {
                numbered.swap(at, rng.below(at as u64 + 1) as usize);
            }
            let cut = rng.below(keys.len() as u64 + 1) as usize;
            // Every other click as the parse places it, where one fits that record.
            let (mut wide, mut placed): (Vec<ClickRec>, Vec<types::record::ClickN>) = (Vec::new(), Vec::new());
            for (at, click) in numbered.iter().enumerate() {
                let place = click.user as u32 - base;
                if at % 2 == 1 && place < 1 << 16 && click.campaign < 1 << 16 {
                    placed.push(types::record::ClickN::new(click.click, place, click.campaign, click.ts));
                } else {
                    wide.push(*click);
                }
            }
            // Every way, with one counter a person and with four, and the walk's times cut or sorted.
            let ways = [(true, true, false), (false, true, false), (true, false, false), (false, false, false), (true, true, true), (false, true, true), (true, false, true)];
            for ((net, wide_walk, narrow), four, time_cut) in ways.into_iter().flat_map(|way| [(way, false, false), (way, true, false), (way, false, true), (way, true, true)]) {
                let mut agg = Agg::new(&ctx);
                let (head, tail) = keys.split_at(cut);
                if four {
                    crate::partitioned::bucket_dense_as::<4>(&ctx, base, span, &|each| { each(head); each(tail) }, &|each| each(&paid), &|each| each(&wide), &|each| each(&placed), &mut agg, net, wide_walk, narrow, time_cut);
                } else {
                    crate::partitioned::bucket_dense_as::<1>(&ctx, base, span, &|each| { each(head); each(tail) }, &|each| each(&paid), &|each| each(&wide), &|each| each(&placed), &mut agg, net, wide_walk, narrow, time_cut);
                }
                // The rows of either table: the walk's are narrow.
                let walked = agg.user_narrow.widen(ctx.day0);
                let mut user_rows: Vec<UserRow> = [&agg.user_daily, &walked]
                    .into_iter()
                    .flat_map(|rows| (0..rows.person.len()).map(move |i| (rows.person[i], rows.day[i], rows.sessions[i], rows.events[i], rows.revenue_cents[i], rows.top_category[i])))
                    .map(|(person, day, sessions, events, revenue, top)| (everyone[(person as usize - base as usize) / step], day, sessions, events, revenue, top))
                    .collect();
                user_rows.sort();
                let rows = campaign_daily(&ctx, std::slice::from_ref(&agg));
                let mut campaign_rows: Vec<CampaignRow> = (0..rows.campaign.len())
                    .map(|i| (rows.campaign[i], rows.day[i], rows.sessions[i], rows.purchasing_sessions[i], rows.revenue_cents[i], rows.assisted_sessions[i]))
                    .collect();
                campaign_rows.sort();
                assert_eq!(user_rows, want.0, "dense user_daily, seed {seed}, sort {net}, walk {wide_walk}, narrow {narrow}, four {four}, cut {time_cut}");
                assert_eq!(campaign_rows, want.1, "dense campaign_daily, seed {seed}, sort {net}, walk {wide_walk}, narrow {narrow}, four {four}, cut {time_cut}");
            }
        }
    }
}

#[test]
fn grows_the_person_table() {
    let start = parse_f1(b"2026-10-01T00:00:00Z").unwrap();
    let campaigns = [(b"c_1".as_slice(), b"search".as_slice()), (b"c_2", b"social")];
    let ctx = Ctx::new(start, start + 14 * DAY, &[], &campaigns).unwrap();
    // One or two events per person: many more persons than `rows / 8` slots hold half full.
    let person = |n: u32| id::pack(format!("u{n}").as_bytes());
    let events: Vec<PersonEvent> =
        (0..6_000u32).map(|n| PersonEvent { person: person(n % 4_000), ts: ctx.event_lo + n * 200_000, pay: n % 7 }).collect();
    let clicks: Vec<ClickRec> = (0..4_000u32)
        .step_by(3)
        .map(|n| ClickRec { user: person(n), click: id::pack(format!("k{n}").as_bytes()), ts: ctx.event_lo + n * 200_000 - 5, campaign: n % 2 })
        .collect();
    let want = reference(&ctx, &events, &clicks);
    assert_eq!(pipeline(&ctx, &[(vec![events], vec![clicks])], 1, |_| 0), want);
}

/// The fixtures' surviving records by hand: events `(person, ts, category, price)` with an
/// empty category for a view or cart, clicks `(click_id, person, campaign, ts)`, links resolved.
/// `ts` is F1 or F3 as in the fixture's CSV.
type Events<'a> = &'a [(&'a str, &'a str, &'a str, u32)];
type Clicks<'a> = &'a [(&'a str, &'a str, &'a str, &'a str)];

fn fixture(case: &str, events: Events, clicks: Clicks) {
    let parse = |ts: &str| parse_f1(ts.as_bytes()).unwrap_or_else(|| ts.parse().unwrap());
    let products = [(id::pack(b"p1"), b"shoes".as_slice()), (id::pack(b"p2"), b"books")];
    let campaigns = [(b"c_1".as_slice(), b"search".as_slice()), (b"c_2", b"social")];
    let ctx = Ctx::new(parse("2026-10-01T00:00:00Z"), parse("2026-10-15T00:00:00Z"), &products, &campaigns).unwrap();
    let category = |name: &str| ctx.categories.iter().position(|c| c == name.as_bytes()).unwrap() as u32;
    let events: Vec<PersonEvent> = events
        .iter()
        .map(|&(person, ts, name, price)| PersonEvent {
            person: id::pack(person.as_bytes()),
            ts: (parse(ts) - ctx.origin) as u32,
            pay: if name.is_empty() { 0 } else { price | category(name) << PAY_CATEGORY_SHIFT },
        })
        .collect();
    let clicks: Vec<ClickRec> = clicks
        .iter()
        .map(|&(click, person, campaign, ts)| ClickRec {
            user: id::pack(person.as_bytes()),
            click: id::pack(click.as_bytes()),
            ts: (parse(ts) - ctx.origin) as u32,
            campaign: ctx.campaign_index.get(id::pack(campaign.as_bytes())).unwrap(),
        })
        .collect();
    let (users, campaign_rows) = pipeline(&ctx, &[(vec![events], vec![clicks])], 1, |_| 0);

    let date = |day: i32| {
        let (y, m, d) = civil_from_days(i64::from(day));
        format!("{y:04}-{m:02}-{d:02}")
    };
    let text = |bytes: &[u8]| String::from_utf8(bytes.to_vec()).unwrap();
    let mut got_users: Vec<String> = users
        .iter()
        .map(|&(person, day, sessions, events, revenue, top)| {
            let mut name = Vec::new();
            id::unpack(person, &mut name);
            let top = if top == NO_CATEGORY { String::new() } else { text(&ctx.categories[top as usize]) };
            format!("{},{},{sessions},{events},{revenue},{top}", text(&name), date(day))
        })
        .collect();
    let mut got_campaigns: Vec<String> = campaign_rows
        .iter()
        .map(|&(campaign, day, sessions, purchasing, revenue, assisted)| {
            let (id, channel) = &ctx.campaigns[campaign as usize];
            format!("{},{},{},{sessions},{purchasing},{revenue},{assisted}", text(id), text(channel), date(day))
        })
        .collect();
    let expected = |table: &str| {
        let path = format!("{}/../../fixtures/{case}/expected/{table}.csv", env!("CARGO_MANIFEST_DIR"));
        let mut lines: Vec<String> = std::fs::read_to_string(&path).unwrap().lines().skip(1).map(String::from).collect();
        lines.sort();
        lines
    };
    got_users.sort();
    got_campaigns.sort();
    assert_eq!(got_users, expected("user_daily"), "{case} user_daily");
    assert_eq!(got_campaigns, expected("campaign_daily"), "{case} campaign_daily");
}

#[test]
fn sessions_fixtures() {
    fixture("E1", &[("u1", "2026-10-03T10:00:00Z", "", 0), ("u1", "2026-10-03T10:30:00Z", "", 0)], &[]);
    fixture("E2", &[("u1", "1791021600000", "", 0), ("u1", "1791023400001", "", 0)], &[]);
    fixture("E3", &[("u1", "2026-10-03T23:50:00Z", "", 0), ("u1", "2026-10-04T00:10:00Z", "shoes", 1000)], &[]);
    fixture("E8", &[("u1", "2026-10-03T10:00:00Z", "shoes", 500), ("u1", "2026-10-03T10:05:00Z", "books", 500)], &[]);
    fixture("E9", &[("u1", "2026-10-03T10:00:00Z", "unknown", 2000), ("u1", "2026-10-03T10:05:00Z", "shoes", 1500)], &[]);
    fixture(
        "E14",
        &[
            ("u1", "2026-10-03T09:00:00Z", "", 0),
            ("u1", "2026-10-03T09:10:00Z", "", 0),
            ("u1", "2026-10-03T14:00:00Z", "", 0),
            ("u1", "2026-10-04T08:00:00Z", "", 0),
        ],
        &[],
    );
    fixture(
        "E16",
        &[
            ("u1", "1791021600000", "", 0),
            ("u1", "2026-10-03T10:00:00Z", "", 0),
            ("u1", "1791023400001", "shoes", 700),
            ("u1", "1791023400001", "", 0),
            ("u1", "1791023400001", "", 0),
            ("u1", "1791025200001", "", 0),
            ("u2", "2026-10-03T10:00:00Z", "", 0),
            ("u2", "2026-10-03T10:30:00Z", "", 0),
            ("u2", "1791023400000", "", 0),
            ("u2", "1791025200001", "", 0),
        ],
        &[("k1", "u1", "c_2", "1791023400001")],
    );
    // L1: `u2` is linked to `u1`, so its purchase is the person `u1`'s.
    fixture(
        "L1",
        &[
            ("u1", "2026-10-03T10:00:00Z", "", 0),
            ("u1", "2026-10-03T10:25:00Z", "books", 800),
            ("u1", "2026-10-03T10:50:00Z", "", 0),
            ("u3", "2026-10-03T10:00:00Z", "", 0),
        ],
        &[],
    );
}

#[test]
fn attribution_fixtures() {
    let one = &[("u1", "2026-10-03T10:00:00Z", "", 0)];
    let two = &[("u1", "2026-10-03T10:00:00Z", "", 0), ("u2", "2026-10-03T10:00:00Z", "", 0)];
    fixture("A1", one, &[("k1", "u1", "c_1", "2026-10-03T09:00:00Z")]);
    fixture("A2", two, &[("k1", "u1", "c_1", "2026-09-26T10:00:00Z"), ("k2", "u2", "c_1", "1790416799999")]);
    fixture("A3", one, &[("k1", "u1", "c_1", "2026-10-02T10:00:00Z"), ("k2", "u1", "c_2", "2026-10-03T08:00:00Z")]);
    fixture("A4", two, &[("k1", "u1", "c_1", "2026-10-03T10:00:00Z"), ("k2", "u2", "c_1", "1791021600001")]);
    fixture("A5", one, &[("k2", "u1", "c_2", "2026-10-03T09:00:00Z"), ("k1", "u1", "c_1", "2026-10-03T09:00:00Z")]);
    fixture(
        "A8",
        &[("u1", "2026-10-03T10:00:00Z", "shoes", 1000), ("u1", "2026-10-04T08:00:00Z", "", 0)],
        &[("k1", "u1", "c_1", "2026-10-03T09:00:00Z")],
    );
    fixture(
        "A9",
        &[("u1", "2026-10-03T10:00:00Z", "", 0), ("u1", "2026-10-03T10:25:00Z", "", 0), ("u1", "2026-10-03T11:30:00Z", "", 0)],
        &[("k1", "u1", "c_1", "2026-10-03T10:20:00Z")],
    );
    // A10: k2 (before the click window) and k3 (at the window's end) were dropped at parse.
    fixture(
        "A10",
        &[("u1", "2026-10-01T00:00:00Z", "", 0), ("u2", "2026-10-01T00:00:00Z", "", 0), ("u3", "2026-10-14T23:59:59Z", "", 0)],
        &[("k1", "u1", "c_1", "2026-09-24T00:00:00Z"), ("k4", "u3", "c_2", "2026-10-14T23:59:59Z")],
    );
    fixture(
        "A11",
        &[("u1", "2026-10-03T10:00:00Z", "shoes", 1000), ("u2", "2026-10-03T10:00:00Z", "", 0)],
        &[
            ("k1", "u1", "c_1", "2026-09-26T10:00:00Z"),
            ("k2", "u1", "c_2", "2026-10-03T09:00:00Z"),
            ("k3", "u1", "c_2", "2026-10-03T09:30:00Z"),
            ("k4", "u1", "c_1", "1791021600001"),
            ("k5", "u2", "c_1", "1790416799999"),
        ],
    );
    // A12 and L4: `u2` is linked to `u1`, so its click is the person `u1`'s.
    fixture(
        "A12",
        &[
            ("u1", "2026-10-03T10:00:00Z", "", 0),
            ("u1", "2026-10-04T10:00:00Z", "", 0),
            ("u1", "2026-10-09T09:00:00Z", "", 0),
            ("u1", "1791622800001", "", 0),
        ],
        &[("k1", "u1", "c_1", "2026-10-03T09:00:00Z"), ("k2", "u1", "c_2", "2026-10-04T09:00:00Z")],
    );
    fixture("L4", one, &[("k1", "u1", "c_1", "2026-10-03T09:00:00Z"), ("k2", "u1", "c_2", "2026-10-03T08:00:00Z")]);
}
