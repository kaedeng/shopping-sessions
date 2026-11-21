use std::path::{Path, PathBuf};

use types::id::pack;

use super::*;

type Pick = (ParseFn, LinksFn);
const SCALAR: Pick = (parse_with::<Scalar>, links_with::<Scalar>);
#[cfg(feature = "tok-mask")]
const MASK: Pick = (parse_with::<tok::Mask>, links_with::<tok::Mask>);
const PICKS: &[Pick] = &[SCALAR, #[cfg(feature = "tok-mask")] MASK];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture(case: &str) -> PathBuf {
    root().join("fixtures").join(case).join("input")
}

#[derive(Debug, PartialEq)]
struct Parsed {
    events: Vec<EventRec>,
    clicks: Vec<ClickRec>,
    links: Vec<[Id; 2]>,
}

fn event_key(e: &EventRec) -> (u64, u64, u32, u32, u32, u32) {
    (e.id, e.user, e.ts, e.ingest, e.pay, e.flags)
}

/// Every task of `data` on one `ParseOut`, records sorted so runs compare as multisets.
fn parse_dir(data: &Path, ctx: &Ctx, chunk: usize, batch: usize, pick: Pick) -> Parsed {
    parse_dir_scatter(data, ctx, chunk, batch, pick, types::parts::mode_from_env())
}

/// `parse_dir` with the partitions written plain (`None`) or staged in buffers of that many lines.
fn parse_dir_scatter(data: &Path, ctx: &Ctx, chunk: usize, batch: usize, (parse, links): Pick, scatter: Option<usize>) -> Parsed {
    let mut ingest = Ingest::open_with(data, ctx, chunk, parse, links).unwrap();
    ingest.scatter = scatter;
    let mut out = ingest.new_out(ctx);
    out.scratch.batch = batch;
    for task in 0..ingest.tasks() {
        ingest.parse(ctx, task, &mut out);
    }
    for at in 0..out.parts.lists() {
        assert!(out.parts.pieces(at).iter().all(|piece| piece.iter().all(|e| (hash(e.id) >> (64 - ctx.part_bits)) as usize == at)));
    }
    let mut events: Vec<EventRec> = out.parts.iter().copied().collect();
    events.sort_by_key(event_key);
    let mut clicks = out.clicks;
    clicks.sort_by_key(|c| (c.user, c.click, c.ts, c.campaign));
    let mut links = ingest.links();
    links.sort();
    Parsed { events, clicks, links }
}

fn parse_fixture(case: &str, pick: Pick) -> (Ctx, Parsed) {
    let ctx = load_ctx(&fixture(case)).unwrap();
    let parsed = parse_dir(&fixture(case), &ctx, 1 << 20, tok::Scratch::new().batch, pick);
    (ctx, parsed)
}

fn at(ctx: &Ctx, f1: &str) -> u32 {
    (parse_f1(f1.as_bytes()).unwrap() - ctx.origin) as u32
}

fn events<'a>(parsed: &'a Parsed, id: &str) -> Vec<&'a EventRec> {
    parsed.events.iter().filter(|e| e.id == pack(id.as_bytes())).collect()
}

/// The one record of `id`, and whether it is valid.
fn one<'a>(parsed: &'a Parsed, id: &str) -> (&'a EventRec, bool) {
    let found = events(parsed, id);
    assert_eq!(found.len(), 1, "{id:?}: {found:?}");
    (found[0], found[0].flags & VALID != 0)
}

fn pay(ctx: &Ctx, cents: u32, product: &str) -> u32 {
    cents | u32::from(ctx.products.get(pack(product.as_bytes())).unwrap_or(ctx.unknown)) << PAY_CATEGORY_SHIFT
}

