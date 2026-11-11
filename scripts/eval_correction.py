#!/usr/bin/env python3
"""Offline evaluation of AI text correction (rewrite) quality.

The input is fixed transcript text, never audio, so recognition errors are kept
apart from rewrite errors. Each case in the dataset
(``docs/eval_set/correction/correction-offline-v1.json``) lists the facts the
rewrite must keep (``rules.require``), the alterations it must not make
(``rules.forbid``), and hand-written fixture outputs with their expected
verdicts.

Every automatic metric here is deterministic string matching:

* ``dropped:<kind>``   -- a required fact (number, name, negation, ...) is missing.
* ``forbidden:<kind>`` -- a forbidden alteration (superseded value, flipped
  polarity, filler, executed instruction, ...) is present.
* ``added:<type>``     -- the output contains a number, URL, code span or
  Latin-script name that the transcript did not contain.
* ``edit_ratio``       -- the character edit distance from the transcript,
  divided by the transcript length, exceeds the case limit.

Readability and tone are *not* scored automatically. Each scored output carries
a ``judge`` object whose fields stay ``null`` unless a human or an LLM judge
supplies them in the outputs file, and the report says which.

Providers for ``run``:

* ``identity``  -- the raw transcript (what the app inserts on fallback).
* ``reference`` -- each case's hand-written acceptable rewrite.
* ``fixtures``  -- every fixture output; the run fails when a verdict differs
  from the fixture's ``expect_failures`` (scorer self-test).
* ``app``       -- the application's own correction path (prompt, provider and
  meaning guard) through ``cargo run --example correction_eval``; the provider
  comes from ``--settings``. This sends the synthetic transcripts to whichever
  provider those settings select.

Only the Python standard library is used.
"""
from __future__ import annotations

import argparse
import csv
import json
import os
import random
import re
import subprocess
import sys
import tempfile
import unicodedata
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable

SCHEMA = "correction-eval/1"
ROOT = Path(__file__).resolve().parents[1]
DEFAULT_DATASET = ROOT / "docs" / "eval_set" / "correction" / "correction-offline-v1.json"

LANGUAGES = {"ja", "en", "mixed"}
MODES = {"conservative", "intent_aware"}
CATEGORIES = {
    "filler",
    "self_correction",
    "slip",
    "technical_terms",
    "negation",
    "numbers_units",
    "url_code",
    "long_memo",
    "ambiguous_instruction",
    "proposition",
}
RULE_KINDS = {
    "number",
    "name",
    "term",
    "url",
    "code",
    "negation",
    "proposition",
    "uncertainty",
    "question",
    "self_correction",
    "filler",
}
ADDED_TYPES = {"number", "url", "code", "name"}
GUARD_EXPECTATIONS = {"accept", "reject", "not_detectable"}
DEFAULT_MAX_EDIT_RATIO = {"conservative": 0.5, "intent_aware": 0.8}
FACT_KINDS = {"number", "name", "term", "url", "code"}
PROPOSITION_KINDS = {"proposition", "uncertainty", "question"}
_ID = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")


class DatasetError(ValueError):
    """The dataset does not follow the ``correction-eval/1`` schema."""


# --------------------------------------------------------------------------
# Dataset schema
# --------------------------------------------------------------------------


def load_dataset(path: Path) -> dict[str, Any]:
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    validate_dataset(data)
    return data


