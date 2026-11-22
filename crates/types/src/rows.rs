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
