#!/usr/bin/env python3
"""Summarize per-take latency and outcomes from the app's `take_spans` table.

Reads the local app database read-only and prints, per cohort (Smart route,
processed mode), the take count, outcome rates and nearest-rank p50/p95 of
hotkey->record-ready, stop->final transcript, stop->confirmed paste and the
shortcut key presses per take. Only fixed codes and numbers are read; the
table holds no text, and take ids are never printed.

Usage:
    uv run scripts/summarize_takes.py [--db PATH] [--since ISO8601] [--json]

Without --db the release database under %APPDATA% is used; pass --dev for a
debug build's database. Only the standard library is used.
"""

from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import sqlite3
import sys
from typing import Any

APP_IDENTIFIER = "org.localvoice.input"
DATABASE_FILE_NAME = "local-voice-input.sqlite3"
DEBUG_SUBDIRECTORY = "dev"
TAKE_SPAN_VERSION = 1

# (report name, column). Stop->confirmed paste uses only `pasted` takes.
METRICS = (
    ("record_ready_ms", "record_ready_ms"),
    ("stop_to_final_ms", "stop_to_final_ms"),
    ("stop_to_delivery_ms", "stop_to_delivery_ms"),
    ("key_presses", "key_presses"),
)
SUCCESS = "pasted"
CLIPBOARD_ONLY = ("clipboard_only", "paste_unverified")
CANCELLED = "cancelled"
ROUTES = ("edit", "dictate", "copy_only")
MODES = ("dictate", "translate", "edit", "ask", "selected_text")


def default_database(dev: bool) -> Path:
    base = Path(os.environ.get("APPDATA", Path.home() / "AppData" / "Roaming")) / APP_IDENTIFIER
    return (base / DEBUG_SUBDIRECTORY if dev else base) / DATABASE_FILE_NAME


def nearest_rank(values: list[float], fraction: float) -> float | None:
    """The same nearest-rank percentile the in-app diagnostics use."""
    ordered = sorted(value for value in values if value is not None and value >= 0)
    if not ordered:
        return None
    rank = max(1, math.ceil(fraction * len(ordered)))
    return ordered[rank - 1]


def _columns(connection: sqlite3.Connection) -> set[str]:
    return {row[1] for row in connection.execute("PRAGMA table_info(take_spans)")}


def load_takes(connection: sqlite3.Connection, since: str | None = None) -> list[dict[str, Any]]:
    present = _columns(connection)
    if not present:
        raise ValueError("this database has no take_spans table; record takes with a current build")
    wanted = ["mode", "outcome", "stage", "route", "created_at"] + [column for _, column in METRICS]
    select = ", ".join(column if column in present else f"NULL AS {column}" for column in wanted)
    query = f"SELECT {select} FROM take_spans WHERE version = ?"
    parameters: list[Any] = [TAKE_SPAN_VERSION]
    if since:
        query += " AND created_at >= ?"
        parameters.append(since)
    connection.row_factory = sqlite3.Row
    rows = [dict(row) for row in connection.execute(query + " ORDER BY id", parameters)]
    for row in rows:
        # Only fixed codes are reported, like the in-app diagnostics.
        row["mode"] = row["mode"] if row["mode"] in MODES else None
        if row["route"] is not None and row["route"] not in ROUTES:
            row["route"] = "other"
    return rows


def summarize(takes: list[dict[str, Any]]) -> list[dict[str, Any]]:
    cohorts: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for take in takes:
        key = (take["route"] or "none", take["mode"] or "unknown")
        cohorts.setdefault(key, []).append(take)
    report = []
    for (route, mode), rows in sorted(cohorts.items()):
        count = len(rows)
        outcomes: dict[str, int] = {}
        for row in rows:
            outcomes[row["outcome"]] = outcomes.get(row["outcome"], 0) + 1
        entry: dict[str, Any] = {
            "route": route,
            "mode": mode,
            "takes": count,
            "outcomes": dict(sorted(outcomes.items())),
            "success_rate": outcomes.get(SUCCESS, 0) / count,
            "clipboard_only_rate": sum(outcomes.get(code, 0) for code in CLIPBOARD_ONLY) / count,
            "cancel_rate": outcomes.get(CANCELLED, 0) / count,
            "failure_rate": sum(
                n for code, n in outcomes.items()
                if code not in (SUCCESS, CANCELLED, "panel", "search", *CLIPBOARD_ONLY)
            ) / count,
        }
        for name, column in METRICS:
            source = [row for row in rows if row["outcome"] == SUCCESS] if column == "stop_to_delivery_ms" else rows
            values = [row[column] for row in source if row[column] is not None]
            entry[name] = {
                "n": len(values),
                "missing": len(source) - len(values),
                "p50": nearest_rank(values, 0.50),
                "p95": nearest_rank(values, 0.95),
            }
        report.append(entry)
    return report


def _format(value: Any) -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:.0%}" if value <= 1 else f"{value:.0f}"
    return str(value)


def render(report: list[dict[str, Any]]) -> str:
    header = ["route", "mode", "takes", "success", "clip_only", "cancel", "fail"]
    for name, _ in METRICS:
        header += [f"{name}.p50", f"{name}.p95", f"{name}.n"]
    lines = ["\t".join(header)]
    for entry in report:
        cells = [entry["route"], entry["mode"], entry["takes"], entry["success_rate"],
                 entry["clipboard_only_rate"], entry["cancel_rate"], entry["failure_rate"]]
        for name, _ in METRICS:
            cells += [entry[name]["p50"], entry[name]["p95"], entry[name]["n"]]
        lines.append("\t".join(_format(cell) for cell in cells))
    lines.append("stop_to_delivery_ms counts pasted takes only (stop -> confirmed paste). "
                 "key_presses counts shortcut presses; a clipboard-only take also needs a manual Ctrl+V.")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--db", type=Path, help="database path (default: the app's release database)")
    parser.add_argument("--dev", action="store_true", help="use the debug build's database")
    parser.add_argument("--since", help="only takes created at or after this ISO 8601 time (UTC)")
    parser.add_argument("--json", action="store_true", help="print JSON instead of a table")
    args = parser.parse_args(argv)
    path = args.db or default_database(args.dev)
    if not path.is_file():
        print(f"database not found: {path}", file=sys.stderr)
        return 2
    connection = sqlite3.connect(f"{path.resolve().as_uri()}?mode=ro", uri=True)
    try:
        report = summarize(load_takes(connection, args.since))
    except ValueError as error:
        print(error, file=sys.stderr)
        return 2
    finally:
        connection.close()
    print(json.dumps(report, indent=2) if args.json else render(report))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