def validate_dataset(data: Any) -> None:
    """Raises :class:`DatasetError` listing every schema violation."""
    problems: list[str] = []

    def need(condition: bool, message: str) -> None:
        if not condition:
            problems.append(message)

    if not isinstance(data, dict):
        raise DatasetError("dataset must be a JSON object")
    need(data.get("schema") == SCHEMA, f"schema must be {SCHEMA!r}")
    for key in ("name", "provenance", "license"):
        need(isinstance(data.get(key), str) and data[key].strip() != "", f"{key} must be a non-empty string")
    cases = data.get("cases")
    if not isinstance(cases, list) or not cases:
        raise DatasetError("; ".join(problems + ["cases must be a non-empty list"]))
    seen: set[str] = set()
    for index, case in enumerate(cases):
        where = f"cases[{index}]"
        if not isinstance(case, dict):
            problems.append(f"{where} must be an object")
            continue
        case_id = case.get("id")
        if not isinstance(case_id, str) or not _ID.match(case_id):
            problems.append(f"{where}.id must be lowercase words joined by '-'")
        elif case_id in seen:
            problems.append(f"{where}.id {case_id!r} is duplicated")
        else:
            seen.add(case_id)
            where = case_id
        need(case.get("language") in LANGUAGES, f"{where}.language must be one of {sorted(LANGUAGES)}")
        need(case.get("correction_mode") in MODES, f"{where}.correction_mode must be one of {sorted(MODES)}")
        categories = case.get("categories")
        need(
            isinstance(categories, list) and bool(categories) and all(item in CATEGORIES for item in categories),
            f"{where}.categories must be a non-empty list of {sorted(CATEGORIES)}",
        )
        for key in ("transcript", "reference"):
            need(isinstance(case.get(key), str) and case[key].strip() != "", f"{where}.{key} must be a non-empty string")
        for key in ("allowed_changes", "forbidden_changes"):
            value = case.get(key)
            need(
                isinstance(value, list) and bool(value) and all(isinstance(item, str) and item.strip() for item in value),
                f"{where}.{key} must be a non-empty list of strings",
            )
        dictionary = case.get("dictionary", [])
        need(
            isinstance(dictionary, list)
            and all(
                isinstance(term, dict)
                and isinstance(term.get("surface"), str)
                and term["surface"].strip()
                and isinstance(term.get("readings"), list)
                and all(isinstance(reading, str) and reading.strip() for reading in term["readings"])
                for term in dictionary
            ),
            f"{where}.dictionary must be a list of {{surface, readings}}",
        )
        settings = case.get("settings", {})
        need(
            isinstance(settings, dict) and all(isinstance(value, bool) for value in settings.values()),
            f"{where}.settings must map correction switches to booleans",
        )
        rules = case.get("rules")
        if not isinstance(rules, dict):
            problems.append(f"{where}.rules must be an object")
            continue
        unknown = set(rules) - {"require", "forbid", "allow_new", "max_edit_ratio"}
        need(not unknown, f"{where}.rules has unknown keys {sorted(unknown)}")
        for key in ("require", "forbid"):
            groups = rules.get(key, [])
            if not isinstance(groups, list):
                problems.append(f"{where}.rules.{key} must be a list")
                continue
            for group_index, group in enumerate(groups):
                label = f"{where}.rules.{key}[{group_index}]"
                need(isinstance(group, dict) and group.get("kind") in RULE_KINDS, f"{label}.kind must be one of {sorted(RULE_KINDS)}")
                alternatives = group.get("any") if isinstance(group, dict) else None
                need(
                    isinstance(alternatives, list) and bool(alternatives) and all(isinstance(item, str) and item.strip() for item in alternatives),
                    f"{label}.any must be a non-empty list of strings",
                )
        need(bool(rules.get("require")) or bool(rules.get("forbid")), f"{where}.rules needs at least one require or forbid group")
        allow_new = rules.get("allow_new", [])
        need(isinstance(allow_new, list) and all(isinstance(item, str) for item in allow_new), f"{where}.rules.allow_new must be a list of strings")
        if "max_edit_ratio" in rules:
            ratio = rules["max_edit_ratio"]
            need(isinstance(ratio, (int, float)) and not isinstance(ratio, bool) and ratio > 0, f"{where}.rules.max_edit_ratio must be a positive number")
        fixtures = case.get("fixtures")
        if not isinstance(fixtures, list) or not fixtures:
            problems.append(f"{where}.fixtures must be a non-empty list")
            continue
        fixture_ids: set[str] = set()
        for fixture_index, fixture in enumerate(fixtures):
            label = f"{where}.fixtures[{fixture_index}]"
            if not isinstance(fixture, dict):
                problems.append(f"{label} must be an object")
                continue
            fixture_id = fixture.get("id")
            need(isinstance(fixture_id, str) and bool(_ID.match(fixture_id)) and fixture_id not in fixture_ids and fixture_id != "reference", f"{label}.id must be a unique lowercase id other than 'reference'")
            if isinstance(fixture_id, str):
                fixture_ids.add(fixture_id)
            need(isinstance(fixture.get("output"), str), f"{label}.output must be a string")
            need(fixture.get("guard") in GUARD_EXPECTATIONS, f"{label}.guard must be one of {sorted(GUARD_EXPECTATIONS)}")
            expected = fixture.get("expect_failures")
            need(
                isinstance(expected, list) and all(isinstance(item, str) and _known_failure(item) for item in expected),
                f"{label}.expect_failures must list known failure codes",
            )
    if problems:
        raise DatasetError("; ".join(problems))