#[test]
fn fixtures_that_concern_parsing() {
    for &pick in PICKS {
        let (ctx, p) = parse_fixture("E5", pick);
        let mut copies = events(&p, "e2");
        copies.sort_by_key(|e| e.ingest);
        assert_eq!(copies.len(), 2);
        assert_eq!((copies[0].flags, copies[1].flags), (0, VALID), "the earliest copy has the bad ts");
        assert_eq!(copies[1].ts, at(&ctx, "2026-10-03T10:00:00Z"));
        assert!(copies[0].ingest < copies[1].ingest);
        assert_eq!(copies[1].ingest - copies[0].ingest, 240);

        let (_, p) = parse_fixture("E10", pick);
        assert!(!one(&p, "e1").1, "19.99 drops the purchase");
        assert!(one(&p, "e2").1, "a view's price is ignored");

        let (_, p) = parse_fixture("E13", pick);
        assert_eq!(p.events.len(), 2, "the empty event_id and the 6-field line are gone");
        assert!(!one(&p, "e3").1 && !one(&p, "e4").1);

        let (_, p) = parse_fixture("E15", pick);
        assert!(p.events.iter().all(|e| e.user == pack(b"u1") && e.flags == VALID) && p.events.len() == 2);

        let (ctx, p) = parse_fixture("E17", pick);
        assert!(!one(&p, "e1").1 && !one(&p, "e2").1 && !one(&p, "e5").1);
        assert_eq!(one(&p, "e3"), (&EventRec { ts: at(&ctx, "2026-10-03T10:00:00Z"), ..*one(&p, "e3").0 }, true));
        assert_eq!(one(&p, "e4"), (&EventRec { ts: at(&ctx, "2026-10-03T12:00:00Z"), ..*one(&p, "e4").0 }, true));

        let (ctx, p) = parse_fixture("E18", pick);
        assert_eq!(one(&p, "e1").0.ts, at(&ctx, "2026-10-03T10:00:00Z"));
        assert!(one(&p, "e1").1 && one(&p, "e7").1);
        assert_eq!(one(&p, "e7").0.pay, pay(&ctx, 250, "p1"));
        for id in ["e2", "e3", "e4", "e5", "e6", "e8"] {
            assert!(!one(&p, id).1, "{id}");
        }

        let (ctx, p) = parse_fixture("E19", pick);
        assert_eq!(p.events.len(), 7);
        for (id, user) in [("e1", "u1"), ("e2", " u1"), ("e3", "u1 "), ("e4", " u1"), ("e1 ", "u1")] {
            assert_eq!(one(&p, id).0.user, pack(user.as_bytes()), "{id}");
            assert!(one(&p, id).1, "{id}");
        }
        assert_eq!(one(&p, "e4").0.pay, pay(&ctx, 300, "p1"));
        assert_eq!(one(&p, "e5"), (&EventRec { pay: pay(&ctx, 400, " p1"), ..*one(&p, "e5").0 }, true));
        assert_eq!(one(&p, "e5").0.pay >> PAY_CATEGORY_SHIFT, u32::from(ctx.unknown));
        assert!(!one(&p, "e6").1, "\" view\" is not a type");
        let k1 = ClickRec { user: pack(b" u1"), click: pack(b"k1"), ts: at(&ctx, "2026-10-03T09:00:00Z"), campaign: 0 };
        assert_eq!(p.clicks, [k1], "\" c_1\" is not a campaign");

        let (ctx, p) = parse_fixture("E20", pick);
        assert!(one(&p, "e1").1 && !one(&p, "e2").1 && one(&p, "e3").1 && one(&p, "e3\r").1);
        assert_eq!(one(&p, "e3").0.pay, pay(&ctx, 500, "p2"));
        assert_eq!(p.clicks.len(), 1);
        assert_eq!((p.clicks[0].click, p.clicks[0].campaign), (pack(b"k2"), 1));

        let (_, p) = parse_fixture("L3", pick);
        let mut links: Vec<[Id; 2]> = [
            ["u4", "u1"], ["u4", "u1"], ["u6", "u6"], ["user_id_a", "user_id_b"], ["u4", "user_id_b"], ["A\tB", "user_id_a"],
        ]
        .iter()
        .map(|[a, b]| [pack(a.as_bytes()), pack(b.as_bytes())])
        .collect();
        links.sort();
        assert_eq!(p.links, links);
        assert!(p.events.len() == 6 && p.events.iter().all(|e| e.flags == VALID));

        let (_, p) = parse_fixture("A6", pick);
        assert!(p.clicks.is_empty() && one(&p, "e1").1);

        let (ctx, p) = parse_fixture("A10", pick);
        let clicks: Vec<(Id, u32)> = p.clicks.iter().map(|c| (c.click, c.ts)).collect();
        assert_eq!(clicks, [(pack(b"k1"), 0), (pack(b"k4"), at(&ctx, "2026-10-14T23:59:59Z"))]);
        assert!(p.events.iter().all(|e| e.flags == VALID) && p.events.len() == 3);
        assert_eq!(one(&p, "e1").0.ts, ctx.event_lo);
    }
}

