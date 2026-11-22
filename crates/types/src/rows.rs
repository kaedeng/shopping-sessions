//! The two output tables as columns, between the sessions stage and the writer.

use crate::id::Id;

/// `UserDaily::top_category` for null.
pub const NO_CATEGORY: u8 = 0xFF;

/// One worker's `user_daily` rows.
#[derive(Default)]
pub struct UserDaily {
    /// The person's name; the writer turns it into bytes (`id::spread`, `id::unpack`).
    pub person: Vec<Id>,
    /// Days since 1970-01-01.
    pub day: Vec<i32>,
    pub sessions: Vec<i64>,
    pub events: Vec<i64>,
    pub revenue_cents: Vec<i64>,
    /// Index into `Ctx::categories`, or `NO_CATEGORY`.
    pub top_category: Vec<u8>,
}

/// Rows a worker's table is given room for when its first rows come (`expect`).
static FIRST_ROOM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Says how many `user_daily` rows a worker may come to hold: `UserDaily::reserve` then asks
/// for that much room with the worker's first rows, once. Without it the six columns grow by
/// doubling, a bucket's rows at a time, and since they grow in turn none is the last block
/// its thread was given: every growth is a new block and a copy. At 20M events that was 100 MB
/// copied and 26,000 pages written for nothing but the copies, each one faulted in where the
/// machine has no huge pages. Room that is asked for and never written costs nothing.
pub fn expect(rows: usize) {
    FIRST_ROOM.store(rows, std::sync::atomic::Ordering::Relaxed);
}

impl UserDaily {
    /// Room for `rows` more rows in every column; for an empty table, also for the rows
    /// `expect` named.
    pub fn reserve(&mut self, rows: usize) {
        let rows = if self.person.capacity() == 0 { rows.max(FIRST_ROOM.load(std::sync::atomic::Ordering::Relaxed)) } else { rows };
        self.person.reserve(rows);
        self.day.reserve(rows);
        self.sessions.reserve(rows);
        self.events.reserve(rows);
        self.revenue_cents.reserve(rows);
        self.top_category.reserve(rows);
    }
}

/// `UserDailyNarrow::events` holds a count below this as it is; a row with more is listed in
/// `UserDailyNarrow::wide_events`. A file's dictionary has an entry for each count below it
/// (`output::direct`), so it is small: 1 row in 1,000 of generated data has more.
pub const EVENTS_HELD: u32 = 1024;
/// The same for `UserDailyNarrow::sessions`: a day has room for 48 sessions half an hour
/// apart, so no row has more unless the gap is made shorter.
pub const SESSIONS_HELD: u32 = 255;

/// One worker's `user_daily` rows the way `sessions::dense_walk` leaves them: 13 bytes a row
/// and 12 a person where `UserDaily` is 37 a row. A person's rows lie together, so the person
/// is said once; the day is counted from the window's first; and a count is as wide as the
/// counts are (at 20M events no row has over 22 sessions, and one in a thousand over 255
/// events). The writer puts the narrow columns into the file as they lie, as the indices of
/// a dictionary page that maps each number to itself (`output::direct`): half the bytes
/// `UserDaily`'s file has, on fewer fresh pages before that.
#[derive(Default)]
pub struct UserDailyNarrow {
    /// A person a run of rows: what `UserDaily::person` holds for each of them.
    pub person: Vec<Id>,
    /// The run's first row. The runs are in row order and together are all the rows.
    pub first: Vec<u32>,
    /// Days since `Ctx::day0`.
    pub day: Vec<u8>,
    /// The count, or anything for a row listed in `wide_sessions`.
    pub sessions: Vec<u8>,
    /// The count, or anything for a row listed in `wide_events`.
    pub events: Vec<u16>,
    pub revenue_cents: Vec<i64>,
    /// Index into `Ctx::categories`, or `NO_CATEGORY`.
    pub top_category: Vec<u8>,
    /// `(row, sessions)` of the rows with `SESSIONS_HELD` sessions or more, in row order.
    pub wide_sessions: Vec<(u32, u32)>,
    /// `(row, events)` of the rows with `EVENTS_HELD` events or more, in row order.
    pub wide_events: Vec<(u32, u32)>,
}

impl UserDailyNarrow {
    /// The rows so far.
    pub fn len(&self) -> usize {
        self.day.len()
    }

    pub fn is_empty(&self) -> bool {
        self.day.is_empty()
    }

    /// Room for `rows` more rows of `persons` more persons; for an empty table, also for the
    /// rows `expect` named (a person has a row at least).
    pub fn reserve(&mut self, rows: usize, persons: usize) {
        let first = if self.day.capacity() == 0 { FIRST_ROOM.load(std::sync::atomic::Ordering::Relaxed) } else { 0 };
        let (rows, persons) = (rows.max(first), persons.max(first));
        assert!(self.day.len() + rows <= u32::MAX as usize, "a worker's rows are numbered in 32 bits");
        self.person.reserve(persons);
        self.first.reserve(persons);
        self.day.reserve(rows);
        self.sessions.reserve(rows);
        self.events.reserve(rows);
        self.revenue_cents.reserve(rows);
        self.top_category.reserve(rows);
    }