def _known_failure(code: str) -> bool:
    if code == "edit_ratio":
        return True
    prefix, _, kind = code.partition(":")
    if prefix in {"dropped", "forbidden"}:
        return kind in RULE_KINDS
    return prefix == "added" and kind in ADDED_TYPES


# --------------------------------------------------------------------------
# Matching helpers
# --------------------------------------------------------------------------


def normalize(text: str) -> str:
    """NFKC, case-folded, with whitespace runs collapsed to one space."""
    return re.sub(r"\s+", " ", unicodedata.normalize("NFKC", text).casefold()).strip()


def _spaceless(text: str) -> str:
    return re.sub(r"\s+", "", text)


def contains(text: str, pattern: str) -> bool:
    """Whether ``pattern`` occurs in ``text`` after normalization.

    A pattern without leading or trailing whitespace also matches across
    spacing differences (``React の`` and ``Reactの``); one with explicit edge
    whitespace (``"um "``) only matches as written, so it acts as a word edge.
    """
    haystack = normalize(text)
    raw = unicodedata.normalize("NFKC", pattern).casefold()
    if raw != raw.strip():
        padded = f" {haystack} "
        return re.sub(r"\s+", " ", raw) in padded
    needle = normalize(pattern)
    return needle in haystack or _spaceless(needle) in _spaceless(haystack)


_URL = re.compile(r"https?://[^\s<>()\[\]{}\"'、。「」]+", re.IGNORECASE)
_CODE = re.compile(r"`[^`\n]+`")
_NUMBER = re.compile(r"\d+(?:[.,:]\d+)*")
_LATIN = re.compile(r"[A-Za-z][A-Za-z0-9+#._'-]*")
_SENTENCE_END = set(".!?。！？:：\n")


def _strip_url(url: str) -> str:
    return url.rstrip(".,;:!?")


def urls(text: str) -> list[str]:
    return [_strip_url(match.group(0)) for match in _URL.finditer(unicodedata.normalize("NFKC", text))]


def code_spans(text: str) -> list[str]:
    return [match.group(0) for match in _CODE.finditer(unicodedata.normalize("NFKC", text))]


def _without(text: str, spans: Iterable[str]) -> str:
    for span in spans:
        text = text.replace(span, " ")
    return text


def numbers(text: str) -> list[str]:
    """Digit values outside URLs and code spans, with thousands separators removed."""
    text = unicodedata.normalize("NFKC", text)
    text = _without(text, sorted(urls(text) + code_spans(text), key=len, reverse=True))
    values = []
    for match in _NUMBER.finditer(text):
        value = match.group(0).rstrip(".,:")
        values.append(re.sub(r"(?<=\d),(?=\d{3}\b)", "", value))
    return values