/// A scratch dataset: the fixtures' rows, plus every awkward file shape.
fn odd_dataset() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ingest-odd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for folder in ["events", "clicks", "links"] {
        std::fs::create_dir_all(dir.join(folder)).unwrap();
    }
    for name in ["manifest.json", "products.csv", "campaigns.csv"] {
        std::fs::copy(fixture("E1").join(name), dir.join(name)).unwrap();
    }
    let mut rows = [Vec::new(), Vec::new(), Vec::new()];
    for case in std::fs::read_dir(root().join("fixtures")).unwrap() {
        let input = case.unwrap().path().join("input");
        for (folder, rows) in ["events", "clicks", "links"].into_iter().zip(&mut rows) {
            if let Ok(file) = std::fs::read(input.join(folder).join("part-00000.csv")) {
                rows.extend_from_slice(&file[memchr(b'\n', &file).unwrap() + 1..]);
            }
        }
    }
    let headers = ["event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", "click_id,user_id,campaign_id,ts", "user_id_a,user_id_b"];
    let last = ["zlast,u1,2026-10-03T10:00:00Z,2026-10-03T10:00:00Z,view,p1,", "zlast,u1,c_1,2026-10-03T10:00:00Z", "zlast,u1"];
    for (((folder, rows), header), last) in ["events", "clicks", "links"].into_iter().zip(&rows).zip(headers).zip(last) {
        let file = |name: &str, bytes: &[u8]| std::fs::write(dir.join(folder).join(name), bytes).unwrap();
        file("a.csv", &[header.as_bytes(), b"\n", &rows.repeat(3)].concat());
        let half = memrchr(b'\n', &rows[..rows.len() / 2]).map_or(0, |at| at + 1);
        file("b.csv", &[header.as_bytes(), b"\n", &rows[..half], last.as_bytes()].concat());
        file("c.csv", &[header.as_bytes(), b"\n"].concat());
        file("d.csv", b"");
        file("e.csv", header.as_bytes());
        file(".hidden.csv", &[header.as_bytes(), b"\n", last.as_bytes(), b"\n"].concat());
        file("notes.txt", &[header.as_bytes(), b"\n", last.as_bytes(), b"\n"].concat());
    }
    dir
}

