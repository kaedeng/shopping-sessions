//! Both tables written through the contract and read back with the parquet crate's Arrow reader.
//! The files stay in `target/.../tmp/output-roundtrip/` for `polars_check.py`.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow_array::{ArrayRef, Date32Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{Field, Schema};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::LogicalType;

use types::rows::{CampaignDaily, NO_CATEGORY, UserDaily};
use types::time::DAY;
use types::{Ctx, id};

fn ctx() -> Ctx {
    let products = [(id::pack(b"p1"), b"shoes".as_slice()), (id::pack(b"p2"), b"books")];
    let campaigns = [(b"c_1".as_slice(), b"search".as_slice()), (b"c_2", b"social")];
    Ctx::new(20_000 * DAY, 20_014 * DAY, &products, &campaigns).unwrap()
}

/// The file's only batch (an empty one when it has no rows), and its columns' Parquet logical types.
fn read(path: &Path) -> (RecordBatch, Vec<Option<LogicalType>>) {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap()).unwrap().with_batch_size(1 << 20);
    let logical = builder.parquet_schema().columns().iter().map(|c| c.logical_type_ref().cloned()).collect();
    let schema = builder.schema().clone();
    let mut batches: Vec<RecordBatch> = builder.build().unwrap().collect::<Result<_, _>>().unwrap();
    assert!(batches.len() <= 1);
    (batches.pop().unwrap_or_else(|| RecordBatch::new_empty(schema)), logical)
}

fn batch(columns: Vec<(&str, ArrayRef, bool)>) -> RecordBatch {
    let fields: Vec<Field> = columns.iter().map(|(name, c, nullable)| Field::new(*name, c.data_type().clone(), *nullable)).collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns.into_iter().map(|c| c.1).collect()).unwrap()
}

fn user_rows(ids: &[Vec<u8>], categories: &[u8]) -> UserDaily {
    let n = ids.len() as i64;
    UserDaily {
        person: ids.iter().map(|bytes| id::pack(bytes)).collect(),
        day: (0..n).map(|row| 20_000 + (row % 14) as i32).collect(),
        sessions: (0..n).map(|row| 1 + row).collect(),
        events: (0..n).map(|row| 3 * row + 1).collect(),
        revenue_cents: (0..n).map(|row| (row << 33) + 7).collect(),
        top_category: categories.to_vec(),
    }
}

fn user_expected(ctx: &Ctx, rows: &UserDaily, ids: &[Vec<u8>]) -> RecordBatch {
    let name = |c: u8| (c != NO_CATEGORY).then(|| std::str::from_utf8(&ctx.categories[c as usize]).unwrap());
    batch(vec![
        ("user_id", Arc::new(StringArray::from_iter_values(ids.iter().map(|b| std::str::from_utf8(b).unwrap()))), false),
        ("day", Arc::new(Date32Array::from(rows.day.clone())), false),
        ("sessions", Arc::new(Int64Array::from(rows.sessions.clone())), false),
        ("events", Arc::new(Int64Array::from(rows.events.clone())), false),
        ("revenue_cents", Arc::new(Int64Array::from(rows.revenue_cents.clone())), false),
        ("top_category", Arc::new(rows.top_category.iter().map(|&c| name(c)).collect::<StringArray>()), true),
    ])
}