def latin_names(text: str) -> list[str]:
    """Latin-script tokens that look like names, terms or identifiers.

    A token counts when it has a digit or an inner capital (``GitHub``,
    ``GPT4``), or starts with a capital where it is not just the first word of
    an English sentence. A capitalized token directly followed by non-Latin
    text (``Dockerの``) is a name even at the start of the text.
    """
    text = unicodedata.normalize("NFKC", text)
    text = _without(text, sorted(urls(text) + code_spans(text), key=len, reverse=True))
    names = []
    for match in _LATIN.finditer(text):
        token = match.group(0).rstrip("._'-")
        if len(token) < 2:
            continue
        inner = any(character.isupper() for character in token[1:]) or any(character.isdigit() for character in token)
        if not inner and not token[0].isupper():
            continue
        if not inner:
            before = text[: match.start()].rstrip(" \t-*•")
            after = text[match.start() + len(token) : match.start() + len(token) + 1]
            sentence_initial = not before or before[-1] in _SENTENCE_END or re.search(r"(?:^|\n)\s*\d+[.)]$", before) is not None
            english_context = after == "" or after.isspace() or after in ",.;:!?)'\""
            if sentence_initial and english_context:
                continue
        names.append(token)
    return names


def levenshtein(source: str, target: str) -> int:
    if len(source) < len(target):
        source, target = target, source
    previous = list(range(len(target) + 1))
    for row, source_character in enumerate(source, 1):
        current = [row]
        for column, target_character in enumerate(target, 1):
            current.append(
                min(
                    previous[column] + 1,
                    current[column - 1] + 1,
                    previous[column - 1] + (source_character != target_character),
                )
            )
        previous = current
    return previous[-1]


def _content(text: str) -> str:
    """Normalized text without whitespace, punctuation and list symbols."""
    return "".join(
        character
        for character in normalize(text)
        if not character.isspace() and not unicodedata.category(character).startswith(("P", "S"))
    )


def edit_ratio(source: str, output: str) -> float:
    base = _content(source)
    return round(levenshtein(base, _content(output)) / max(1, len(base)), 4)


# --------------------------------------------------------------------------
# Scoring
# --------------------------------------------------------------------------


def added_facts(case: dict[str, Any], output: str) -> list[str]:
    """``added:<type>`` codes for values in the output the transcript lacks."""
    transcript = case["transcript"]
    rules = case["rules"]
    allowed = {normalize(item) for item in rules.get("allow_new", [])}
    allowed |= {normalize(term["surface"]) for term in case.get("dictionary", [])}
    failures = []
    source_numbers = set(numbers(transcript))
    if any(value not in source_numbers and normalize(value) not in allowed for value in numbers(output)):
        failures.append("added:number")
    source_urls = {normalize(url) for url in urls(transcript)}
    if any(normalize(url) not in source_urls and normalize(url) not in allowed for url in urls(output)):
        failures.append("added:url")
    source_code = {normalize(span) for span in code_spans(transcript)}
    if any(normalize(span) not in source_code and normalize(span) not in allowed for span in code_spans(output)):
        failures.append("added:code")
    source_text = _spaceless(normalize(transcript))
    if any(
        _spaceless(normalize(name)) not in source_text and normalize(name) not in allowed
        for name in latin_names(output)
    ):
        failures.append("added:name")
    return failures


def score_output(case: dict[str, Any], output: str) -> dict[str, Any]:
    """Deterministic verdict for one output of one case."""
    rules = case["rules"]
    failures: set[str] = set()
    for group in rules.get("require", []):
        if not any(contains(output, alternative) for alternative in group["any"]):
            failures.add(f"dropped:{group['kind']}")
    for group in rules.get("forbid", []):
        if any(contains(output, alternative) for alternative in group["any"]):
            failures.add(f"forbidden:{group['kind']}")
    failures.update(added_facts(case, output))
    ratio = edit_ratio(case["transcript"], output)
    limit = rules.get("max_edit_ratio", DEFAULT_MAX_EDIT_RATIO[case["correction_mode"]])
    if ratio > limit:
        failures.add("edit_ratio")
    kinds = {group["kind"] for key in ("require", "forbid") for group in rules.get(key, [])}

    def clean(selected: set[str], *, added: bool = False) -> bool:
        return not any(
            code.partition(":")[2] in selected and (added or not code.startswith("added:"))
            for code in failures
        )

    def optional(kind: str) -> bool | None:
        return clean({kind}) if kind in kinds else None

    return {
        "pass": not failures,
        "failures": sorted(failures),
        "metrics": {
            "information_dropped": any(code.startswith("dropped:") for code in failures),
            "information_added": any(code.startswith("added:") for code in failures),
            "names_numbers_match": clean(FACT_KINDS, added=True),
            "negation_preserved": optional("negation"),
            "proposition_preserved": clean(PROPOSITION_KINDS),
            "self_correction_resolved": optional("self_correction"),
            "fillers_removed": optional("filler"),
            "edit_ratio": ratio,
            "edit_ratio_limit": limit,
            "reference_edit_ratio": edit_ratio(case["reference"], output),
        },
    }