#[test]
fn chunk_cuts_and_batches_change_nothing() {
    let dir = odd_dataset();
    let ctx = load_ctx(&dir).unwrap();
    let reference = parse_dir(&dir, &ctx, 1 << 20, 1 << 14, SCALAR);
    let zlast = pack(b"zlast");
    assert_eq!(reference.events.iter().filter(|e| e.id == zlast).count(), 1, "a last line without \\n is a row");
    assert_eq!(reference.clicks.iter().filter(|c| c.click == zlast).count(), 1);
    assert_eq!(reference.links.iter().filter(|l| l[0] == zlast).count(), 1);
    assert!(reference.events.len() > 100 && reference.clicks.len() > 20 && reference.links.len() > 10);
    let longest = std::fs::metadata(dir.join("events/a.csv")).unwrap().len() as usize;
    for &pick in PICKS {
        for chunk in 1..=longest + 1 {
            assert!(parse_dir(&dir, &ctx, chunk, 1 << 14, pick) == reference, "chunk {chunk}");
        }
        for batch in [1, 2, 63, 64, 65, 100, 1000] {
            for chunk in [1, 777, 1 << 20] {
                assert!(parse_dir(&dir, &ctx, chunk, batch, pick) == reference, "batch {batch}, chunk {chunk}");
            }
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The fields of every well-formed line of `text`.
fn tokenize<T: Tok, const N: usize>(text: &[u8], batch: usize) -> Vec<Vec<Vec<u8>>> {
    let buf = padded(text);
    let mut scratch = Scratch::new();
    scratch.batch = batch;
    let mut lines = Vec::new();
    // SAFETY: `padded` made it a segment.
    unsafe {
        T::lines::<N>(buf.as_ptr().add(1), buf.len() - 1 - PAD, &mut scratch, |fields| {
            lines.push(fields.iter().map(|f| std::slice::from_raw_parts(f.p, f.len).to_vec()).collect())
        })
    };
    lines
}

fn both<const N: usize>(text: &[u8]) -> Vec<Vec<Vec<u8>>> {
    let scalar = tokenize::<Scalar, N>(text, 1 << 14);
    #[cfg(feature = "tok-mask")]
    for batch in [1, 37, 64, 1 << 14] {
        assert_eq!(tokenize::<tok::Mask, N>(text, batch), scalar, "batch {batch}: {:?}", String::from_utf8_lossy(text));
    }
    #[cfg(feature = "tok-shape")]
    assert_eq!(tokenize::<tok::Shape, N>(text, 1 << 14), scalar, "shape: {:?}", String::from_utf8_lossy(text));
    #[cfg(all(feature = "tok-shape", target_arch = "x86_64", target_feature = "avx512bw", target_feature = "bmi2"))]
    assert_eq!(tokenize::<tok::Fused, N>(text, 1 << 14), scalar, "fused: {:?}", String::from_utf8_lossy(text));
    scalar
}

#[test]
fn tokenizer_cases() {
    let ok: &[(&str, [&str; 2])] = &[
        ("a,b", ["a", "b"]),
        ("\"a\",\"b\"", ["a", "b"]),
        ("\"\",b", ["", "b"]),
        (",", ["", ""]),
        ("\"\",\"\"", ["", ""]),
        ("a\r,b\r", ["a\r", "b\r"]),
        (" a ,\" b\"", [" a ", " b"]),
        ("\" a \",b", [" a ", "b"]),
    ];
    let bad = [" \"a\",b", "a,\"b\" ", "a", "a,b,c", "", "\"a,b", "a\",b", "a\"b,c", "\",b", "\"\"\"\",b", "\"a\"b,c", "a,\"b", "\"a\"\",b", "a,b\""];
    // Each case alone, after a line that shifts it across a block edge, and all in one text.
    for pad in [0, 1, 30, 62, 63, 64, 65, 127] {
        let lead = format!("{},x\n", "y".repeat(pad));
        for (line, fields) in ok {
            let want = vec![vec![b"y".repeat(pad), b"x".to_vec()], fields.iter().map(|f| f.as_bytes().to_vec()).collect()];
            assert_eq!(both::<2>(format!("{lead}{line}\n").as_bytes()), want, "{line:?}");
            assert_eq!(both::<2>(format!("{lead}{line}").as_bytes()), want, "no final newline: {line:?}");
        }
        for line in bad {
            assert_eq!(both::<2>(format!("{lead}{line}\n").as_bytes()).len(), 1, "{line:?}");
        }
    }
    let all: String = ok.iter().map(|(line, _)| *line).chain(bad).map(|line| format!("{line}\n")).collect();
    assert_eq!(both::<2>(all.as_bytes()).len(), ok.len());
    assert_eq!(both::<2>(b"\n\n\n").len(), 0);
    assert_eq!(both::<7>(b"a,b,c,d,e,f,g\na,b,c,d,e,f\na,b,c,d,e,f,g,h\n\"a\",b,c,d,e,f,\"\"\n").len(), 2);
}

/// xorshift64*, enough for test data.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) % n
    }
}

#[test]
fn tokenizers_agree_on_random_lines() {
    let pieces: &[&str] = &["", "a", "u1x9", " x ", "\r", "\"\"", "\"q\"", "\"", "a\"b", "\"\"\"\"", ",", "\n"];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut total = 0;
    for _ in 0..200 {
        let mut text = Vec::new();
        for _ in 0..rng.below(300) {
            if rng.below(4) == 0 {
                for _ in 0..rng.below(150) {
                    text.push(b"a,\"\n\r "[rng.below(6) as usize]);
                }
            } else {
                for field in 0..1 + rng.below(9) {
                    if field > 0 {
                        text.push(b',');
                    }
                    let value = if rng.below(10) == 0 { "x".repeat(rng.below(200) as usize) } else { pieces[rng.below(pieces.len() as u64) as usize].to_string() };
                    let quote = if rng.below(5) == 0 { "\"" } else { "" };
                    text.extend_from_slice(format!("{quote}{value}{quote}").as_bytes());
                }
            }
            text.push(b'\n');
        }
        total += both::<2>(&text).len() + both::<4>(&text).len() + both::<7>(&text).len();
    }
    assert!(total > 1000, "only {total} well-formed lines: the generator needs more of them");
}

/// An instant as F1.
fn f1(ms: i64) -> String {
    let (y, mo, d) = types::time::civil_from_days(ms.div_euclid(DAY));
    let s = ms.rem_euclid(DAY) / SECOND;
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, s / 60 % 60, s % 60)
}

