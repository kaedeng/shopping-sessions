use std::collections::HashMap;

use types::hash::hash;
use types::id::pack;
use types::record::{EventRec, VALID};

use crate::{Scratch, partition};

/// splitmix64
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let x = (self.0 ^ (self.0 >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn sorted(mut recs: Vec<EventRec>) -> Vec<EventRec> {
    recs.sort_unstable_by_key(|r| r.id);
    recs
}

fn reference(pieces: &[Vec<EventRec>]) -> Vec<EventRec> {
    let mut first: HashMap<u64, EventRec> = HashMap::new();
    for r in pieces.iter().flatten() {
        let kept = first.entry(r.id).or_insert(*r);
        if r.ingest < kept.ingest {
            *kept = *r;
        }
    }
    sorted(first.into_values().collect())
}

/// One partition's records spread over `workers` pieces, some empty: distinct events, a few with
/// 1 to 3 later or tying copies (a tie is the same record), any copy possibly a tombstone.
fn random_partition(random: &mut Random, part_bits: u32, part: u64, workers: usize) -> Vec<Vec<EventRec>> {
    let events = random.below(5000) as usize;
    let mut pieces = vec![Vec::new(); workers];
    let mut made = 0;
    while made < events {
        let id = random.next() >> random.below(56);
        if id == 0 || (part_bits > 0 && hash(id) >> (64 - part_bits) != part) {
            continue;
        }
        made += 1;
        let copies = if random.below(10) == 0 { 2 + random.below(3) } else { 1 };
        let ingest = random.below(1 << 20) as u32;
        let mut made_copies: Vec<EventRec> = Vec::new();
        for copy in 0..copies {
            let ingest = if copy > 0 && random.below(4) == 0 { ingest } else { ingest + random.below(600) as u32 };
            let mut rec = EventRec {
                id,
                user: random.next() >> 1,
                ts: random.next() as u32,
                ingest,
                pay: copy as u32,
                flags: if random.below(20) == 0 { 0 } else { VALID },
            };
            if let Some(tie) = made_copies.iter().find(|r| r.ingest == ingest) {
                rec = *tie;
            }
            made_copies.push(rec);
            pieces[random.below(workers as u64) as usize].push(rec);
        }
    }
    for piece in &mut pieces {
        for at in (1..piece.len()).rev() {
            piece.swap(at, random.below(at as u64 + 1) as usize);
        }
    }
    pieces
}

#[test]
fn matches_the_reference_on_random_partitions() {
    let mut random = Random(3);
    let mut scratch = Scratch::default();
    for round in 0..200 {
        let part_bits = [0, 1, 10, 11][round % 4];
        let part = random.below(1 << part_bits);
        let pieces = random_partition(&mut random, part_bits, part, 1 + round % 9);
        let kept = partition(pieces.iter().map(Vec::as_slice), part_bits, &mut scratch).to_vec();
        assert_eq!(sorted(kept), reference(&pieces), "round {round}");
    }
}

#[test]
fn fixtures_e4_and_e5_and_empty_pieces() {
    let e = |id: &[u8], user: &[u8], ingest: u32, flags: u32| EventRec { id: pack(id), user: pack(user), ts: 1, ingest, pay: 0, flags };
    let mut scratch = Scratch::default();
    // E4: the earliest copy wins across users. E5: an invalid earliest copy wins over a valid
    // later one listed first, so the event is dropped.
    let pieces = [
        vec![],
        vec![e(b"e2", b"u1", 605, VALID), e(b"e1", b"u2", 605, VALID)],
        vec![],
        vec![e(b"e1", b"u1", 601, VALID), e(b"e2", b"u1", 601, 0)],
    ];
    let kept = sorted(partition(pieces.iter().map(Vec::as_slice), 0, &mut scratch).to_vec());
    let mut expected = vec![e(b"e1", b"u1", 601, VALID), e(b"e2", b"u1", 601, 0)];
    expected.sort_unstable_by_key(|r| r.id);
    assert_eq!(kept, expected);
    assert!(partition([[].as_slice(); 3].into_iter(), 10, &mut scratch).is_empty());
}