def _judge(value: Any) -> dict[str, Any]:
    """Readability/tone are never computed here; only a supplied judgment is kept."""
    judge = {"readability": None, "tone": None, "source": None}
    if isinstance(value, dict):
        for key in ("readability", "tone"):
            if isinstance(value.get(key), (int, float)) and not isinstance(value.get(key), bool):
                judge[key] = value[key]
        if value.get("source") in {"human", "llm"}:
            judge["source"] = value["source"]
    judge["status"] = "supplied" if judge["source"] else "not_scored"
    return judge


def summarize(rows: list[dict[str, Any]]) -> dict[str, Any]:
    """Aggregate pass and metric rates; ``None`` metrics are not applicable."""
    def rates(selected: list[dict[str, Any]]) -> dict[str, Any]:
        summary: dict[str, Any] = {"outputs": len(selected), "passed": sum(row["score"]["pass"] for row in selected)}
        for metric in (
            "information_dropped",
            "information_added",
            "names_numbers_match",
            "negation_preserved",
            "proposition_preserved",
            "self_correction_resolved",
            "fillers_removed",
        ):
            values = [row["score"]["metrics"][metric] for row in selected if row["score"]["metrics"][metric] is not None]
            summary[metric] = {"true": sum(values), "applicable": len(values)}
        ratios = [row["score"]["metrics"]["edit_ratio"] for row in selected]
        summary["mean_edit_ratio"] = round(sum(ratios) / len(ratios), 4) if ratios else None
        return summary

    by_language: dict[str, list[dict[str, Any]]] = defaultdict(list)
    by_category: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        by_language[row["language"]].append(row)
        for category in row["categories"]:
            by_category[category].append(row)
    return {
        "overall": rates(rows),
        "by_language": {key: rates(value) for key, value in sorted(by_language.items())},
        "by_category": {key: rates(value) for key, value in sorted(by_category.items())},
    }


def _row(case: dict[str, Any], output_id: str, output: str, extra: dict[str, Any] | None = None) -> dict[str, Any]:
    row = {
        "case": case["id"],
        "output_id": output_id,
        "language": case["language"],
        "categories": case["categories"],
        "correction_mode": case["correction_mode"],
        "output": output,
        "score": score_output(case, output),
        "judge": _judge((extra or {}).get("judge")),
    }
    if extra:
        row.update({key: value for key, value in extra.items() if key != "judge"})
    return row


def check_fixtures(dataset: dict[str, Any]) -> tuple[list[dict[str, Any]], list[str]]:
    """Scores the reference and every fixture; returns rows and expectation mismatches."""
    rows, mismatches = [], []
    for case in dataset["cases"]:
        reference = _row(case, "reference", case["reference"])
        rows.append(reference)
        if reference["score"]["failures"]:
            mismatches.append(f"{case['id']}/reference: expected pass, got {reference['score']['failures']}")
        for fixture in case["fixtures"]:
            row = _row(case, fixture["id"], fixture["output"], {"guard_expectation": fixture["guard"]})
            rows.append(row)
            expected = sorted(set(fixture["expect_failures"]))
            if row["score"]["failures"] != expected:
                mismatches.append(f"{case['id']}/{fixture['id']}: expected {expected}, got {row['score']['failures']}")
    return rows, mismatches


