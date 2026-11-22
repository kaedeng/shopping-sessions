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

impl UserDaily {
    /// Room for `rows` more rows in every column.
    pub fn reserve(&mut self, rows: usize) {
        self.person.reserve(rows);
        self.day.reserve(rows);
        self.sessions.reserve(rows);
        self.events.reserve(rows);
        self.revenue_cents.reserve(rows);
        self.top_category.reserve(rows);
    }

    /// Slot `free-early`: every column's pages back to the kernel once the rows are written,
    /// on the writing worker, and not on the one thread that exits (`mem::release`).
    ///
    /// # Safety
    /// Nothing reads the rows again.
    pub unsafe fn release(&self) {
        fn column<T>(values: &Vec<T>) {
            // SAFETY: the vector's own allocation; the caller's contract.
            unsafe { crate::mem::release(values.as_ptr().cast(), values.capacity() * size_of::<T>()) };
        }
        if crate::mem::free_part("rows") {
            column(&self.person);
            column(&self.day);
            column(&self.sessions);
            column(&self.events);
            column(&self.revenue_cents);
            column(&self.top_category);
        }
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
