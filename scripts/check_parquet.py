#!/usr/bin/env python3
"""
Reads two output folders with every Parquet reader installed (pyarrow, DuckDB, Polars) and
checks that each reader sees the same tables in both: same columns, same logical types, same
rows. For a writer that is not a library's (`out-direct`), against the library's (`out-arrow`):

  SS_OUT=arrow  DATA_DIR=... OUTPUT_DIR=/tmp/a ./shopping-sessions
  SS_OUT=direct DATA_DIR=... OUTPUT_DIR=/tmp/b ./shopping-sessions
  python check_parquet.py /tmp/a /tmp/b
"""
import glob
import sys

TABLES = {
    "user_daily": ["user_id", "day", "sessions", "events", "revenue_cents", "top_category"],
    "campaign_daily": ["campaign_id", "channel", "day", "sessions", "purchasing_sessions", "revenue_cents", "assisted_sessions"],
}


def with_pyarrow(folder, columns):
    import pyarrow as pa
    import pyarrow.parquet as pq

    files = sorted(glob.glob(f"{folder}/*.parquet"))
    for path in files:
        pq.ParquetFile(path).metadata  # the footer parses
    table = pa.concat_tables([pq.read_table(path) for path in files])
    assert table.column_names == columns, table.column_names
    kinds = []
    for field in table.schema:
        kind = field.type
        kinds.append("string" if pa.types.is_string(kind) or pa.types.is_large_string(kind) else "date" if pa.types.is_date(kind) else "int" if pa.types.is_integer(kind) else str(kind))
    rows = sorted(zip(*[column.to_pylist() for column in table.columns]), key=repr)
    return kinds, rows


def with_duckdb(folder, columns):
    import duckdb

    con = duckdb.connect()
    described = con.execute(f"describe select * from read_parquet('{folder}/*.parquet')").fetchall()
    assert [row[0] for row in described] == columns, described
    kinds = ["string" if row[1] == "VARCHAR" else "date" if row[1] == "DATE" else "int" if "INT" in row[1] else row[1] for row in described]
    rows = sorted(con.execute(f"select * from read_parquet('{folder}/*.parquet')").fetchall(), key=repr)
    return kinds, rows


def with_polars(folder, columns):
    import polars as pl

    frame = pl.read_parquet(f"{folder}/*.parquet")
    assert frame.columns == columns, frame.columns
    kinds = ["string" if kind == pl.String else "date" if kind == pl.Date else "int" if kind.is_integer() else str(kind) for kind in frame.dtypes]
    return kinds, sorted(frame.rows(), key=repr)


def main():
    a, b = sys.argv[1], sys.argv[2]
    failed = False
    for name, reader in [("pyarrow", with_pyarrow), ("duckdb", with_duckdb), ("polars", with_polars)]:
        try:
            __import__(name)
        except ImportError:
            print(f"{name}: not installed, skipped")
            continue
        for table, columns in TABLES.items():
            (kinds_a, rows_a), (kinds_b, rows_b) = reader(f"{a}/{table}", columns), reader(f"{b}/{table}", columns)
            same = kinds_a == kinds_b and rows_a == rows_b
            failed |= not same
            print(f"{name:<8} {table:<15} {'same' if same else 'DIFFERENT'}: {len(rows_a)} and {len(rows_b)} rows, types {kinds_b}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
