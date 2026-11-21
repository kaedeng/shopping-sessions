#!/usr/bin/env python3
"""Exact comparison of two output folders (user_daily/ and campaign_daily/ as Parquet): for one
pipeline against another on the same data (scripts/pos_check.sh). Needs polars.
Usage: compare_outputs.py OUT_A OUT_B"""
import sys

import polars as pl

a, b = sys.argv[1], sys.argv[2]
ok = True
for table in ("user_daily", "campaign_daily"):
    x = pl.read_parquet(f"{a}/{table}/*.parquet")
    y = pl.read_parquet(f"{b}/{table}/*.parquet")
    cols = sorted(x.columns)
    if sorted(y.columns) != cols:
        print(f"{table}: columns differ: {x.columns} vs {y.columns}")
        ok = False
        continue
    x, y = x.select(cols).sort(cols), y.select(cols).sort(cols)
    if x.equals(y):
        print(f"{table}: MATCH ({x.height:,} rows)")
    else:
        ok = False
        print(f"{table}: DIFFER ({x.height:,} vs {y.height:,} rows)")
        only_a = x.join(y, on=cols, how="anti", nulls_equal=True)
        only_b = y.join(x, on=cols, how="anti", nulls_equal=True)
        print(f"  only in first: {only_a.height:,}; only in second: {only_b.height:,}")
        print(only_a.head(8))
        print(only_b.head(8))
sys.exit(0 if ok else 1)
