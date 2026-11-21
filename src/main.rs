//! Shopping Sessions: Polars (Rust) starter (skeleton).
//!
//! This runs end to end inside the scorer's container and writes both output tables
//! with the exact schema, but the tables are EMPTY, so the checker will mark it
//! incorrect. It gives you the plumbing; the rules are yours to implement.
//!
//! Reads  $DATA_DIR    (default /data)
//! Writes $OUTPUT_DIR  (default /output): user_daily/ and campaign_daily/ as Parquet
//! Temp   $SCRATCH_DIR (default /scratch)

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use polars::prelude::*;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn env_or(key: &str, default: &str) -> PathBuf {
    std::env::var_os(key).map_or_else(|| default.into(), PathBuf::from)
}

/// Every `*.csv` directly in `dir`, sorted. A missing folder is an error, not zero files.
fn csv_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    files.retain(|p| p.extension().is_some_and(|ext| ext == "csv"));
    files.sort();
    Ok(files)
}

fn main() -> Result<()> {
    let data = env_or("DATA_DIR", "/data");
    let out = env_or("OUTPUT_DIR", "/output");
    let _scratch = env_or("SCRATCH_DIR", "/scratch");

    let manifest: serde_json::Value =
        serde_json::from_reader(File::open(data.join("manifest.json"))?)?;
    println!("window: {} to {}", manifest["window_start"], manifest["window_end"]);
    println!("polars threads: {}", polars_core::runtime::RAYON.current_num_threads());

    let event_files = csv_files(&data.join("events"))?;
    let click_files = csv_files(&data.join("clicks"))?;
    let link_files = csv_files(&data.join("links"))?;
    println!(
        "{} event files, {} click files, {} link files",
        event_files.len(),
        click_files.len(),
        link_files.len()
    );

    // TODO: implement the rules.
    //   Links:  parse -> union-find -> name each person by the byte-wise smallest ID
    //   Events: parse -> dedup on event_id (smallest ingest_ts, before validation)
    //           -> validate -> drop late -> sessionize per person (gap > 30 min)
    //           -> day of session start -> categories
    //   Clicks: parse -> dedup -> validate -> credit each session to the latest
    //           click at or before its start, within 7 days (ties: smallest click_id)
    //           -> assists
    //
    // Careful with Polars' CSV reader: it handles malformed lines and quoting its own
    // way, which is not necessarily the spec's way (`\r`, stray quotes, field counts).
    // Check it against the fixtures. LazyFrame + streaming helps with memory at full
    // size; the container has about 24 GB and no swap.

    let user_daily = Schema::from_iter([
        Field::new("user_id".into(), DataType::String),
        Field::new("day".into(), DataType::Date),
        Field::new("sessions".into(), DataType::Int64),
        Field::new("events".into(), DataType::Int64),
        Field::new("revenue_cents".into(), DataType::Int64),
        Field::new("top_category".into(), DataType::String),
    ]);
    let campaign_daily = Schema::from_iter([
        Field::new("campaign_id".into(), DataType::String),
        Field::new("channel".into(), DataType::String),
        Field::new("day".into(), DataType::Date),
        Field::new("sessions".into(), DataType::Int64),
        Field::new("purchasing_sessions".into(), DataType::Int64),
        Field::new("revenue_cents".into(), DataType::Int64),
        Field::new("assisted_sessions".into(), DataType::Int64),
    ]);

    // Write both tables with the exact schema the checker expects.
    for (name, schema) in [("user_daily", &user_daily), ("campaign_daily", &campaign_daily)] {
        let folder = out.join(name);
        fs::create_dir_all(&folder)?;
        let mut df = DataFrame::empty_with_schema(schema);
        ParquetWriter::new(File::create(folder.join("part-0.parquet"))?).finish(&mut df)?;
    }
    println!("wrote empty user_daily and campaign_daily (skeleton)");
    Ok(())
}