/// The rules' timestamp grammar, written plainly: epoch ms, or `None` for a malformed value.
fn reference_ts(text: &[u8]) -> Option<i128> {
    if text.len() > 10 && text[10] == b'T' {
        // `parse_f1` overflows in debug builds on a non-digit where it expects one.
        if text.len() < 19 || [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18].iter().any(|&at| !text[at].is_ascii_digit()) {
            return None;
        }
        if text.len() == 25 {
            let two = |at: usize| (text[at].is_ascii_digit() && text[at + 1].is_ascii_digit()).then(|| i128::from((text[at] - b'0') * 10 + text[at + 1] - b'0'));
            let (hours, minutes) = (two(20)?, two(23)?);
            let sign = match text[19] { b'+' => 1, b'-' => -1, _ => return None };
            if text[22] != b':' || hours > 23 || minutes > 59 {
                return None;
            }
            let utc = [&text[..19], b"Z"].concat();
            return parse_f1(&utc).map(|ms| i128::from(ms) - sign * (hours * 3600 + minutes * 60) * 1000);
        }
        return parse_f1(text).map(i128::from);
    }
    (!text.is_empty() && text.iter().all(u8::is_ascii_digit)).then(|| std::str::from_utf8(text).unwrap().parse::<u128>().map_or(i128::MAX, |v| v.min(i128::MAX as u128) as i128))
}

