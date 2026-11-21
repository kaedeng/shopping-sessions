//! Slot `out-arrow`: one Parquet file per call with arrow-rs (the `parquet` crate): one row group,
//! PLAIN pages, uncompressed, no statistics, no dictionary, no Arrow schema in the footer.
//!
//! SLOT CONTRACT (every writer in this crate has exactly these two functions):
//!   pub fn user_daily(folder: &Path, worker: usize, ctx: &Ctx, rows: UserDaily) -> Result<(), String>
//!   pub fn campaign_daily(folder: &Path, ctx: &Ctx, rows: CampaignDaily) -> Result<(), String>

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{ArrowDictionaryKeyType, Date32Type, Int64Type, UInt8Type, UInt32Type};
use arrow_array::{ArrayRef, Date32Array, DictionaryArray, Int64Array, PrimitiveArray, RecordBatch, StringArray};
use arrow_buffer::{BooleanBuffer, Buffer, NullBuffer, OffsetBuffer};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_writer::{ArrowWriterOptions, compute_leaves};
use parquet::data_type::{Int32Type, Int64Type as ParquetInt64};
use parquet::file::properties::{EnabledStatistics, WriterProperties};

use types::Ctx;
use types::id::{self, Id, SLOW};
use types::rows::{CampaignDaily, NO_CATEGORY, UserDaily};

pub fn user_daily(folder: &Path, worker: usize, ctx: &Ctx, rows: UserDaily) -> Result<(), String> {
    let UserDaily { person, day, sessions, events, revenue_cents, top_category } = rows;
    let valid = BooleanBuffer::collect_bool(top_category.len(), |row| top_category[row] != NO_CATEGORY);
    let top_category = PrimitiveArray::<UInt8Type>::new(top_category.into(), Some(NullBuffer::new(valid)));
    write(
        &folder.join(format!("part-{worker}.parquet")),
        vec![
            ("user_id", Arc::new(ids(&person)?)),
            ("day", Arc::new(Date32Array::from(day))),
            ("sessions", Arc::new(Int64Array::from(sessions))),
            ("events", Arc::new(Int64Array::from(events))),
            ("revenue_cents", Arc::new(Int64Array::from(revenue_cents))),
            ("top_category", names(top_category, ctx.categories.iter().map(Vec::as_slice))?),
        ],
    )
}

pub fn campaign_daily(folder: &Path, ctx: &Ctx, rows: CampaignDaily) -> Result<(), String> {
    let CampaignDaily { campaign, day, sessions, purchasing_sessions, revenue_cents, assisted_sessions } = rows;
    let campaign = PrimitiveArray::<UInt32Type>::new(campaign.into(), None);
    write(
        &folder.join("part-0.parquet"),
        vec![
            ("campaign_id", names(campaign.clone(), ctx.campaigns.iter().map(|(id, _)| id.as_slice()))?),
            ("channel", names(campaign, ctx.campaigns.iter().map(|(_, channel)| channel.as_slice()))?),
            ("day", Arc::new(Date32Array::from(day))),
            ("sessions", Arc::new(Int64Array::from(sessions))),
            ("purchasing_sessions", Arc::new(Int64Array::from(purchasing_sessions))),
            ("revenue_cents", Arc::new(Int64Array::from(revenue_cents))),
            ("assisted_sessions", Arc::new(Int64Array::from(assisted_sessions))),
        ],
    )
}

/// The `user_id` column: 9 bytes stored per row whatever the ID's length and the end advanced by
/// the length, so the loop's only branch is the rare slow-path test.
fn ids(person: &[Id]) -> Result<StringArray, String> {
    let rows = person.len();
    let mut values: Vec<u8> = Vec::with_capacity(rows * 9);
    let mut offsets: Vec<i32> = Vec::with_capacity(rows + 1);
    let mut end = 0;
    // Invariant: capacity >= end + 9 * (rows left), so every 9-byte store fits.
    for (row, &id) in person.iter().enumerate() {
        unsafe { *offsets.as_mut_ptr().add(row) = end as i32 };
        if id & SLOW != 0 {
            end = unpack_slow(id, &mut values, end, rows - row);
            continue;
        }
        let (word, ninth, len) = id::spread(id);
        unsafe {
            let at = values.as_mut_ptr().add(end);
            (at as *mut u64).write_unaligned(word.to_le());
            *at.add(8) = ninth;
        }
        end += len;
    }
    if end > i32::MAX as usize {
        return Err(format!("user_id: {end} bytes in one file, at most 2 GiB supported"));
    }
    unsafe {
        *offsets.as_mut_ptr().add(rows) = end as i32;
        offsets.set_len(rows + 1);
        values.set_len(end);
        // The offsets only grow, and the input is UTF-8 by the rules.
        Ok(StringArray::new_unchecked(OffsetBuffer::new_unchecked(offsets.into()), Buffer::from_vec(values), None))
    }
}