def score_outputs(dataset: dict[str, Any], outputs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Scores app/provider outputs (``{"id", "output", ...}`` per case)."""
    cases = {case["id"]: case for case in dataset["cases"]}
    rows = []
    for item in outputs:
        case = cases.get(item.get("id"))
        if case is None:
            raise DatasetError(f"output for unknown case {item.get('id')!r}")
        if not isinstance(item.get("output"), str):
            raise DatasetError(f"output for {case['id']} must be a string")
        extra = {key: item[key] for key in ("accepted", "error", "judge") if key in item}
        row = _row(case, "inserted", item["output"], extra)
        candidate = item.get("candidate")
        if isinstance(candidate, str):
            row["candidate_score"] = score_output(case, candidate)
        rows.append(row)
    return rows


def guard_summary(rows: list[dict[str, Any]]) -> dict[str, Any] | None:
    """How the app's meaning guard treated provider candidates.

    ``caught`` counts rejected candidates that also fail the deterministic
    rules; ``false_rejections`` counts rejected candidates the rules accept.
    """
    judged = [row for row in rows if "candidate_score" in row and "accepted" in row]
    if not judged:
        return None
    rejected = [row for row in judged if not row["accepted"]]
    return {
        "candidates": len(judged),
        "rejected": len(rejected),
        "caught": sum(not row["candidate_score"]["pass"] for row in rejected),
        "false_rejections": sum(row["candidate_score"]["pass"] for row in rejected),
        "accepted_failing": sum(row["accepted"] and not row["candidate_score"]["pass"] for row in judged),
    }


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    rows = []
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        if line.strip():
            rows.append(json.loads(line))
    return rows


def run_app(dataset_path: Path, settings_path: Path | None, cargo: str = "cargo") -> list[dict[str, Any]]:
    """Runs the app's correction path over every case and returns its JSONL rows."""
    with tempfile.TemporaryDirectory() as directory:
        out = Path(directory) / "outputs.jsonl"
        command = [
            cargo, "run", "--quiet", "--manifest-path", str(ROOT / "src-tauri" / "Cargo.toml"),
            "--example", "correction_eval", "--",
            "--dataset", str(Path(dataset_path).resolve()), "--out", str(out),
        ]
        if settings_path is not None:
            command += ["--settings", str(Path(settings_path).resolve())]
        subprocess.run(command, check=True)
        return read_jsonl(out)


def build_report(dataset: dict[str, Any], provider: str, rows: list[dict[str, Any]], mismatches: list[str] | None = None) -> dict[str, Any]:
    report = {
        "schema": "correction-eval-report/1",
        "dataset": dataset["name"],
        "provider": provider,
        "automatic_metrics": "deterministic string rules; readability and tone are not scored",
        "summary": summarize(rows),
        "rows": rows,
    }
    guard = guard_summary(rows)
    if guard is not None:
        report["guard"] = guard
    if mismatches is not None:
        report["fixture_mismatches"] = mismatches
    return report


def blind_sheet(dataset: dict[str, Any], systems: dict[str, list[dict[str, Any]]], seed: int) -> tuple[list[dict[str, str]], dict[str, dict[str, str]]]:
    """Rows for a blind human comparison and the key that maps labels back.

    Each case gets its systems' outputs in a random order under the labels
    A, B, C...; the key is written separately so raters never see it.
    """
    rng = random.Random(seed)
    by_system = {name: {item["id"]: item["output"] for item in outputs} for name, outputs in systems.items()}
    sheet, key = [], {}
    for case in dataset["cases"]:
        names = [name for name in systems if case["id"] in by_system[name]]
        rng.shuffle(names)
        labels = {}
        for index, name in enumerate(names):
            label = chr(ord("A") + index)
            labels[label] = name
            sheet.append({
                "case": case["id"], "label": label, "transcript": case["transcript"],
                "output": by_system[name][case["id"]], "meaning_preserved_1_5": "",
                "readability_1_5": "", "tone_1_5": "", "notes": "",
            })
        key[case["id"]] = labels
    return sheet, key


def _print_summary(report: dict[str, Any]) -> None:
    overall = report["summary"]["overall"]
    print(f"{report['provider']}: {overall['passed']}/{overall['outputs']} outputs pass the deterministic rules")
    for metric in ("information_dropped", "information_added", "names_numbers_match", "negation_preserved", "proposition_preserved", "self_correction_resolved"):
        value = overall[metric]
        print(f"  {metric}: {value['true']}/{value['applicable']}")
    print(f"  mean_edit_ratio: {overall['mean_edit_ratio']}")
    if "guard" in report:
        print(f"  guard: {report['guard']}")
    for mismatch in report.get("fixture_mismatches", []):
        print(f"  MISMATCH {mismatch}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--dataset", type=Path, default=DEFAULT_DATASET)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("validate", help="check the dataset schema")
    run = sub.add_parser("run", help="produce outputs with a provider and score them")
    run.add_argument("--provider", choices=["identity", "reference", "fixtures", "app"], default="fixtures")
    run.add_argument("--settings", type=Path, help="app settings patch (JSON) for --provider app")
    run.add_argument("--out", type=Path, help="write the JSON report here")
    score = sub.add_parser("score", help="score an outputs JSONL file ({id, output[, candidate, accepted, judge]})")
    score.add_argument("--outputs", type=Path, required=True)
    score.add_argument("--name", default="outputs")
    score.add_argument("--out", type=Path)
    blind = sub.add_parser("blind", help="write a blind comparison sheet (CSV) and a separate key (JSON)")
    blind.add_argument("--system", action="append", required=True, metavar="NAME=OUTPUTS.jsonl")
    blind.add_argument("--sheet", type=Path, required=True)
    blind.add_argument("--key", type=Path, required=True)
    blind.add_argument("--seed", type=int, default=85)
    args = parser.parse_args(argv)

    try:
        dataset = load_dataset(args.dataset)
    except DatasetError as error:
        print(f"invalid dataset: {error}", file=sys.stderr)
        return 2
    if args.command == "validate":
        print(f"{dataset['name']}: {len(dataset['cases'])} cases, {sum(len(case['fixtures']) for case in dataset['cases'])} fixtures")
        return 0
    if args.command == "blind":
        systems = {}
        for item in args.system:
            name, _, path = item.partition("=")
            if not name or not path:
                parser.error("--system takes NAME=OUTPUTS.jsonl")
            systems[name] = read_jsonl(Path(path))
        sheet, key = blind_sheet(dataset, systems, args.seed)
        with Path(args.sheet).open("w", encoding="utf-8-sig", newline="") as handle:
            writer = csv.DictWriter(handle, fieldnames=list(sheet[0]) if sheet else ["case"])
            writer.writeheader()
            writer.writerows(sheet)
        Path(args.key).write_text(json.dumps(key, ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"wrote {len(sheet)} rows to {args.sheet}; keep {args.key} away from raters")
        return 0

    mismatches = None
    if args.command == "score":
        rows = score_outputs(dataset, read_jsonl(args.outputs))
        provider = args.name
    elif args.provider == "fixtures":
        rows, mismatches = check_fixtures(dataset)
        provider = "fixtures"
    elif args.provider == "app":
        rows = score_outputs(dataset, run_app(args.dataset, args.settings, os.environ.get("CARGO", "cargo")))
        provider = "app"
    else:
        field = "transcript" if args.provider == "identity" else "reference"
        rows = [_row(case, args.provider, case[field]) for case in dataset["cases"]]
        provider = args.provider
    report = build_report(dataset, provider, rows, mismatches)
    if args.out:
        Path(args.out).write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    _print_summary(report)
    return 1 if mismatches else 0


if __name__ == "__main__":
    sys.exit(main())
