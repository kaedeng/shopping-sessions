use std::collections::HashMap;

use types::hash::person_hash;
use types::id::{SLOW, pack};
use types::parts::Parts;
use types::record::{ClickRec, EventRec, PersonEvent, VALID};

use crate::{build, scatter_clicks, scatter_events};

/// splitmix64
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let x = (self.0 ^ (self.0 >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Packed IDs of every length 1 to 9, and slow ones: longer than 9 bytes, non-ASCII, or holding 0x00.
fn random_id(random: &mut Random) -> Vec<u8> {
    const ALPHABET: &[u8] = b"\t AUZaku0129_";
    let longest = if random.below(8) == 0 { 14 } else { 9 };
    let len = 1 + random.below(longest);
    let mut id: Vec<u8> = (0..len).map(|_| ALPHABET[random.below(ALPHABET.len())]).collect();
    match random.below(16) {
        0 => id.extend_from_slice("é".as_bytes()),
        1 => id.insert(random.below(id.len() + 1), 0),
        _ => {}
    }
    id
}

/// The naive reference: a plain union-find over byte strings, each person the smallest bytes.
fn reference(pairs: &[[Vec<u8>; 2]]) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut index: HashMap<&[u8], usize> = HashMap::new();
    let mut ids: Vec<&[u8]> = Vec::new();
    let mut parent: Vec<usize> = Vec::new();
    fn find(parent: &[usize], mut x: usize) -> usize {
        while parent[x] != x {
            x = parent[x];
        }
        x
    }
    for pair in pairs {
        let [a, b] = [0, 1].map(|side| {
            *index.entry(&pair[side]).or_insert_with(|| {
                ids.push(&pair[side]);
                parent.push(parent.len());
                parent.len() - 1
            })
        });
        let (ra, rb) = (find(&parent, a), find(&parent, b));
        parent[ra] = rb;
    }
    let mut smallest: HashMap<usize, &[u8]> = HashMap::new();
    for (node, &id) in ids.iter().enumerate() {
        let name = smallest.entry(find(&parent, node)).or_insert(id);
        *name = (*name).min(id);
    }
    ids.iter().enumerate().map(|(node, id)| (id.to_vec(), smallest[&find(&parent, node)].to_vec())).collect()
}

/// Components of every shape, their pairs shuffled together.
fn random_pairs(random: &mut Random) -> Vec<[Vec<u8>; 2]> {
    let mut pairs: Vec<[Vec<u8>; 2]> = Vec::new();
    for _ in 0..1 + random.below(60) {
        let size = match random.below(10) {
            0 => 100 + random.below(400),
            1..=3 => 1,
            _ => 2 + random.below(8),
        };
        let ids: Vec<Vec<u8>> = (0..size).map(|_| random_id(random)).collect();
        match random.below(3) {
            // A chain in a random order of its IDs.
            0 => pairs.extend(ids.windows(2).map(|w| [w[0].clone(), w[1].clone()])),
            // A star around one ID.
            1 => pairs.extend(ids[1..].iter().map(|id| [ids[0].clone(), id.clone()])),
            // Each ID to an earlier one, as the generator does.
            _ => pairs.extend((1..size).map(|at| [ids[at].clone(), ids[random.below(at)].clone()])),
        }
        if size == 1 || random.below(4) == 0 {
            pairs.push([ids[0].clone(), ids[0].clone()]);
        }
    }
    for at in 0..pairs.len() {
        if random.below(8) == 0 {
            let [a, b] = pairs[at].clone();
            pairs.push(if random.below(2) == 0 { [a, b] } else { [b, a] });
        }
    }
    for at in (1..pairs.len()).rev() {
        pairs.swap(at, random.below(at + 1));
    }
    for pair in &mut pairs {
        if random.below(2) == 0 {
            pair.swap(0, 1);
        }
    }
    pairs
}

#[test]
fn matches_the_reference_on_random_pairs() {
    let mut random = Random(7);
    let (mut past_home, mut misses_past_full_line) = (0, 0);
    for _ in 0..300 {
        let pairs = random_pairs(&mut random);
        let packed: Vec<[u64; 2]> = pairs.iter().map(|[a, b]| [pack(a), pack(b)]).collect();
        let table = build(&packed);
        let names = reference(&pairs);
        for (id, name) in &names {
            assert_eq!(table.person(pack(id)), pack(name), "{id:?} should be named {name:?}");
        }
        for _ in 0..200 {
            let id = random_id(&mut random);
            if !names.contains_key(&id) {
                assert_eq!(table.person(pack(&id)), pack(&id), "{id:?} is in no pair");
                misses_past_full_line += usize::from(table.lines[table.home(pack(&id))].0[3][0] != 0);
            }
        }
        let homes = table.lines.iter().enumerate().flat_map(|(at, line)| line.0.map(|slot| (at, slot[0])));
        past_home += homes.filter(|&(at, key)| key != 0 && table.home(key) != at).count();
    }
    // The paths past a full line ran, for keys stored and for IDs missing.
    assert!(past_home > 0 && misses_past_full_line > 0, "{past_home} {misses_past_full_line}");
}

#[test]
fn smallest_id_named_even_when_seen_last() {
    // A chain of 300 IDs whose smallest is the last one any pair mentions, and the pairs run
    // from the largest down, so every union links a root under a root first seen later.
    let ids: Vec<Vec<u8>> = (0..300).rev().map(|n| format!("v{n:03}").into_bytes()).collect();
    let pairs: Vec<[u64; 2]> = ids.windows(2).map(|w| [pack(&w[0]), pack(&w[1])]).collect();
    let table = build(&pairs);
    for id in &ids {
        assert_eq!(table.person(pack(id)), pack(b"v000"));
    }
}

#[test]
fn fixtures_l2_and_l3() {
    let l2 = [[b"u3".as_slice(), b"u9"], [b"u9", b"u2"], [b"U7", b"u9"], [b"u10", b"u10"]];
    let table = build(&l2.map(|pair| pair.map(pack)));
    for (id, name) in [("u3", "U7"), ("u2", "U7"), ("u9", "U7"), ("U7", "U7"), ("u10", "u10"), ("u1", "u1")] {
        assert_eq!(table.person(pack(id.as_bytes())), pack(name.as_bytes()), "{id}");
    }
    // The kept lines of L3, the header-like row among them.
    let l3 = [
        [b"u4".as_slice(), b"u1"],
        [b"u4", b"u1"],
        [b"u6", b"u6"],
        [b"user_id_a", b"user_id_b"],
        [b"u4", b"user_id_b"],
        [b"A\tB", b"user_id_a"],
    ];
    let table = build(&l3.map(|pair| pair.map(pack)));
    for id in ["u1", "u4", "user_id_a", "user_id_b", "A\tB"] {
        assert_eq!(table.person(pack(id.as_bytes())), pack(b"A\tB"), "{id}");
    }
    for id in ["u2", "u3", "u5", "u6"] {
        assert_eq!(table.person(pack(id.as_bytes())), pack(id.as_bytes()), "{id}");
    }
}

#[test]
fn slow_ids_order_by_bytes() {
    // "0123456789" (slow, long) < "u" < "u\0" (slow) < "u1" < "u\u{e9}" (slow), byte-wise.
    let ids: [&[u8]; 5] = [b"u1", "u\u{e9}".as_bytes(), b"u\0", b"0123456789", b"u"];
    assert!(ids.iter().filter(|id| pack(id) & SLOW != 0).count() == 3);
    for first in 0..ids.len() {
        let pairs: Vec<[u64; 2]> = (1..ids.len()).map(|k| [pack(ids[(first + k - 1) % 5]), pack(ids[(first + k) % 5])]).collect();
        let table = build(&pairs);
        for id in ids {
            assert_eq!(table.person(pack(id)), pack(b"0123456789"));
        }
    }
    let table = build(&[[pack(b"u1"), pack("u\u{e9}".as_bytes())], [pack(b"u\0"), pack(b"u\0")]]);
    assert_eq!(table.person(pack("u\u{e9}".as_bytes())), pack(b"u1"));
    assert_eq!(table.person(pack(b"u\0")), pack(b"u\0"));
}

#[test]
fn no_pairs_and_empty_sides() {
    let table = build(&[]);
    assert_eq!(table.person(pack(b"u1")), pack(b"u1"));
    let table = build(&[[0, 0], [pack(b"u3"), 0], [pack(b"u2"), pack(b"u1")], [0, pack(b"u2")]]);
    assert_eq!([b"u1", b"u2", b"u3"].map(|id| table.person(pack(id))), [b"u1", b"u1", b"u3"].map(|id| pack(id)));
}

#[test]
fn scatter_puts_each_record_in_its_persons_bucket() {
    let mut random = Random(11);
    let pairs = random_pairs(&mut random);
    let names = reference(&pairs);
    let table = build(&pairs.iter().map(|[a, b]| [pack(a), pack(b)]).collect::<Vec<_>>());
    let mut users: Vec<Vec<u8>> = names.keys().cloned().collect();
    users.extend((0..200).map(|_| random_id(&mut random)));
    let person = |user: &[u8]| pack(names.get(user).map_or(user, |name| name));
    let bits = 4;
    let bucket = |person: u64| (person_hash(person) >> (64 - bits)) as usize;

    for rows in [0, 1, 63, 64, 65, 3000] {
        let events: Vec<(EventRec, &[u8])> = (0..rows)
            .map(|n| {
                let user = &users[random.below(users.len())];
                let flags = if random.below(10) == 0 { 0 } else { VALID };
                let rec = EventRec { id: n as u64 + 1, user: pack(user), ts: n as u32, ingest: 0, pay: 7 * n as u32, flags };
                (rec, user.as_slice())
            })
            .collect();
        let kept: Vec<EventRec> = events.iter().map(|e| e.0).collect();
        // A room of 3 makes the busier buckets spill, so both pieces of a list are checked.
        fn lists<T: Copy>(parts: &Parts<T>) -> Vec<Vec<T>> {
            (0..parts.lists()).map(|list| parts.pieces(list).concat()).collect()
        }
        let mut parts = Parts::new(1 << bits, 3);
        scatter_events(&table, &kept, bits, &mut parts);
        let out = lists(&parts);
        let mut expected = vec![Vec::new(); 1 << bits];
        for (rec, user) in events.iter().filter(|e| e.0.flags & VALID != 0) {
            let person = person(user);
            expected[bucket(person)].push(PersonEvent { person, ts: rec.ts, pay: rec.pay });
        }
        assert_eq!(out, expected, "{rows} events");

        let clicks: Vec<(ClickRec, &[u8])> = (0..rows)
            .map(|n| {
                let user = &users[random.below(users.len())];
                (ClickRec { user: pack(user), click: n as u64 + 1, ts: n as u32, campaign: 3 }, user.as_slice())
            })
            .collect();
        let recs: Vec<ClickRec> = clicks.iter().map(|c| c.0).collect();
        let mut parts = Parts::new(1 << bits, 3);
        scatter_clicks(&table, &recs, bits, &mut parts);
        let out = lists(&parts);
        let mut expected = vec![Vec::new(); 1 << bits];
        for (rec, user) in &clicks {
            let person = person(user);
            expected[bucket(person)].push(ClickRec { user: person, ..*rec });
        }
        assert_eq!(out, expected, "{rows} clicks");
    }
}
