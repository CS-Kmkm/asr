from __future__ import annotations

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

_SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "summarize_takes.py"


def _load():
    spec = importlib.util.spec_from_file_location("summarize_takes", _SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


takes = _load()

# The take_spans columns this script reads, as of schema version 5.
SCHEMA = """CREATE TABLE take_spans (
  id INTEGER PRIMARY KEY AUTOINCREMENT, take_id TEXT NOT NULL UNIQUE,
  version INTEGER NOT NULL, created_at TEXT NOT NULL, mode TEXT,
  outcome TEXT NOT NULL, stage TEXT, record_ready_ms INTEGER,
  stop_to_delivery_ms INTEGER, stop_to_final_ms INTEGER, route TEXT, key_presses INTEGER)"""


def _database(directory: str, rows: list[tuple]) -> Path:
    path = Path(directory) / "takes.sqlite3"
    connection = sqlite3.connect(path)
    connection.execute(SCHEMA)
    connection.executemany(
        "INSERT INTO take_spans(take_id, version, created_at, mode, outcome, stage, record_ready_ms,"
        " stop_to_final_ms, stop_to_delivery_ms, route, key_presses) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        rows,
    )
    connection.commit()
    connection.close()
    return path


class SummarizeTakesTest(unittest.TestCase):
    def test_nearest_rank_matches_the_in_app_diagnostics(self) -> None:
        self.assertEqual(takes.nearest_rank([30, 10, 20, 40], 0.5), 20)
        self.assertEqual(takes.nearest_rank([30, 10, 20, 40], 0.95), 40)
        self.assertIsNone(takes.nearest_rank([], 0.5))
        self.assertEqual(takes.nearest_rank([0], 0.5), 0)

    def test_cohorts_by_route_and_mode_with_rates_and_percentiles(self) -> None:
        rows = [
            ("a", 1, "2026-11-01T00:00:00+00:00", "edit", "pasted", None, 100, 400, 900, "edit", 2),
            ("b", 1, "2026-11-01T00:00:01+00:00", "edit", "pasted", None, 120, 500, 1100, "edit", 1),
            ("c", 1, "2026-11-01T00:00:02+00:00", "dictate", "clipboard_only", None, 90, 300, 350, "copy_only", 2),
            ("d", 1, "2026-11-01T00:00:03+00:00", "dictate", "cancelled", "recording", 80, None, None, "dictate", 2),
            ("e", 1, "2026-11-01T00:00:04+00:00", "dictate", "pasted", None, 70, 200, 260, None, 2),
            # Another record version is never mixed in.
            ("f", 2, "2026-11-01T00:00:05+00:00", "dictate", "pasted", None, 1, 1, 1, None, 9),
            # Unknown codes are not printed as stored.
            ("g", 1, "2026-11-01T00:00:06+00:00", "private-mode", "insertion_failed", None, 1, 1, None, "private-route", 1),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = _database(directory, rows)
            connection = sqlite3.connect(path)
            report = takes.summarize(takes.load_takes(connection))
            connection.close()
        by_key = {(entry["route"], entry["mode"]): entry for entry in report}
        self.assertEqual(set(by_key), {("edit", "edit"), ("copy_only", "dictate"), ("dictate", "dictate"),
                                       ("none", "dictate"), ("other", "unknown")})
        edit = by_key[("edit", "edit")]
        self.assertEqual(edit["takes"], 2)
        self.assertEqual(edit["success_rate"], 1.0)
        self.assertEqual(edit["stop_to_delivery_ms"], {"n": 2, "missing": 0, "p50": 900, "p95": 1100})
        self.assertEqual(edit["key_presses"]["p50"], 1)
        copy = by_key[("copy_only", "dictate")]
        self.assertEqual(copy["clipboard_only_rate"], 1.0)
        # Stop -> confirmed paste counts pasted takes only.
        self.assertEqual(copy["stop_to_delivery_ms"]["n"], 0)
        self.assertEqual(copy["stop_to_final_ms"]["p50"], 300)
        cancelled = by_key[("dictate", "dictate")]
        self.assertEqual(cancelled["cancel_rate"], 1.0)
        self.assertEqual(cancelled["stop_to_final_ms"], {"n": 0, "missing": 1, "p50": None, "p95": None})
        self.assertEqual(by_key[("other", "unknown")]["failure_rate"], 1.0)
        self.assertNotIn("private", json.dumps(report))

    def test_older_databases_without_new_columns_report_them_missing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "old.sqlite3"
            connection = sqlite3.connect(path)
            connection.execute("CREATE TABLE take_spans (id INTEGER PRIMARY KEY, take_id TEXT, version INTEGER,"
                               " created_at TEXT, mode TEXT, outcome TEXT, stage TEXT, record_ready_ms INTEGER,"
                               " stop_to_delivery_ms INTEGER)")
            connection.execute("INSERT INTO take_spans VALUES (1, 'x', 1, '2026-01-01', 'dictate', 'pasted', NULL, 50, 300)")
            connection.commit()
            report = takes.summarize(takes.load_takes(connection))
            connection.close()
        self.assertEqual(report[0]["route"], "none")
        self.assertEqual(report[0]["stop_to_final_ms"]["missing"], 1)
        self.assertEqual(report[0]["record_ready_ms"]["p50"], 50)

    def test_cli_reads_read_only_and_filters_by_time(self) -> None:
        rows = [
            ("a", 1, "2026-11-01T00:00:00+00:00", "dictate", "pasted", None, 10, 20, 30, "dictate", 2),
            ("b", 1, "2026-11-02T00:00:00+00:00", "dictate", "pasted", None, 11, 21, 31, "dictate", 2),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = _database(directory, rows)
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                code = takes.main(["--db", str(path), "--since", "2026-11-02", "--json"])
            self.assertEqual(code, 0)
            report = json.loads(output.getvalue())
            self.assertEqual(report[0]["takes"], 1)
            table = io.StringIO()
            with contextlib.redirect_stdout(table):
                self.assertEqual(takes.main(["--db", str(path)]), 0)
            self.assertIn("stop_to_delivery_ms.p50", table.getvalue())
            missing = io.StringIO()
            with contextlib.redirect_stderr(missing):
                self.assertEqual(takes.main(["--db", str(Path(directory) / "none.sqlite3")]), 2)


if __name__ == "__main__":
    unittest.main()
