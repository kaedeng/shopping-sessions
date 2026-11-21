"""Reads the roundtrip test's files with Polars, as scripts/run_fixtures.py's load_actual does.

python polars_check.py target/agent-output/tmp/output-roundtrip
"""
import sys
from pathlib import Path

import polars as pl

pl.Config.set_tbl_rows(12)
for folder in sorted(Path(sys.argv[1]).iterdir()):
    for path in sorted(folder.glob("*.parquet")):
        frame = pl.read_parquet(path)
        print(f"{folder.name}/{path.name}: {frame.height} rows, {frame.schema}")
        print(frame.head(8))
        print("nulls:", frame.null_count().row(0, named=True))