fn check_ts(dates: &Dates, ctx: &Ctx, text: &[u8]) {
    let buf = padded(text);
    // SAFETY: a padded buffer has PAD bytes after the text.
    let (ok, ms) = unsafe { dates.ts(buf.as_ptr().add(1), text.len()) };
    let in_click_window = |ms: i128| (0..i128::from(ctx.ts_end)).contains(&ms);
    let want = reference_ts(text).map(|epoch| epoch - i128::from(ctx.origin)).filter(|&ms| in_click_window(ms));
    let got = (ok && in_click_window(i128::from(ms))).then_some(i128::from(ms));
    assert_eq!(got, want, "{:?}", String::from_utf8_lossy(text));
    // The 8-wide forms: `f1_wide` accepts exactly what `f1` does, to the same values;
    // `ts_wide` accepts nothing `ts` rejects, gives its value, and takes every F1, F2 and
    // plain F3.
    #[cfg(all(feature = "pipe-positional", target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512dq"))]
    // SAFETY: a padded buffer has PAD bytes after the text.
    unsafe {
        use core::arch::x86_64::*;
        let field = buf.as_ptr().add(1);
        let all = |value: u64| _mm512_set1_epi64(value as i64);
        let lane = |v: __m512i| {
            let mut out = [0i64; 8];
            _mm512_storeu_si512(out.as_mut_ptr().cast(), v);
            assert!(out.iter().all(|&x| x == out[0]));
            out[5]
        };
        let (g0, g1, g2, g3) = (all(ts::rd(field)), all(ts::rd(field.add(8))), all(ts::rd(field.add(12))), all(ts::rd(field.add(19))));
        let (len, date) = (all(text.len() as u64), all(dates.date_s(field) as u64));
        let (wide_ok, wide_ms, wide_s) = dates.f1_wide(g1, g2, len, date);
        let (f1_ok, f1_ms, f1_s) = dates.f1(field, text.len());
        assert_eq!(wide_ok, if f1_ok { 0xFF } else { 0 }, "f1 {:?}", String::from_utf8_lossy(text));
        if f1_ok {
            assert_eq!((lane(wide_ms), lane(wide_s)), (f1_ms, f1_s), "f1 {:?}", String::from_utf8_lossy(text));
        }
        let (wide_ok, wide_ms) = dates.ts_wide(g0, g1, g2, g3, len, date);
        assert!(wide_ok == 0 || wide_ok == 0xFF);
        if wide_ok != 0 {
            assert_eq!((ok, ms), (true, lane(wide_ms)), "ts {:?}", String::from_utf8_lossy(text));
        }
        let plain_f3 = text.len() == 13 && text[0] != b'0' && text.iter().all(u8::is_ascii_digit);
        let dated = text.len() > 10 && text[10] == b'T';
        assert!(!((plain_f3 || dated) && ok) || wide_ok != 0, "ts not taken {:?}", String::from_utf8_lossy(text));
    }
}

#[test]
fn timestamps_match_the_rules() {
    let windows = [("2026-10-01T00:00:00Z", "2026-10-15T00:00:00Z"), ("2028-02-20T00:00:00Z", "2028-03-05T00:00:00Z"), ("2027-12-25T06:30:00Z", "2028-01-08T06:30:00Z")];
    let named = [
        "2026-10-03T23:58:10Z", "2026-10-03T19:58:10-04:00", "1791071890000", "0001791071890000", "2026-10-04T09:59:00+23:59",
        "2026-10-03T12:00:00-00:00", "2026-10-03T10:00:00+24:00", "2026-10-03T10:00:00+05:60", "2026-10-03T10:00:00+0200",
        "", "2026-10-03T23:58:10.000Z", "2026-10-03t23:58:10Z", "2026-10-03T23:58:10z", "2026-10-03 23:58:10Z", "2026-10-03T23:58:10",
        "1791071890000.0", "1.79e12", "-5", "2026-13-03T23:58:10Z", "2026-02-30T00:00:00Z", "2027-02-29T00:00:00Z", "2028-02-29T00:00:00Z",
        "2026-10-03T24:00:00Z", "2026-10-03T23:60:00Z", "2026-10-03T23:59:60Z", "86400000", "179102160000000000000", "00000000000000000000",
        "2028-02-29T12:00:00+12:00", "2028-03-01T00:30:00+01:00", "2027-12-31T23:59:59Z", "2028-01-01T00:00:00-23:59", "+1791071890000",
        "1791071890000 ", " 1791071890000", "\u{661}\u{667}\u{669}\u{661}\u{660}\u{667}\u{661}\u{668}\u{669}\u{660}\u{660}\u{660}\u{660}",
        "2026-10-03T23:58:10+00:00\r", "2026-10-03T23:58:10Z\r", "18446744073709551616", "9999999999999999999",
    ];
    let mut rng = Rng(42);
    for (start, end) in windows {
        let ctx = Ctx::new(parse_f1(start.as_bytes()).unwrap(), parse_f1(end.as_bytes()).unwrap(), &[], &[]).unwrap();
        let dates = Dates::new(&ctx);
        for text in named {
            check_ts(&dates, &ctx, text.as_bytes());
        }
        let offsets = [-1439, -600, -1, 0, 1, 330, 525, 720, 1439];
        let mut ms = ctx.origin - 3 * DAY;
        while ms < ctx.window_end + 3 * DAY {
            let offset = offsets[rng.below(offsets.len() as u64) as usize];
            let local = f1(ms + offset * 60_000);
            let sign = if offset < 0 { '-' } else { '+' };
            let f2 = format!("{}{sign}{:02}:{:02}", &local[..19], offset.abs() / 60, offset.abs() % 60);
            let exact = ms + rng.below(1000) as i64;
            for text in [f1(ms), f2, exact.to_string(), format!("00{exact}")] {
                check_ts(&dates, &ctx, text.as_bytes());
                for _ in 0..4 {
                    let mut bad = text.clone().into_bytes();
                    let at = rng.below(bad.len() as u64) as usize;
                    bad[at] = b"0123456789:-+TZtz. "[rng.below(19) as usize];
                    check_ts(&dates, &ctx, &bad);
                    check_ts(&dates, &ctx, &bad[..at]);
                }
            }
            // F1 strictly, as `ingest_ts` takes it, anywhere in the table.
            let (text, whole) = (f1(ms), ms - ms.rem_euclid(SECOND));
            let buf = padded(text.as_bytes());
            let (ok, rel, seconds) = unsafe { dates.f1(buf.as_ptr().add(1), text.len()) };
            if ms >= ctx.origin {
                assert_eq!((ok, rel, seconds * SECOND), (true, whole - ctx.origin, whole), "{text}");
            }
            ms += 3_599_000 + rng.below(2_000_000) as i64;
        }
    }
}