    /// The rows as `UserDaily` has them, for a writer or a test that wants every column
    /// whole: `day0` is `Ctx::day0`.
    pub fn widen(&mut self, day0: i32) -> UserDaily {
        self.pieces(1).pop().expect("one piece").widen(day0)
    }

    /// The table as `count` stretches of about as many rows each, cut where a person's rows
    /// begin: each can be written as a file of its own, by any thread. (A worker's table is
    /// up to a third larger than another's, and one file a worker ended the output stage
    /// on the largest: 2 ms of 7 at 20M events.) A stretch may be empty.
    pub fn pieces(&mut self, count: usize) -> Vec<NarrowPiece<'_>> {
        fn within(list: &[(u32, u32)], from: usize, to: usize) -> &[(u32, u32)] {
            &list[list.partition_point(|&(row, _)| (row as usize) < from)..list.partition_point(|&(row, _)| (row as usize) < to)]
        }
        let rows = self.len();
        let mut pieces = Vec::with_capacity(count);
        let mut events = &mut self.events[..];
        let (mut run, mut row) = (0, 0);
        for piece in 1..=count {
            // Up to the first person at or past this stretch's share of the rows.
            let end_run = if piece == count { self.person.len() } else { run + self.first[run..].partition_point(|&first| (first as usize) < rows * piece / count) };
            let end_row = self.first.get(end_run).map_or(rows, |&first| first as usize);
            let (mine, rest) = std::mem::take(&mut events).split_at_mut(end_row - row);
            events = rest;
            pieces.push(NarrowPiece {
                person: &self.person[run..end_run],
                first: &self.first[run..end_run],
                from: row,
                day: &self.day[row..end_row],
                sessions: &self.sessions[row..end_row],
                events: mine,
                revenue_cents: &self.revenue_cents[row..end_row],
                top_category: &self.top_category[row..end_row],
                wide_sessions: within(&self.wide_sessions, row, end_row),
                wide_events: within(&self.wide_events, row, end_row),
            });
            (run, row) = (end_run, end_row);
        }
        pieces
    }
}

/// A stretch of a `UserDailyNarrow`: whole persons, and their rows.
pub struct NarrowPiece<'a> {
    pub person: &'a [Id],
    /// Each person's first row, numbered as in the table: `from` is the stretch's first.
    pub first: &'a [u32],
    pub from: usize,
    pub day: &'a [u8],
    pub sessions: &'a [u8],
    /// The writer may put what it likes where a row is listed in `wide_events`.
    pub events: &'a mut [u16],
    pub revenue_cents: &'a [i64],
    pub top_category: &'a [u8],
    /// The table's entries for this stretch's rows, the rows numbered as in the table.
    pub wide_sessions: &'a [(u32, u32)],
    pub wide_events: &'a [(u32, u32)],
}

impl NarrowPiece<'_> {
    /// The stretch's rows.
    pub fn len(&self) -> usize {
        self.day.len()
    }

    pub fn is_empty(&self) -> bool {
        self.day.is_empty()
    }

    /// The rows as `UserDaily` has them: `day0` is `Ctx::day0`.
    pub fn widen(&self, day0: i32) -> UserDaily {
        let rows = self.len();
        let mut person = Vec::with_capacity(rows);
        for (run, &id) in self.person.iter().enumerate() {
            let end = self.first.get(run + 1).map_or(rows, |&next| next as usize - self.from);
            person.resize(end, id);
        }
        debug_assert_eq!(person.len(), rows);
        let mut sessions: Vec<i64> = self.sessions.iter().map(|&count| i64::from(count)).collect();
        for &(row, count) in self.wide_sessions {
            sessions[row as usize - self.from] = i64::from(count);
        }
        let mut events: Vec<i64> = self.events.iter().map(|&count| i64::from(count)).collect();
        for &(row, count) in self.wide_events {
            events[row as usize - self.from] = i64::from(count);
        }
        UserDaily { person, day: self.day.iter().map(|&day| day0 + i32::from(day)).collect(), sessions, events, revenue_cents: self.revenue_cents.to_vec(), top_category: self.top_category.to_vec() }
    }
}

/// All of `campaign_daily`.
#[derive(Default)]
pub struct CampaignDaily {
    /// Index into `Ctx::campaigns`, whose last entry is `organic`.
    pub campaign: Vec<u32>,
    pub day: Vec<i32>,
    pub sessions: Vec<i64>,
    pub purchasing_sessions: Vec<i64>,
    pub revenue_cents: Vec<i64>,
    pub assisted_sessions: Vec<i64>,
}
