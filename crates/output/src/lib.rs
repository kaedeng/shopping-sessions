//! The two tables as Parquet (decision: Output).
//!
//! CONTRACT (the binary depends on exactly this; the bodies are the implementer's):

use std::path::Path;

use types::Ctx;
use types::rows::{CampaignDaily, UserDaily};

#[cfg(feature = "out-arrow")]
mod arrow;

/// Writes one worker's rows to `folder/part-{worker}.parquet`, also when there are none.
/// `folder` exists. Called concurrently, one call per worker.
pub fn user_daily(folder: &Path, worker: usize, ctx: &Ctx, rows: UserDaily) -> Result<(), String> {
    arrow::user_daily(folder, worker, ctx, rows)
}

/// Writes the whole table to `folder/part-0.parquet`, also when it is empty. `folder` exists.
pub fn campaign_daily(folder: &Path, ctx: &Ctx, rows: CampaignDaily) -> Result<(), String> {
    arrow::campaign_daily(folder, ctx, rows)
}
