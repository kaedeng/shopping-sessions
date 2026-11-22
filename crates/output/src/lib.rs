//! The two tables as Parquet (decision: Output).
//!
//! CONTRACT (the binary depends on exactly this; the bodies are the implementer's):

use std::path::Path;

use types::Ctx;
use types::rows::{CampaignDaily, UserDaily};

#[cfg(feature = "out-arrow")]
mod arrow;
#[cfg(feature = "out-direct")]
mod direct;

/// Writes one worker's rows to `folder/part-{worker}.parquet`, also when there are none.
/// `folder` exists. Called concurrently, one call per worker.
///
/// `SS_OUT=direct|arrow`: written by hand (`direct`, the pick), or through arrow-rs.
pub fn user_daily(folder: &Path, worker: usize, ctx: &Ctx, rows: UserDaily) -> Result<(), String> {
    #[cfg(feature = "out-direct")]
    if types::env::pick("SS_OUT", &["direct", "arrow"]) == 0 {
        return direct::user_daily(folder, worker, ctx, rows, None);
    }
    arrow::user_daily(folder, worker, ctx, rows)
}

/// `user_daily` for rows whose `person` is not a name but the caller's key for one:
/// `text_of(key, out)` appends the name's bytes. Called once per run of equal keys, on the
/// writing thread. (A name of 33 bytes as an `Id` is an entry of the intern tables; one in
/// eight names is that long, and the pipeline for opaque IDs has their bytes elsewhere.)
pub fn user_daily_named(folder: &Path, worker: usize, ctx: &Ctx, mut rows: UserDaily, text_of: &(dyn Fn(types::Id, &mut Vec<u8>) + Sync)) -> Result<(), String> {
    #[cfg(feature = "out-direct")]
    if types::env::pick("SS_OUT", &["direct", "arrow"]) == 0 {
        return direct::user_daily(folder, worker, ctx, rows, Some(text_of));
    }
    // Through arrow-rs: the names as `Id`s first.
    let (mut key, mut name, mut text) = (0, 0, Vec::new());
    for id in rows.person.iter_mut() {
        if *id != key {
            text.clear();
            text_of(*id, &mut text);
            (key, name) = (*id, types::id::pack(&text));
        }
        *id = name;
    }
    arrow::user_daily(folder, worker, ctx, rows)
}

/// Writes `rows`, the whole table or a part of it, to `folder/part-{part}.parquet`, also when
/// there are none. `folder` exists. Called concurrently, one call per part.
///
/// `SS_OUT=direct|arrow`, as `user_daily`.
pub fn campaign_daily(folder: &Path, part: usize, ctx: &Ctx, rows: CampaignDaily) -> Result<(), String> {
    #[cfg(feature = "out-direct")]
    if types::env::pick("SS_OUT", &["direct", "arrow"]) == 0 {
        return direct::campaign_daily(folder, part, ctx, &rows);
    }
    arrow::campaign_daily(folder, part, ctx, rows)
}
