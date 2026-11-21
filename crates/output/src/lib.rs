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
        return direct::user_daily(folder, worker, ctx, rows);
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