#[cold]
#[inline(never)]
fn unpack_slow(id: Id, values: &mut Vec<u8>, end: usize, rows_left: usize) -> usize {
    unsafe { values.set_len(end) };
    id::unpack(id, values);
    values.reserve(rows_left * 9);
    values.len()
}

/// A string column given as indices into `names`: the writer looks each name up as it encodes it
/// PLAIN, so the column's strings are never built. The file holds plain strings, as from a
/// `StringArray`.
fn names<'a, K: ArrowDictionaryKeyType>(
    keys: PrimitiveArray<K>,
    names: impl Iterator<Item = &'a [u8]>,
) -> Result<ArrayRef, String> {
    let mut values = Vec::new();
    let mut offsets = vec![0i32];
    for name in names {
        values.extend_from_slice(name);
        offsets.push(values.len() as i32);
    }
    let names = StringArray::try_new(OffsetBuffer::new(offsets.into()), Buffer::from_vec(values), None)
        .map_err(|error| error.to_string())?;
    Ok(Arc::new(DictionaryArray::try_new(keys, Arc::new(names)).map_err(|error| error.to_string())?))
}

fn write(path: &Path, columns: Vec<(&str, ArrayRef)>) -> Result<(), String> {
    let fields: Vec<Field> = columns
        .iter()
        .map(|(name, column)| Field::new(*name, column.data_type().clone(), *name == "top_category"))
        .collect();
    let columns = columns.into_iter().map(|(_, column)| column).collect();
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
        .map_err(|error| error.to_string())
        .and_then(|batch| {
            let file = File::create(path).map_err(|error| error.to_string())?;
            encode(file, &batch).map_err(|error| error.to_string())
        })
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// `ArrowWriter::write` would gather every column through a `0..rows` index vector; the integer
/// columns go to the typed column writers as slices instead, the strings through arrow-rs.
fn encode<W: Write + Send>(sink: W, batch: &RecordBatch) -> parquet::errors::Result<()> {
    let properties = WriterProperties::builder()
        .set_dictionary_enabled(false)
        .set_statistics_enabled(EnabledStatistics::None)
        .set_offset_index_disabled(true)
        .build();
    let options = ArrowWriterOptions::new().with_properties(properties).with_skip_arrow_metadata(true);
    let (mut file, factory) = ArrowWriter::try_new_with_options(sink, batch.schema(), options)?.into_serialized_writer()?;
    // No rows, no row group: the file still has the full schema.
    if batch.num_rows() > 0 {
        let mut group = file.next_row_group()?;
        let schema = batch.schema();
        let strings = factory.create_column_writers(0)?;
        for ((column, field), mut string) in batch.columns().iter().zip(schema.fields()).zip(strings) {
            match column.data_type() {
                DataType::Int64 => {
                    let mut writer = group.next_column()?.expect("one writer per column");
                    writer.typed::<ParquetInt64>().write_batch(column.as_primitive::<Int64Type>().values(), None, None)?;
                    writer.close()?;
                }
                DataType::Date32 => {
                    let mut writer = group.next_column()?.expect("one writer per column");
                    writer.typed::<Int32Type>().write_batch(column.as_primitive::<Date32Type>().values(), None, None)?;
                    writer.close()?;
                }
                _ => {
                    for leaf in compute_leaves(field, column)? {
                        string.write(&leaf)?;
                    }
                    string.close()?.append_to_row_group(&mut group)?;
                }
            }
        }
        group.close()?;
    }
    file.close()?;
    Ok(())
}
