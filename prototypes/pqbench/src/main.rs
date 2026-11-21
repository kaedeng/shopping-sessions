// Scratch benchmark: Polars 0.55.2 ParquetWriter vs arrow-rs parquet 60.1 for a 7M x 6 table.
use std::fs::File;
use std::path::Path;
use std::time::Instant;

use polars::prelude::*;
use polars_arrow::array::MutableBinaryViewArray;
use polars_parquet::write as ppw;

const CATS: [&str; 21] = [
    "electronics", "books", "toys", "garden", "grocery", "fashion", "beauty", "sports", "auto",
    "music", "movies", "games", "office", "pets", "baby", "health", "home", "tools", "jewelry",
    "shoes", "travel",
];
const ALPHA: &[u8; 36] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const THREADS: usize = 8;

pub struct Part {
    id: Vec<[u8; 9]>,
    day: Vec<i32>,
    a: Vec<u32>,
    b: Vec<u32>,
    c: Vec<u32>,
    cat: Vec<u8>, // 255 = null
}

fn gen_parts(n_rows: usize, n_parts: usize) -> Vec<Part> {
    let per = n_rows / n_parts;
    let mut s = 0x9E3779B97F4A7C15u64;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    (0..n_parts)
        .map(|_| {
            let mut p = Part { id: vec![], day: vec![], a: vec![], b: vec![], c: vec![], cat: vec![] };
            while p.day.len() < per {
                let mut x = next();
                let mut id = [0u8; 9];
                for b in &mut id {
                    *b = ALPHA[(x % 36) as usize];
                    x /= 36;
                }
                let k = 1 + (next() % 6) as usize; // ~3.5 rows per user => ~2M users in 7M rows
                let d0 = (next() % 15) as i32;
                for j in 0..k.min(per - p.day.len()) {
                    let r = next();
                    p.id.push(id);
                    p.day.push(20_000 + (d0 + j as i32) % 15);
                    p.a.push(1 + (r % 5) as u32);
                    p.b.push(1 + ((r >> 8) % 60) as u32);
                    p.c.push(((r >> 16) % 50_000) as u32);
                    p.cat.push(if (r >> 40) % 10 == 0 { 255 } else { ((r >> 44) % 21) as u8 });
                }
            }
            p
        })
        .collect()
}

fn cat<T: Copy>(parts: &[Part], f: impl Fn(&Part) -> &Vec<T>) -> Vec<T> {
    let mut v = Vec::with_capacity(parts.iter().map(|p| f(p).len()).sum());
    for p in parts {
        v.extend_from_slice(f(p));
    }
    v
}

fn build_df(parts: &[Part]) -> DataFrame {
    let n: usize = parts.iter().map(|p| p.day.len()).sum();
    let mut id = MutableBinaryViewArray::<str>::with_capacity(n);
    let mut ct = MutableBinaryViewArray::<str>::with_capacity(n);
    for p in parts {
        for s in &p.id {
            id.push_value_ignore_validity(unsafe { std::str::from_utf8_unchecked(s) });
        }
        for &c in &p.cat {
            ct.push((c != 255).then(|| CATS[c as usize]));
        }
    }
    let u = |name: &str, v: Vec<u32>| UInt32Chunked::from_vec(name.into(), v).into_series().into_column();
    DataFrame::new(
        n,
        vec![
            StringChunked::with_chunk("user_id".into(), id.freeze()).into_series().into_column(),
            Int32Chunked::from_vec("day".into(), cat(parts, |p| &p.day)).into_date().into_series().into_column(),
            u("sessions", cat(parts, |p| &p.a)),
            u("events", cat(parts, |p| &p.b)),
            u("revenue_cents", cat(parts, |p| &p.c)),
            StringChunked::with_chunk("top_category".into(), ct.freeze()).into_series().into_column(),
        ],
    )
    .unwrap()
}

fn pl_write(df: &mut DataFrame, path: &Path, comp: ParquetCompression, stats: StatisticsOptions, par: bool) {
    ParquetWriter::new(File::create(path).unwrap())
        .with_compression(comp)
        .with_statistics(stats)
        .set_parallel(par)
        .finish(df)
        .unwrap();
}

type Rg = (u64, Vec<Vec<ppw::CompressedPage>>);

// Low-level path: caller picks the encodings, pages are encoded eagerly on the calling thread.
fn pl_encode(df: &DataFrame, types: &[ppw::ParquetType], encs: &[ppw::Encoding]) -> Vec<Rg> {
    let opts = ppw::WriteOptions {
        statistics: StatisticsOptions::empty(),
        compression: ppw::CompressionOptions::Uncompressed,
        version: ppw::Version::V1,
        data_page_size: None,
    };
    df.iter_chunks(CompatLevel::newest(), false)
        .map(|rb| {
            let mut cols = vec![];
            for ((arr, ty), enc) in rb.columns().iter().zip(types).zip(encs) {
                for it in ppw::array_to_columns(arr, ty.clone(), opts, &[*enc]).unwrap() {
                    let it = it.map(|r| r.map_err(|e| polars_parquet::read::ParquetError::FeatureNotSupported(e.to_string())));
                    cols.push(ppw::Compressor::new_from_vec(it, opts.compression, vec![]).collect::<Result<Vec<_>, _>>().unwrap());
                }
            }
            (rb.len() as u64, cols)
        })
        .collect()
}