#[test]
fn both_tables_read_back_exactly() {
    let ctx = ctx();
    assert_eq!(ctx.categories, [b"books".to_vec(), b"shoes".to_vec(), b"unknown".to_vec()]);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("output-roundtrip");
    let _ = std::fs::remove_dir_all(&root);
    let (users, campaigns, no_campaigns) = (root.join("user_daily"), root.join("campaign_daily"), root.join("campaign_empty"));
    for folder in [&users, &campaigns, &no_campaigns] {
        std::fs::create_dir_all(folder).unwrap();
    }

    // Worker 0: 9-byte, 1-byte and tab IDs, and two slow-path ones (10 bytes; non-ASCII) between them.
    let small: Vec<Vec<u8>> = [&b"user_id_a"[..], b"0123456789", b"u", b"A\tB", "\u{fc}ser".as_bytes(), b"zzzzzzzzz"]
        .iter()
        .map(|b| b.to_vec())
        .collect();
    assert_eq!(id::pack(&small[1]) & id::SLOW, id::SLOW);
    assert_eq!(id::pack(&small[4]) & id::SLOW, id::SLOW);
    assert_eq!(id::pack(&small[3]) & id::SLOW, 0);
    let small_rows = user_rows(&small, &[0, NO_CATEGORY, 2, 1, NO_CATEGORY, 0]);
    // Worker 2: many pages, a slow ID every 997 rows and a null every 10.
    let many: Vec<Vec<u8>> = (0..50_000)
        .map(|row| if row % 997 == 0 { format!("long_user_{row}") } else { format!("u{row}") }.into_bytes())
        .collect();
    let many_categories: Vec<u8> = (0..50_000).map(|row| if row % 10 == 0 { NO_CATEGORY } else { (row % 3) as u8 }).collect();
    let many_rows = user_rows(&many, &many_categories);
    let expected = [user_expected(&ctx, &small_rows, &small), user_expected(&ctx, &UserDaily::default(), &[]), user_expected(&ctx, &many_rows, &many)];

    for (worker, rows) in [small_rows, UserDaily::default(), many_rows].into_iter().enumerate() {
        output::user_daily(&users, worker, &ctx, rows).unwrap();
    }
    for (worker, expected) in expected.iter().enumerate() {
        let (actual, logical) = read(&users.join(format!("part-{worker}.parquet")));
        assert_eq!(&actual, expected, "worker {worker}");
        assert_eq!(logical[0], Some(LogicalType::String));
        assert_eq!(logical[1], Some(LogicalType::Date));
        assert_eq!(logical[5], Some(LogicalType::String));
    }

    let rows = CampaignDaily {
        campaign: vec![ctx.organic(), 0, 1, 0],
        day: vec![20_000, 20_000, 20_003, 20_013],
        sessions: vec![5, 2, 0, 1],
        purchasing_sessions: vec![1, 0, 0, 1],
        revenue_cents: vec![12_345, 0, 0, 1 << 40],
        assisted_sessions: vec![0, 3, 4, 0],
    };
    let expected = |rows: &CampaignDaily, campaign_ids: &[&str], channels: &[&str]| {
        batch(vec![
            ("campaign_id", Arc::new(StringArray::from(campaign_ids.to_vec())), false),
            ("channel", Arc::new(StringArray::from(channels.to_vec())), false),
            ("day", Arc::new(Date32Array::from(rows.day.clone())), false),
            ("sessions", Arc::new(Int64Array::from(rows.sessions.clone())), false),
            ("purchasing_sessions", Arc::new(Int64Array::from(rows.purchasing_sessions.clone())), false),
            ("revenue_cents", Arc::new(Int64Array::from(rows.revenue_cents.clone())), false),
            ("assisted_sessions", Arc::new(Int64Array::from(rows.assisted_sessions.clone())), false),
        ])
    };
    let full = expected(&rows, &["organic", "c_1", "c_2", "c_1"], &["organic", "search", "social", "search"]);
    let empty = expected(&CampaignDaily::default(), &[], &[]);
    output::campaign_daily(&campaigns, 0, &ctx, rows).unwrap();
    output::campaign_daily(&no_campaigns, 0, &ctx, CampaignDaily::default()).unwrap();
    for (folder, expected) in [(&campaigns, full), (&no_campaigns, empty)] {
        let (actual, logical) = read(&folder.join("part-0.parquet"));
        assert_eq!(actual, expected);
        assert_eq!(logical[..3], [Some(LogicalType::String), Some(LogicalType::String), Some(LogicalType::Date)]);
    }
}