#[test]
fn prices_match_the_rules() {
    let cases: &[(&str, Option<u32>)] = &[
        ("1", Some(1)), ("0250", Some(250)), ("10000000", Some(10_000_000)), ("000000000001", Some(1)), ("00000010000000", Some(10_000_000)),
        ("12345678", None), ("00", None), ("0", None), ("", None), ("+500", None), ("1e3", None),
        ("19.99", None), ("-5", None), ("10000001", None), ("250000000", None), ("1000\r", None), (" 1", None), ("２５０", None),
    ];
    for &(text, want) in cases {
        let buf = padded(text.as_bytes());
        // SAFETY: a padded buffer has PAD bytes after the text.
        let (ok, cents) = unsafe { price_cents(buf.as_ptr().add(1), text.len()) };
        assert_eq!(ok.then_some(cents), want, "{text:?}");
    }
}

/// `tok-mask` against `tok-scalar` over the generated `dev/` dataset, at several chunk sizes.
#[test]
#[cfg(feature = "tok-mask")]
fn mask_matches_scalar_on_dev() {
    let dev = root().join("dev");
    if !dev.join("manifest.json").exists() {
        return;
    }
    let ctx = load_ctx(&dev).unwrap();
    let batch = Scratch::new().batch;
    let reference = parse_dir(&dev, &ctx, 1 << 20, batch, SCALAR);
    assert!(reference.events.len() > 900_000 && reference.clicks.len() > 100_000 && reference.links.len() > 10_000);
    assert!(parse_dir(&dev, &ctx, 4093, batch, SCALAR) == reference, "scalar, 4093-byte chunks");
    for chunk in [1 << 20, 64 << 10, 4093, 4 << 20] {
        assert!(parse_dir(&dev, &ctx, chunk, batch, MASK) == reference, "mask, {chunk}-byte chunks");
    }
    assert!(parse_dir(&dev, &ctx, 1 << 20, 4096, MASK) == reference, "mask, 4 KB batches");
}

/// Slot `scatter-wc`: the partitions hold the same records whether written plain or staged, with
/// any buffer size, on `dev/` when present and on a fixture with every kind of row otherwise.
#[test]
fn scatter_modes_agree() {
    let dev = root().join("dev");
    let data = if dev.join("manifest.json").exists() { dev } else { fixture("E1") };
    let ctx = load_ctx(&data).unwrap();
    let batch = Scratch::new().batch;
    let reference = parse_dir_scatter(&data, &ctx, 1 << 20, batch, SCALAR, None);
    for pick in PICKS {
        for scatter in [None, Some(1), Some(2), Some(4)] {
            assert!(parse_dir_scatter(&data, &ctx, 1 << 20, batch, *pick, scatter) == reference, "scatter {scatter:?}");
        }
    }
}