fn pl_batched(df: &DataFrame, path: &Path) -> polars::io::parquet::write::BatchedWriter<File> {
    ParquetWriter::new(File::create(path).unwrap())
        .with_compression(ParquetCompression::Uncompressed)
        .with_statistics(StatisticsOptions::empty())
        .batched(df.schema())
        .unwrap()
}

mod ar {
    use std::fs::File;
    use std::path::Path;
    use std::sync::Arc;

    use arrow_array::builder::StringBuilder;
    use arrow_array::{ArrayRef, Date32Array, RecordBatch, UInt32Array};
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::WriterProperties;

    use super::{CATS, Part, cat};

    pub fn build(parts: &[Part]) -> RecordBatch {
        let n: usize = parts.iter().map(|p| p.day.len()).sum();
        let mut id = StringBuilder::with_capacity(n, n * 9);
        let mut ct = StringBuilder::with_capacity(n, n * 8);
        for p in parts {
            for s in &p.id {
                id.append_value(unsafe { std::str::from_utf8_unchecked(s) });
            }
            for &c in &p.cat {
                ct.append_option((c != 255).then(|| CATS[c as usize]));
            }
        }
        let u = |f: fn(&Part) -> &Vec<u32>| Arc::new(UInt32Array::from(cat(parts, f))) as ArrayRef;
        let schema = Arc::new(Schema::new(vec![
            Field::new("user_id", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
            Field::new("sessions", DataType::UInt32, false),
            Field::new("events", DataType::UInt32, false),
            Field::new("revenue_cents", DataType::UInt32, false),
            Field::new("top_category", DataType::Utf8, true),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(id.finish()),
                Arc::new(Date32Array::from(cat(parts, |p| &p.day))),
                u(|p| &p.a),
                u(|p| &p.b),
                u(|p| &p.c),
                Arc::new(ct.finish()),
            ],
        )
        .unwrap()
    }

    pub fn write(rb: &RecordBatch, path: &Path, props: WriterProperties) {
        let mut w = ArrowWriter::try_new(File::create(path).unwrap(), rb.schema(), Some(props)).unwrap();
        w.write(rb).unwrap();
        w.close().unwrap();
    }
}

fn bench(name: &str, dir: &Path, f: impl Fn(&Path)) {
    let mut times = vec![];
    let mut size = 0;
    for _ in 0..5 {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let t = Instant::now();
        f(dir);
        times.push(t.elapsed().as_secs_f64());
        size = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum::<u64>();
    }
    times.sort_by(f64::total_cmp);
    println!("{name:<58} min {:.3}s  median {:.3}s  {:>4} MB", times[0], times[2], size >> 20);
}

// Run `f(thread_index, partitions_of_that_thread)` on THREADS threads.
fn per_thread<R: Send>(parts: &[Part], f: impl Fn(usize, &[Part]) -> R + Sync) -> Vec<R> {
    std::thread::scope(|s| {
        let hs: Vec<_> = parts
            .chunks(parts.len().div_ceil(THREADS))
            .enumerate()
            .map(|(i, g)| {
                let f = &f;
                s.spawn(move || f(i, g))
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("output dir"));
    let parts = gen_parts(7_000_000, 256);
    let parts = &parts[..];
    println!("rows {}  polars threads {}", parts.iter().map(|p| p.day.len()).sum::<usize>(), polars_core::runtime::RAYON.current_num_threads());

    use ParquetCompression as C;
    let stats = StatisticsOptions::default;
    let none = StatisticsOptions::empty;
    let zstd = |l| C::Zstd(Some(ppw::ZstdLevel::try_new(l).unwrap()));

    bench("build only: polars DataFrame, 8 threads", &dir, |_| drop(per_thread(parts, |_, g| build_df(g))));
    bench("build only: arrow-rs RecordBatch, 8 threads", &dir, |_| drop(per_thread(parts, |_, g| ar::build(g))));

    // --- Polars, one file, ParquetWriter::finish ---
    let one_file = |comp: C, st: StatisticsOptions| {
        move |d: &Path| {
            let mut dfs = per_thread(parts, |_, g| build_df(g)).into_iter();
            let mut df = dfs.next().unwrap();
            dfs.for_each(|o| {
                df.vstack_mut(&o).unwrap();
            });
            pl_write(&mut df, &d.join("part-0.parquet"), comp, st, true);
        }
    };
    bench("polars 1 file  DEFAULT (zstd3, stats, parallel)", &dir, one_file(C::default(), stats()));
    bench("polars 1 file  uncompressed, no stats, parallel", &dir, one_file(C::Uncompressed, none()));

    // --- Polars, one file per thread ---
    let n_files = |comp: C, st: StatisticsOptions, par: bool| {
        move |d: &Path| {
            per_thread(parts, |i, g| pl_write(&mut build_df(g), &d.join(format!("part-{i}.parquet")), comp, st, par));
        }
    };
    bench("polars 8 files DEFAULT (zstd3, stats), set_parallel(false)", &dir, n_files(C::default(), stats(), false));
    bench("polars 8 files zstd1, no stats", &dir, n_files(zstd(1), none(), false));
    bench("polars 8 files snappy, no stats", &dir, n_files(C::Snappy, none(), false));
    bench("polars 8 files lz4raw, no stats", &dir, n_files(C::Lz4Raw, none(), false));
    bench("polars 8 files uncompressed, default stats", &dir, n_files(C::Uncompressed, stats(), false));
    bench("polars 8 files uncompressed, no stats, set_parallel(false)", &dir, n_files(C::Uncompressed, none(), false));
    bench("polars 8 files uncompressed, no stats, set_parallel(true)", &dir, n_files(C::Uncompressed, none(), true));

    // --- Polars low level: explicit encodings ---
    use ppw::Encoding::{Plain as P, RleDictionary as D};
    let low = |encs: [ppw::Encoding; 6]| {
        move |d: &Path| {
            per_thread(parts, |i, g| {
                let df = build_df(g);
                let mut bw = pl_batched(&df, &d.join(format!("part-{i}.parquet")));
                let types = bw.parquet_schema().fields().to_vec();
                for (n, rg) in pl_encode(&df, &types, &encs) {
                    bw.write_row_group(n, &rg).unwrap();
                }
                bw.finish().unwrap();
            });
        }
    };
    bench("polars low-level 8 files, user_id PLAIN, rest dict", &dir, low([P, D, D, D, D, D]));
    bench("polars low-level 8 files, strings PLAIN, ints dict", &dir, low([P, D, D, D, D, P]));
    bench("polars low-level 8 files, all PLAIN", &dir, low([P; 6]));
    bench("polars low-level 1 file, 8 encoder threads, all PLAIN", &dir, |d| {
        let rgs = per_thread(parts, |_, g| {
            let df = build_df(g);
            let types = ppw::to_parquet_schema(&df.schema().to_arrow(CompatLevel::newest())).unwrap().fields().to_vec();
            (pl_encode(&df, &types, &[P; 6]), df.slice(0, 0))
        });
        let mut bw = pl_batched(&rgs[0].1, &d.join("part-0.parquet"));
        for (n, rg) in rgs.iter().flat_map(|r| &r.0) {
            bw.write_row_group(*n, rg).unwrap();
        }
        bw.finish().unwrap();
    });

    // --- arrow-rs parquet, one file per thread ---
    use parquet::basic::{Compression, ZstdLevel as AZ};
    use parquet::file::properties::{EnabledStatistics, WriterProperties};
    use parquet::schema::types::ColumnPath;
    let arw = |props: WriterProperties| {
        move |d: &Path| {
            per_thread(parts, |i, g| ar::write(&ar::build(g), &d.join(format!("part-{i}.parquet")), props.clone()));
        }
    };
    let b = WriterProperties::builder;
    let ns = || b().set_statistics_enabled(EnabledStatistics::None);
    bench("arrow-rs 8 files DEFAULT (uncompressed, page stats, dict)", &dir, arw(b().build()));
    bench("arrow-rs 8 files zstd3, default stats", &dir, arw(b().set_compression(Compression::ZSTD(AZ::try_new(3).unwrap())).build()));
    bench("arrow-rs 8 files uncompressed, no stats, dict", &dir, arw(ns().build()));
    bench("arrow-rs 8 files uncompressed, no stats, user_id no dict", &dir, arw(ns().set_column_dictionary_enabled(ColumnPath::from("user_id"), false).build()));
    bench("arrow-rs 8 files uncompressed, no stats, no dict at all", &dir, arw(ns().set_dictionary_enabled(false).build()));
    bench("arrow-rs 1 file, 1 thread, uncompressed, no stats, no dict", &dir, |d| {
        let rbs = per_thread(parts, |_, g| ar::build(g));
        let mut w = parquet::arrow::ArrowWriter::try_new(
            File::create(d.join("part-0.parquet")).unwrap(),
            rbs[0].schema(),
            Some(ns().set_dictionary_enabled(false).build()),
        )
        .unwrap();
        rbs.iter().for_each(|rb| w.write(rb).unwrap());
        w.close().unwrap();
    });

    // Sanity: the last arrow-rs output and a low-level polars output read back with the Polars reader.
    let check = |d: &Path| {
        let df = ParquetReader::new(File::open(d.join("part-0.parquet")).unwrap()).finish().unwrap();
        println!("readback: {} rows, schema {:?}, nulls in top_category {}", df.height(), df.schema(), df.column("top_category").unwrap().null_count());
    };
    check(&dir);
    low([P; 6])(&dir);
    check(&dir);
}
