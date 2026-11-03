#!/usr/bin/env python3
"""ASR accuracy scoring for the bench_asr.py JSON output (and ad-hoc pairs).

This script computes the accuracy KPIs described in ``docs/benchmarks.md`` §3.4
that the timing harness (``scripts/bench_asr.py``) intentionally leaves out:

* **CER** -- Character Error Rate. Character-level Levenshtein edit distance
  divided by the number of reference characters. Primary metric for Japanese.
* **WER** -- Word Error Rate. Word-level (whitespace-tokenised) Levenshtein
  edit distance divided by the number of reference words. Primary metric for
  English.
* **Term recall ("固有名詞正解率")** -- given a list of terms (proper nouns /
  dictionary entries), the fraction of reference-present terms that also appear
  in the hypothesis.

Only the Python standard library is used (no jiwer / Levenshtein / numpy), so
this runs in the offline WSL2 dev box and on the Windows measurement machine
without extra dependencies.

Two input modes
---------------
(a) Bench mode -- score a full ``bench_asr.py --json`` file against a reference
    transcript map (audio filename -> ground-truth sentence)::

        python scripts/score_asr.py --bench results/vibevoice_4bit_3s.json \\
            --ref refs.json --terms terms.txt

    The reference map may be JSON (``{"ja_3s.wav": "正解文", ...}``) or TSV
    (``<audio filename><TAB><reference text>`` per line). Audio is matched by
    basename, so ``bench_audio/ja_3s.wav`` matches the key ``ja_3s.wav``.

(b) Pair mode -- compare two strings directly::

        python scripts/score_asr.py --hyp "the cat" --ref "the bat"

Normalisation
-------------
Normalisation defaults are tuned for the common case and can be toggled:

* CER (Japanese-leaning): NFKC, strip-whitespace, strip-punctuation -- all ON
  by default. ``--keep-*`` flags turn each off.
* WER (English-leaning): lowercase, strip-punctuation -- ON by default;
  whitespace is the tokeniser so it is always collapsed for WER.

Output
------
Human-readable markdown by default; ``--json`` for machine consumption. Bench
mode reports per-file rows plus an overall aggregate (micro-averaged over the
pooled edit distances / lengths).

Self test
---------
``python scripts/score_asr.py --self-test`` runs built-in known cases (exact
match -> 0, known edit-distance pairs, a Japanese pair, term recall edge cases)
and exits non-zero on any mismatch.
"""

from __future__ import annotations

import argparse
import contextlib
import csv
import hashlib
import json
import math
import os
import sys
import unicodedata
from pathlib import Path
from typing import Any, Iterable

# Punctuation we strip when --strip-punctuation is active. Covers ASCII and the
# common Japanese full-width marks; not exhaustive but matches docs §3.4 intent
# ("空白除去 ... 句読点の扱いを明文化して固定"). The long vowel mark ー (U+30FC)
# is deliberately absent: it is part of a word, so ビール and ビル must differ.
_PUNCT_CHARS = set(
    "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~"
    "、。，．・「」『』（）〔〕［］｛｝〈〉《》【】〜―‐-…！？："
    "；＂＇｀＾～＿｜＠＃＄％＆＊＋／＜＝＞｟｠｢｣､｡･"
    "“”‘’«»"
)


# --------------------------------------------------------------------------- #
# Normalisation
# --------------------------------------------------------------------------- #
def normalize_text(
    text: str,
    *,
    nfkc: bool = False,
    lower: bool = False,
    strip_whitespace: bool = False,
    strip_punctuation: bool = False,
) -> str:
    """Apply the requested normalisations in a fixed, documented order.

    Order: NFKC -> lowercase -> strip punctuation -> whitespace handling.
    ``strip_whitespace`` removes *all* whitespace (used for CER); when it is
    False, runs of whitespace are collapsed to a single space and the string is
    trimmed (used for WER tokenisation).
    """
    if nfkc:
        text = unicodedata.normalize("NFKC", text)
    if lower:
        text = text.lower()
    if strip_punctuation:
        text = "".join(ch for ch in text if ch not in _PUNCT_CHARS)
    if strip_whitespace:
        text = "".join(text.split())
    else:
        text = " ".join(text.split())
    return text


# --------------------------------------------------------------------------- #
# Levenshtein edit distance (DP, stdlib only)
# --------------------------------------------------------------------------- #
def levenshtein(a: list[Any] | str, b: list[Any] | str) -> int:
    """Levenshtein distance between two sequences (sub/ins/del cost = 1).

    Implemented with the standard two-row dynamic-programming table so the
    memory footprint stays O(min(len)) and there are no external deps.
    """
    if a == b:
        return 0
    n, m = len(a), len(b)
    if n == 0:
        return m
    if m == 0:
        return n
    # Keep the inner (column) loop over the shorter sequence.
    if n < m:
        a, b = b, a
        n, m = m, n
    previous = list(range(m + 1))
    for i in range(1, n + 1):
        current = [i] + [0] * m
        ai = a[i - 1]
        for j in range(1, m + 1):
            cost = 0 if ai == b[j - 1] else 1
            current[j] = min(
                previous[j] + 1,      # deletion
                current[j - 1] + 1,   # insertion
                previous[j - 1] + cost,  # substitution / match
            )
        previous = current
    return previous[m]


# --------------------------------------------------------------------------- #
# CER / WER
# --------------------------------------------------------------------------- #
def cer_stats(
    reference: str,
    hypothesis: str,
    *,
    nfkc: bool = True,
    strip_whitespace: bool = True,
    strip_punctuation: bool = True,
) -> dict[str, Any]:
    """Character-level edit distance / reference length.

    Empty-reference handling (docs §3.4 edge case): if the normalised reference
    is empty, the rate is 0.0 when the hypothesis is also empty, else 1.0 (every
    hypothesis character is an insertion error; we report it as a full miss so
    the row is flagged rather than dividing by zero).
    """
    ref = normalize_text(
        reference, nfkc=nfkc, strip_whitespace=strip_whitespace, strip_punctuation=strip_punctuation
    )
    hyp = normalize_text(
        hypothesis, nfkc=nfkc, strip_whitespace=strip_whitespace, strip_punctuation=strip_punctuation
    )
    dist = levenshtein(ref, hyp)
    ref_len = len(ref)
    rate = _rate(dist, ref_len, hyp_len=len(hyp))
    return {"errors": dist, "ref_len": ref_len, "cer": rate}


def wer_stats(
    reference: str,
    hypothesis: str,
    *,
    nfkc: bool = False,
    lower: bool = True,
    strip_punctuation: bool = True,
) -> dict[str, Any]:
    """Word-level edit distance / reference word count.

    Tokenisation is whitespace-based after normalisation. Empty-reference
    handling mirrors :func:`cer_stats`.
    """
    ref = normalize_text(
        reference, nfkc=nfkc, lower=lower, strip_whitespace=False, strip_punctuation=strip_punctuation
    )
    hyp = normalize_text(
        hypothesis, nfkc=nfkc, lower=lower, strip_whitespace=False, strip_punctuation=strip_punctuation
    )
    ref_words = ref.split()
    hyp_words = hyp.split()
    dist = levenshtein(ref_words, hyp_words)
    ref_len = len(ref_words)
    rate = _rate(dist, ref_len, hyp_len=len(hyp_words))
    return {"errors": dist, "ref_len": ref_len, "wer": rate}


def _rate(errors: int, ref_len: int, *, hyp_len: int) -> float:
    """Errors / ref_len with the documented empty-reference convention."""
    if ref_len == 0:
        return 0.0 if hyp_len == 0 else 1.0
    return errors / ref_len


# --------------------------------------------------------------------------- #
# Term recall (固有名詞正解率)
# --------------------------------------------------------------------------- #
def term_recall_stats(
    reference: str,
    hypothesis: str,
    terms: list[str],
    *,
    nfkc: bool = True,
    lower: bool = False,
    strip_punctuation: bool = True,
) -> dict[str, Any]:
    """Fraction of reference-present terms that also occur in the hypothesis.

    Only terms that appear in the (normalised) reference count toward the
    denominator -- a term the speaker never said cannot be scored. If no term
    appears in the reference, recall is reported as None (n/a) so it is excluded
    from aggregation rather than counted as 1.0 or 0.0.
    """
    norm = lambda s: normalize_text(  # noqa: E731 - small local helper
        s, nfkc=nfkc, lower=lower, strip_whitespace=False, strip_punctuation=strip_punctuation
    )
    ref = norm(reference)
    hyp = norm(hypothesis)
    present: list[str] = []
    hit: list[str] = []
    for term in terms:
        t = norm(term)
        if not t:
            continue
        if t in ref:
            present.append(term)
            if t in hyp:
                hit.append(term)
    denom = len(present)
    recall = (len(hit) / denom) if denom else None
    return {
        "terms_in_ref": denom,
        "terms_hit": len(hit),
        "recall": recall,
        "present": present,
        "hit": hit,
    }


# --------------------------------------------------------------------------- #
# Per-pair scoring (combines all metrics with a single normalisation policy)
# --------------------------------------------------------------------------- #
def score_pair(reference: str, hypothesis: str, opts: "Options") -> dict[str, Any]:
    cer = cer_stats(
        reference,
        hypothesis,
        nfkc=opts.cer_nfkc,
        strip_whitespace=opts.cer_strip_whitespace,
        strip_punctuation=opts.cer_strip_punctuation,
    )
    wer = wer_stats(
        reference,
        hypothesis,
        nfkc=opts.wer_nfkc,
        lower=opts.wer_lower,
        strip_punctuation=opts.wer_strip_punctuation,
    )
    out: dict[str, Any] = {"cer": cer, "wer": wer}
    if opts.terms:
        out["terms"] = term_recall_stats(
            reference,
            hypothesis,
            opts.terms,
            nfkc=opts.cer_nfkc,
            lower=opts.wer_lower,
            strip_punctuation=opts.cer_strip_punctuation,
        )
    return out


# --------------------------------------------------------------------------- #
# Options bundle (normalisation policy resolved from argparse)
# --------------------------------------------------------------------------- #
class Options:
    def __init__(self, args: argparse.Namespace) -> None:
        self.cer_nfkc = not args.cer_keep_width
        self.cer_strip_whitespace = not args.cer_keep_whitespace
        self.cer_strip_punctuation = not args.cer_keep_punctuation
        self.wer_nfkc = args.wer_nfkc
        self.wer_lower = not args.wer_keep_case
        self.wer_strip_punctuation = not args.wer_keep_punctuation
        self.terms: list[str] = args._terms


# --------------------------------------------------------------------------- #
# Reference map loading
# --------------------------------------------------------------------------- #
MANIFEST_SCHEMA = "asr-bench-manifest/1"
JA_WER_REASON = "ja: no word segmentation; CER is primary"


def load_manifest(path: Path) -> dict[str, Any]:
    """Validate consumer fields and resolve audio paths from the manifest directory."""
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict) or data.get("schema") != MANIFEST_SCHEMA:
        raise ValueError(f"{path}: expected schema {MANIFEST_SCHEMA!r}")
    if not isinstance(data.get("name"), str) or not data["name"]:
        raise ValueError(f"{path}: manifest name must be a nonempty string")
    if not isinstance(data.get("sources"), list) or not isinstance(data.get("items"), list):
        raise ValueError(f"{path}: sources and items must be lists")
    if not data["items"]:
        raise ValueError(f"{path}: manifest must contain at least one item")
    seen: set[str] = set()
    for item in data["items"]:
        if not isinstance(item, dict):
            raise ValueError(f"{path}: each item must be an object")
        for field in ("id", "audio_path", "reference", "language", "source"):
            if not isinstance(item.get(field), str) or (field != "reference" and not item[field]):
                raise ValueError(f"{path}: item {field} must be a string (nonempty except reference)")
        if item["id"] in seen:
            raise ValueError(f"{path}: duplicate id {item['id']!r}")
        seen.add(item["id"])
        audio = Path(item["audio_path"])
        if audio.name != item["id"] + ".wav":
            raise ValueError(f"{path}: audio basename must be <id>.wav for {item['id']!r}")
        item["audio_path"] = str((path.parent / audio).resolve())
    return data


def manifest_conditions(manifest: dict[str, Any], path: Path) -> dict[str, Any]:
    return {
        "name": manifest["name"], "schema": manifest["schema"], "path": str(path.resolve()),
        "sources": manifest["sources"], "item_count": len(manifest["items"]),
    }


def load_reference_map(path: Path) -> dict[str, str]:
    """Load audio-basename -> reference text from JSON or TSV.

    JSON form: ``{"ja_3s.wav": "正解文", ...}``.
    TSV form : one ``<key>\\t<text>`` per line (blank lines / '#'-comments ok).
    Keys are normalised to their basename so bench ``audio`` paths match.
    """
    raw = path.read_text(encoding="utf-8")
    mapping: dict[str, str] = {}
    stripped = raw.lstrip()
    if stripped.startswith("{"):
        data = json.loads(raw)
        if not isinstance(data, dict):
            raise ValueError(f"reference JSON in {path} must be an object {{audio: text}}")
        for key, value in data.items():
            mapping[Path(str(key)).name] = "" if value is None else str(value)
        return mapping
    # TSV fallback.
    for lineno, line in enumerate(raw.splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if "\t" not in line:
            raise ValueError(f"{path}:{lineno}: TSV line has no tab separator: {line!r}")
        key, text = line.split("\t", 1)
        mapping[Path(key.strip()).name] = text.strip()
    return mapping


def load_terms(path: Path) -> list[str]:
    """One term per line; blank lines and '#'-comments ignored."""
    terms: list[str] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        s = line.strip()
        if not s or s.startswith("#"):
            continue
        terms.append(s)
    return terms


# --------------------------------------------------------------------------- #
# Bench file scoring
# --------------------------------------------------------------------------- #
def _iter_bench_samples(bench: dict[str, Any]) -> Iterable[tuple[str, str, str]]:
    """Yield (section, audio, text) for every transcription sample.

    Reads the structure produced by ``bench_asr.py``: top-level ``warm`` and
    ``cold`` objects each carry a ``samples`` list of {audio, text, ...}.
    """
    for section in ("warm", "cold"):
        block = bench.get(section)
        if not isinstance(block, dict):
            continue
        for sample in block.get("samples", []) or []:
            if not isinstance(sample, dict):
                continue
            yield section, str(sample.get("audio", "")), str(sample.get("text", ""))


def score_bench(bench: dict[str, Any], refs: dict[str, str], opts: Options) -> dict[str, Any]:
    """Score every bench sample, aggregating per audio file and overall.

    Each audio file is scored once using its first encountered sample text
    (warm preferred), because repeated transcriptions of the same deterministic
    sample yield identical text; we still record how many samples were seen and
    warn if texts diverge.
    """
    per_file: dict[str, dict[str, Any]] = {}
    unmatched: list[str] = []
    divergent: list[str] = []

    for _section, audio, text in _iter_bench_samples(bench):
        key = Path(audio).name
        if key not in refs:
            if key not in unmatched:
                unmatched.append(key)
            continue
        if key not in per_file:
            per_file[key] = {"audio": audio, "text": text, "n_samples": 1}
        else:
            entry = per_file[key]
            entry["n_samples"] += 1
            if entry["text"] != text and key not in divergent:
                divergent.append(key)

    rows: list[dict[str, Any]] = []
    # Micro-average accumulators.
    cer_err = cer_len = wer_err = wer_len = 0
    terms_hit = terms_total = 0

    for key in sorted(per_file):
        entry = per_file[key]
        ref = refs[key]
        scored = score_pair(ref, entry["text"], opts)
        row = {
            "audio": key,
            "n_samples": entry["n_samples"],
            "cer": scored["cer"]["cer"],
            "cer_errors": scored["cer"]["errors"],
            "cer_ref_len": scored["cer"]["ref_len"],
            "wer": scored["wer"]["wer"],
            "wer_errors": scored["wer"]["errors"],
            "wer_ref_len": scored["wer"]["ref_len"],
        }
        cer_err += scored["cer"]["errors"]
        cer_len += scored["cer"]["ref_len"]
        wer_err += scored["wer"]["errors"]
        wer_len += scored["wer"]["ref_len"]
        if "terms" in scored:
            t = scored["terms"]
            row["term_recall"] = t["recall"]
            row["terms_in_ref"] = t["terms_in_ref"]
            row["terms_hit"] = t["terms_hit"]
            terms_hit += t["terms_hit"]
            terms_total += t["terms_in_ref"]
        rows.append(row)

    overall: dict[str, Any] = {
        "files": len(rows),
        "cer": _rate(cer_err, cer_len, hyp_len=1),
        "cer_errors": cer_err,
        "cer_ref_len": cer_len,
        "wer": _rate(wer_err, wer_len, hyp_len=1),
        "wer_errors": wer_err,
        "wer_ref_len": wer_len,
    }
    if opts.terms:
        overall["term_recall"] = (terms_hit / terms_total) if terms_total else None
        overall["terms_in_ref"] = terms_total
        overall["terms_hit"] = terms_hit

    return {
        "mode": "bench",
        "rows": rows,
        "overall": overall,
        "unmatched_audio": unmatched,
        "divergent_audio": divergent,
        "config": bench.get("config", {}),
    }


def _manifest_metrics(reference: str, hypothesis: str, language: str, opts: Options) -> dict[str, Any]:
    ref = unicodedata.normalize("NFC", reference).strip()
    hyp = unicodedata.normalize("NFC", hypothesis).strip()
    chars = levenshtein(ref, hyp)
    words = levenshtein(ref.split(), hyp.split())
    raw = {
        "cer": {"errors": chars, "ref_len": len(ref), "cer": _rate(chars, len(ref), hyp_len=len(hyp))},
        "wer": {"errors": words, "ref_len": len(ref.split()), "wer": _rate(words, len(ref.split()), hyp_len=len(hyp.split()))},
    }
    normalized = score_pair(ref, hyp, opts)
    if language.lower().startswith("ja"):
        for metrics in (raw, normalized):
            metrics["wer"] = None
            metrics["wer_reason"] = JA_WER_REASON
    return {"raw": raw, "normalized": normalized}


def _aggregate_manifest(rows: list[dict[str, Any]]) -> dict[str, Any]:
    aggregate: dict[str, Any] = {"files": len(rows)}
    for policy in ("raw", "normalized"):
        metrics: dict[str, Any] = {}
        for metric in ("cer", "wer"):
            included = [row[policy][metric] for row in rows if row[policy][metric] is not None]
            if not included:
                metrics[metric] = None
                if metric == "wer":
                    metrics["wer_reason"] = JA_WER_REASON if rows else "no scored items"
                continue
            errors = sum(item["errors"] for item in included)
            length = sum(item["ref_len"] for item in included)
            metrics[metric] = {"errors": errors, "ref_len": length, metric: _rate(errors, length, hyp_len=errors)}
            if metric == "wer":
                metrics["wer_files"] = len(included)
        terms = [row[policy]["terms"] for row in rows if "terms" in row[policy]]
        if terms:
            hits = sum(item["terms_hit"] for item in terms)
            total = sum(item["terms_in_ref"] for item in terms)
            metrics["terms"] = {"terms_hit": hits, "terms_in_ref": total, "recall": hits / total if total else None}
        aggregate[policy] = metrics
    return aggregate


def score_manifest(bench: dict[str, Any], manifest: dict[str, Any], path: Path, opts: Options) -> dict[str, Any]:
    """Score each item once (first warm sample preferred), with micro averages."""
    config = bench.get("config", {})
    if not isinstance(config, dict):
        raise ValueError("bench config must be an object")
    refs = {Path(item["audio_path"]).name: item["reference"] for item in manifest["items"]}
    matched = score_bench(bench, refs, opts)
    by_audio = {row["audio"]: row for row in matched["rows"]}
    texts: dict[str, str] = {}
    for _, audio, text in _iter_bench_samples(bench):
        texts.setdefault(Path(audio).name, text)
    rows: list[dict[str, Any]] = []
    missing: list[str] = []
    groups: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for item in manifest["items"]:
        audio = Path(item["audio_path"]).name
        if audio not in by_audio:
            missing.append(item["id"])
            continue
        row = {
            "id": item["id"], "audio": audio, "source": item["source"], "language": item["language"],
            "n_samples": by_audio[audio]["n_samples"],
            **_manifest_metrics(item["reference"], texts[audio], item["language"], opts),
        }
        rows.append(row)
        groups.setdefault((item["source"], item["language"]), []).append(row)
    warnings: list[str] = []
    recorded = config.get("manifest")
    if recorded is not None:
        if not isinstance(recorded, dict):
            raise ValueError("bench config.manifest must be an object")
        for field, expected in (("name", manifest["name"]), ("item_count", len(manifest["items"]))):
            if recorded.get(field) != expected:
                warnings.append(f"bench config.manifest.{field} {recorded.get(field)!r} differs from scoring manifest {expected!r}")
    if missing:
        warnings.append(f"{len(missing)} manifest item(s) missing from bench result")
    for warning in warnings:
        print(f"[score_asr] warning: {warning}", file=sys.stderr)
    return {
        "mode": "manifest", "manifest": manifest_conditions(manifest, path), "rows": rows,
        "overall": _aggregate_manifest(rows),
        "groups": [{"source": source, "language": language, **_aggregate_manifest(items)}
                   for (source, language), items in sorted(groups.items())],
        "missing_items": missing, "unmatched_audio": matched["unmatched_audio"],
        "divergent_audio": matched["divergent_audio"], "config": bench.get("config", {}),
        "warnings": warnings,
        "normalization": {key: value for key, value in vars(opts).items() if key != "terms"},
    }


def _validate_baseline(baseline: dict[str, Any], mode: str) -> None:
    """Validate fields used by comparison before indexing or calculating deltas."""
    def fail(field: str) -> None:
        raise ValueError(f"baseline score JSON has invalid {field}")

    def count(value: Any, field: str) -> None:
        if type(value) is not int or value < 0:
            fail(field)

    def rate(value: Any, field: str) -> None:
        if type(value) not in (int, float) or value < 0 or (type(value) is float and not math.isfinite(value)):
            fail(field)

    def aggregate(value: Any, field: str) -> None:
        if not isinstance(value, dict):
            fail(field)
        count(value.get("files"), field + ".files")
        if mode == "bench":
            for metric in ("cer", "wer"):
                if metric not in value:
                    fail(field + "." + metric)
                rate(value[metric], field + "." + metric)
            return
        for policy in ("raw", "normalized"):
            if not isinstance(value.get(policy), dict):
                fail(field + "." + policy)
            for metric in ("cer", "wer"):
                name = f"{field}.{policy}.{metric}"
                if metric not in value[policy]:
                    fail(name)
                stats = value[policy][metric]
                if stats is None:
                    continue
                if not isinstance(stats, dict) or metric not in stats:
                    fail(name)
                rate(stats[metric], name)
                for key in ("errors", "ref_len"):
                    count(stats.get(key), name + "." + key)

    if not isinstance(baseline, dict) or baseline.get("mode") != mode:
        fail("mode (must match the current score)")
    if not isinstance(baseline.get("config", {}), dict):
        fail("config")
    for key in ("manifest", "normalization"):
        if key in baseline and not isinstance(baseline[key], dict):
            fail(key)
    recorded = baseline.get("config", {}).get("manifest")
    if recorded is not None and not isinstance(recorded, dict):
        fail("config.manifest")
    aggregate(baseline.get("overall"), "overall")
    groups = baseline.get("groups", [] if mode == "bench" else None)
    if not isinstance(groups, list):
        fail("groups")
    seen: set[tuple[str, str]] = set()
    for index, group in enumerate(groups):
        name = f"groups[{index}]"
        if not isinstance(group, dict):
            fail(name)
        for key in ("source", "language"):
            if not isinstance(group.get(key), str) or not group[key]:
                fail(name + "." + key)
        key = (group["source"], group["language"])
        if key in seen:
            fail(name + " (duplicate source/language)")
        seen.add(key)
        aggregate(group, name)
    missing = baseline.get("missing_items", [] if mode == "bench" else None)
    if not isinstance(missing, list) or any(not isinstance(item, str) for item in missing):
        fail("missing_items")


def compare_baseline(result: dict[str, Any], baseline: dict[str, Any]) -> dict[str, Any]:
    """Deltas are current minus baseline; missing/inapplicable metrics stay null."""
    _validate_baseline(baseline, result["mode"])

    def flatten(value: dict[str, Any], prefix: str = "") -> dict[str, Any]:
        out: dict[str, Any] = {}
        for key, item in value.items():
            if key == "timestamp":
                continue
            name = prefix + key
            if isinstance(item, dict):
                out.update(flatten(item, name + "."))
            elif isinstance(item, list):
                serialized = json.dumps(item, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
                out[name] = {"count": len(item), "sha256": hashlib.sha256(serialized.encode("utf-8")).hexdigest()[:12]}
            else:
                out[name] = item
        return out

    current = flatten(result.get("config", {}))
    previous = flatten(baseline.get("config", {}))
    # The scoring manifest is also a condition when the bench came from an older harness.
    for report, fields in ((result, current), (baseline, previous)):
        fields.update(flatten({"scoring_manifest": report.get("manifest"), "normalization": report.get("normalization")}))
        fields["scored_item_count"] = report["overall"]["files"]
        fields["missing_item_count"] = len(report.get("missing_items", []))

    def delta(now: dict[str, Any], before: dict[str, Any]) -> dict[str, Any]:
        out: dict[str, Any] = {}
        for policy in ("raw", "normalized"):
            out[policy] = {}
            for metric in ("cer", "wer"):
                a = (now.get(policy, {}).get(metric) or {}).get(metric)
                b = (before.get(policy, {}).get(metric) or {}).get(metric)
                out[policy][metric] = a - b if a is not None and b is not None else None
        return out

    old_groups = {(g["source"], g["language"]): g for g in baseline.get("groups", [])}
    groups = [{"source": g["source"], "language": g["language"],
               **delta(g, old_groups.get((g["source"], g["language"]), {}))} for g in result.get("groups", [])]
    overall = delta(result["overall"], baseline.get("overall", {}))
    if result.get("mode") == "bench":
        overall = {metric: result["overall"][metric] - baseline["overall"][metric]
                   if baseline.get("overall", {}).get(metric) is not None else None for metric in ("cer", "wer")}
    priority = ["backend", "model", "quantization", "whisper_decode_profile", "profile", "device_policy",
                "worker_health.device", "worker_health.compute_type", "observed_devices", "device",
                "manifest.name", "scoring_manifest.name", "scored_item_count", "missing_item_count"]
    ordered = sorted(current.keys() | previous.keys(), key=lambda key: (priority.index(key) if key in priority else len(priority), key))
    machine_local = {"worker_cmd", "audio", "audio_format", "manifest.path", "scoring_manifest.path"}
    return {
        "overall": overall, "groups": groups,
        "differing_conditions": [{"condition": key, "baseline": previous.get(key), "current": current.get(key)}
                                 for key in ordered
                                 if key not in machine_local and current.get(key) != previous.get(key)],
        "informational": [{"key": key, "baseline": previous.get(key), "current": current.get(key)}
                          for key in ordered if key in machine_local and current.get(key) != previous.get(key)],
    }


def write_score_csv(result: dict[str, Any], path: Path) -> None:
    rows = []
    for row in result["rows"]:
        flat = {key: row[key] for key in ("id", "audio", "source", "language", "n_samples") if key in row}
        if result["mode"] == "manifest":
            for policy in ("raw", "normalized"):
                for metric in ("cer", "wer"):
                    stats = row[policy][metric]
                    for field in (metric, "errors", "ref_len"):
                        flat[f"{policy}_{metric}" + ("" if field == metric else f"_{field}")] = stats[field] if stats else None
                flat[f"{policy}_wer_reason"] = row[policy].get("wer_reason", "")
                if "terms" in row[policy]:
                    flat[f"{policy}_term_recall"] = row[policy]["terms"]["recall"]
        else:
            flat = dict(row)
        rows.append(flat)
    fields = list(dict.fromkeys(key for row in rows for key in row))
    if not fields:
        fields = ["id", "audio", "source", "language", "n_samples"] if result["mode"] == "manifest" else ["audio", "n_samples"]
        for policy in (("raw", "normalized") if result["mode"] == "manifest" else ("",)):
            prefix = policy + "_" if policy else ""
            for metric in ("cer", "wer"):
                fields.extend(prefix + metric + suffix for suffix in ("", "_errors", "_ref_len"))
            if policy:
                fields.append(prefix + "wer_reason")
    with path.open("x", encoding="utf-8", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)


# --------------------------------------------------------------------------- #
# Rendering
# --------------------------------------------------------------------------- #
def _pct(rate: float | None) -> str:
    if rate is None:
        return "n/a"
    if rate != rate:  # NaN guard
        return "-"
    return f"{rate * 100:.2f}"


def render_pair_markdown(reference: str, hypothesis: str, scored: dict[str, Any], opts: Options) -> str:
    lines = ["# ASR accuracy (pair)", ""]
    lines.append(f"- reference: `{reference}`")
    lines.append(f"- hypothesis: `{hypothesis}`")
    lines.append("")
    lines.append("| metric | errors | ref len | rate (%) |")
    lines.append("|---|---:|---:|---:|")
    cer, wer = scored["cer"], scored["wer"]
    lines.append(f"| CER | {cer['errors']} | {cer['ref_len']} | {_pct(cer['cer'])} |")
    lines.append(f"| WER | {wer['errors']} | {wer['ref_len']} | {_pct(wer['wer'])} |")
    if "terms" in scored:
        t = scored["terms"]
        lines.append(
            f"| term recall | {t['terms_hit']}/{t['terms_in_ref']} | "
            f"{t['terms_in_ref']} | {_pct(t['recall'])} |"
        )
    return "\n".join(lines)


def render_condition_header(cfg: dict[str, Any]) -> list[str]:
    if not cfg:
        return []
    lines = [
        f"- backend: `{cfg.get('backend', '?')}`  quantization: "
        f"`{cfg.get('quantization', '?')}`  model: `{cfg.get('model', '?')}`",
    ]
    if cfg.get("whisper_decode_profile") is not None:
        lines.append(f"- profile: `{cfg['whisper_decode_profile']}`")
    health = cfg.get("worker_health") or {}
    if health or "device_policy" in cfg:
        lines.append(f"- device: `{health.get('device') or '?'}`  compute type: `{health.get('compute_type') or '?'}`"
                     f"  policy: `{cfg.get('device_policy', 'auto')}`")
    if "observed_devices" in cfg and cfg["observed_devices"]:
        devices = ", ".join(f"{d.get('device')}/{d.get('compute_type')}" for d in cfg["observed_devices"])
        lines.append(f"- observed devices: `{devices}`")
    if "language" in cfg or "language_source" in cfg:
        language = "per-item (manifest)" if cfg.get("language_source") == "manifest" else cfg.get("language") or "auto"
        lines.append(f"- language: `{language}`")
    if "request_shape" in cfg:
        lines.append(f"- request shape: `{cfg['request_shape']}`")
    if cfg.get("manifest"):
        lines.append(f"- bench manifest: `{cfg['manifest'].get('name', '?')}`")
    lines.append("")
    return lines


def render_bench_markdown(result: dict[str, Any]) -> str:
    lines = ["# ASR accuracy (bench)", "", *render_condition_header(result.get("config", {}))]
    has_terms = "term_recall" in result["overall"]
    if has_terms:
        lines.append("| audio | n | CER (%) | WER (%) | term recall (%) |")
        lines.append("|---|---:|---:|---:|---:|")
    else:
        lines.append("| audio | n | CER (%) | WER (%) |")
        lines.append("|---|---:|---:|---:|")
    for row in result["rows"]:
        base = f"| {row['audio']} | {row['n_samples']} | {_pct(row['cer'])} | {_pct(row['wer'])} |"
        if has_terms:
            tr = row.get("term_recall")
            tcell = f" {_pct(tr)} ({row.get('terms_hit', 0)}/{row.get('terms_in_ref', 0)}) |"
            base = base + tcell
        lines.append(base)
    o = result["overall"]
    overall_cells = f"| **overall** | {o['files']} | **{_pct(o['cer'])}** | **{_pct(o['wer'])}** |"
    if has_terms:
        overall_cells += f" **{_pct(o['term_recall'])}** ({o['terms_hit']}/{o['terms_in_ref']}) |"
    lines.append(overall_cells)
    lines.append("")
    lines.append(
        "> CER/WER overall are micro-averaged (sum of edit distances / sum of "
        "reference lengths). Term recall overall pools reference-present terms."
    )
    if result["unmatched_audio"]:
        lines.append("")
        lines.append(
            "> WARNING: no reference for audio: "
            + ", ".join(result["unmatched_audio"])
        )
    if result["divergent_audio"]:
        lines.append(
            "> WARNING: repeated samples produced differing text for: "
            + ", ".join(result["divergent_audio"])
            + " (scored the first occurrence)"
        )
    return "\n".join(lines)


def render_manifest_markdown(result: dict[str, Any]) -> str:
    has_terms = "terms" in result["overall"]["normalized"]
    lines = ["# ASR accuracy (manifest)", "", *render_condition_header(result.get("config", {})),
             f"- scoring manifest: `{result['manifest']['name']}`", "",
             "| item / group | samples | items | raw CER (%) | raw WER (%) | normalized CER (%) | normalized WER (%) |"
             + (" normalized term recall (%) |" if has_terms else ""),
             "|---|---:|---:|---:|---:|---:|---:|" + ("---:|" if has_terms else "")]

    def add(label: str, entry: dict[str, Any], samples: int | str, items: int | str) -> None:
        values = [_pct((entry[policy][metric] or {}).get(metric))
                  for policy in ("raw", "normalized") for metric in ("cer", "wer")]
        if has_terms:
            terms = entry["normalized"].get("terms", {})
            values.append(f"{_pct(terms.get('recall'))} ({terms.get('terms_hit', 0)}/{terms.get('terms_in_ref', 0)})")
        lines.append(f"| {label.replace('|', '/')} | {samples} | {items} | " + " | ".join(values) + " |")

    for row in result["rows"]:
        add(row["id"], row, row["n_samples"], "-")
    add("**overall**", result["overall"], "-", result["overall"]["files"])
    for group in result["groups"]:
        add(f"{group['source']} / {group['language']}", group, "-", group["files"])
    lines.extend(["", "- Aggregates: total edits / total reference length (micro average).",
                  "- WER includes only non-Japanese items.", f"- Japanese WER: n/a ({JA_WER_REASON})."])
    for field in ("missing_items", "unmatched_audio", "divergent_audio"):
        if result[field]:
            lines.append(f"- {field}: " + ", ".join(result[field]))
    lines.extend(f"- WARNING: {warning}" for warning in result.get("warnings", []))
    return "\n".join(lines)


def render_baseline_markdown(comparison: dict[str, Any]) -> str:
    lines = ["", "## Baseline deltas (current minus baseline)", ""]
    for label, entry in [("overall", comparison["overall"])] + [
        (f"{g['source']} / {g['language']}", {p: g[p] for p in ("raw", "normalized")})
        for g in comparison["groups"]
    ]:
        lines.append(f"- {label}: `{json.dumps(entry, ensure_ascii=False, sort_keys=True)}` (rates)")
    differences = comparison["differing_conditions"]
    if not differences:
        lines.append("- Differing conditions: none")
    for item in differences:
        lines.append(f"- condition `{item['condition']}`: `{item['baseline']}` -> `{item['current']}`")
    lines.extend(["", "### Informational (machine-local)", ""])
    informational = comparison.get("informational", [])
    if not informational:
        lines.append("- none")
    for item in informational:
        lines.append(f"- `{item['key']}`: `{item['baseline']}` -> `{item['current']}`")
    return "\n".join(lines)


# --------------------------------------------------------------------------- #
# Self test
# --------------------------------------------------------------------------- #
def run_self_test() -> int:
    """Run built-in known cases; return 0 on success, 1 on any failure."""
    failures: list[str] = []

    def check(name: str, got: Any, want: Any) -> None:
        ok = got == want
        if isinstance(got, float) and isinstance(want, float):
            ok = abs(got - want) < 1e-9
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}: got={got!r} want={want!r}")
        if not ok:
            failures.append(name)

    print("Levenshtein:")
    check("identical strings -> 0", levenshtein("abc", "abc"), 0)
    check("empty vs empty -> 0", levenshtein("", ""), 0)
    check("empty vs 'abc' -> 3", levenshtein("", "abc"), 3)
    check("kitten->sitting -> 3", levenshtein("kitten", "sitting"), 3)
    check("single substitution -> 1", levenshtein("cat", "bat"), 1)
    check("word list 1 sub -> 1", levenshtein(["the", "cat"], ["the", "bat"]), 1)

    print("CER:")
    check("exact match CER=0", cer_stats("こんにちは", "こんにちは")["cer"], 0.0)
    # "東京" vs "東today" : after NFKC + strip, ref="東京"(2), one char wrong
    check(
        "1/2 char error -> 0.5",
        cer_stats("東京", "東today", strip_punctuation=True)["cer"],
        # ref="東京" len2; hyp="東today"; dist = edit("東京","東today")
        levenshtein("東京", "東today") / 2,
    )
    # whitespace stripped: "a b c" vs "abc" -> identical after strip
    check("whitespace stripped CER=0", cer_stats("a b c", "abc")["cer"], 0.0)
    # punctuation stripped
    check("punct stripped CER=0", cer_stats("hello, world!", "hello world")["cer"], 0.0)
    # empty reference, non-empty hyp -> 1.0
    check("empty ref nonempty hyp -> 1.0", cer_stats("", "x")["cer"], 1.0)
    check("empty ref empty hyp -> 0.0", cer_stats("", "")["cer"], 0.0)
    # The long vowel mark is part of the word, not punctuation.
    check("long vowel mark kept (beer vs bill in katakana) -> 1/3", cer_stats("ビール", "ビル")["cer"], 1 / 3)
    check("half-width long vowel mark NFKC-matches CER=0", cer_stats("ビール", "ﾋﾞｰﾙ")["cer"], 0.0)

    print("WER:")
    check("exact match WER=0", wer_stats("the cat sat", "the cat sat")["wer"], 0.0)
    check("1 of 3 words wrong -> 1/3", wer_stats("the cat sat", "the bat sat")["wer"], 1 / 3)
    check("case-insensitive WER=0", wer_stats("The Cat", "the cat")["wer"], 0.0)
    check("punct-insensitive WER=0", wer_stats("hello, world.", "hello world")["wer"], 0.0)
    check("empty ref nonempty hyp -> 1.0", wer_stats("", "word")["wer"], 1.0)

    print("Term recall:")
    tr = term_recall_stats("VibeVoice runs fast", "vibevoice runs", ["VibeVoice", "Whisper"], lower=False)
    # "VibeVoice" present in ref and hyp (case differs but lower=False here so
    # ref="VibeVoice runs fast", term "VibeVoice" in ref yes; hyp="vibevoice runs"
    # -> "VibeVoice" not substring of hyp (case) -> miss; "Whisper" not in ref.
    check("term recall case-sensitive miss -> 0.0", tr["recall"], 0.0)
    tr2 = term_recall_stats("VibeVoice runs fast", "vibevoice runs", ["VibeVoice"], lower=True)
    check("term recall lowercased hit -> 1.0", tr2["recall"], 1.0)
    tr3 = term_recall_stats("no special terms", "no special terms", ["Acme"], lower=True)
    check("term absent from ref -> None", tr3["recall"], None)
    tr4 = term_recall_stats("Acme and Beta ship", "Acme ships", ["Acme", "Beta"], lower=True)
    check("1 of 2 ref terms hit -> 0.5", tr4["recall"], 0.5)

    print("Micro-average sanity:")
    # Pool two files: file1 ref="abcd" hyp="abcd"(0/4), file2 ref="ab" hyp="xy"(2/2)
    # overall = (0+2)/(4+2) = 2/6
    overall_rate = _rate(0 + 2, 4 + 2, hyp_len=1)
    check("micro-average CER 2/6", overall_rate, 2 / 6)

    print()
    if failures:
        print(f"SELF-TEST FAILED: {len(failures)} case(s): {', '.join(failures)}")
        return 1
    print("SELF-TEST PASSED")
    return 0


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #
def parse_args(argv: list[str] | None) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Score ASR accuracy (CER/WER/term recall) for bench_asr.py output or a text pair.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    mode = p.add_argument_group("input mode (choose one)")
    mode.add_argument("--bench", metavar="JSON", help="bench_asr.py --json output file")
    references = mode.add_mutually_exclusive_group()
    references.add_argument("--ref", metavar="TEXT_OR_FILE", help="reference: text (pair mode) or path to JSON/TSV map (bench mode)")
    references.add_argument("--manifest", metavar="FILE", help="frozen manifest references (bench mode)")
    mode.add_argument("--hyp", metavar="TEXT", help="hypothesis text (pair mode)")

    p.add_argument("--terms", metavar="FILE", help="term list file (one term per line) for term recall")
    p.add_argument("--json", action="store_true", help="emit JSON instead of markdown")
    p.add_argument("--output", metavar="PATH", help="write UTF-8 results to a new file; existing files are never overwritten")
    p.add_argument("--csv", metavar="PATH", help="write per-item rows to a new UTF-8 CSV file (bench mode)")
    p.add_argument("--baseline", metavar="PREV_SCORE_JSON", help="compare aggregate metrics and conditions with a previous score JSON")
    p.add_argument("--self-test", action="store_true", help="run built-in checks and exit")

    cer = p.add_argument_group("CER normalisation (defaults: NFKC + strip whitespace + strip punctuation)")
    cer.add_argument("--cer-keep-width", action="store_true", help="disable NFKC width/kana normalisation")
    cer.add_argument("--cer-keep-whitespace", action="store_true", help="keep whitespace (do not collapse-strip)")
    cer.add_argument("--cer-keep-punctuation", action="store_true", help="keep punctuation")

    wer = p.add_argument_group("WER normalisation (defaults: lowercase + strip punctuation)")
    wer.add_argument("--wer-nfkc", action="store_true", help="also apply NFKC before WER tokenisation")
    wer.add_argument("--wer-keep-case", action="store_true", help="do not lowercase")
    wer.add_argument("--wer-keep-punctuation", action="store_true", help="keep punctuation")

    return p.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)

    if args.self_test:
        return run_self_test()

    try:
        return _run(args)
    except OSError as exc:
        print(f"[score_asr] error: could not create or write a new result file: {exc}", file=sys.stderr)
        return 2


def _emit_report(rendered: str, args: argparse.Namespace) -> None:
    if not args.output:
        print(rendered)
        return
    path = Path(args.output)
    created: os.stat_result | None = None
    completed = False
    try:
        # Argument/input validation and rendering finish before reserving a file.
        with path.open("x", encoding="utf-8", newline="\n") as stream:
            created = os.fstat(stream.fileno())
            stream.write(rendered + "\n")
            stream.flush()
        completed = True
    except OSError:
        if created is not None:
            print(rendered)
        raise
    finally:
        if created is not None and not completed:
            # Never remove pre-existing files, replacements, or partial reports.
            with contextlib.suppress(OSError):
                current = path.lstat()
                if current.st_size == 0 and os.path.samestat(created, current):
                    path.unlink()


def _run(args: argparse.Namespace) -> int:
    if (args.manifest or args.csv or args.baseline) and not args.bench:
        print("[score_asr] error: --manifest, --csv and --baseline require --bench", file=sys.stderr)
        return 2

    args._terms = load_terms(Path(args.terms)) if args.terms else []
    opts = Options(args)

    # --- Bench mode ---
    if args.bench:
        if not args.ref and not args.manifest:
            print("[score_asr] error: --bench requires --ref or --manifest", file=sys.stderr)
            return 2
        bench_path = Path(args.bench)
        ref_path = Path(args.manifest or args.ref)
        if not bench_path.is_file():
            print(f"[score_asr] error: bench file not found: {bench_path}", file=sys.stderr)
            return 2
        if not ref_path.is_file():
            print(f"[score_asr] error: reference map not found: {ref_path}", file=sys.stderr)
            return 2
        try:
            bench = json.loads(bench_path.read_text(encoding="utf-8"))
            if not isinstance(bench, dict):
                raise ValueError("bench JSON must be an object")
            if args.manifest:
                result = score_manifest(bench, load_manifest(ref_path), ref_path, opts)
            else:
                result = score_bench(bench, load_reference_map(ref_path), opts)
            if args.baseline:
                baseline = json.loads(Path(args.baseline).read_text(encoding="utf-8"))
                result["baseline"] = compare_baseline(result, baseline)
        except (OSError, ValueError) as exc:
            print(f"[score_asr] error: {exc}", file=sys.stderr)
            return 2
        if args.csv:
            try:
                write_score_csv(result, Path(args.csv))
            except OSError as exc:
                print(f"[score_asr] error: could not create a new CSV file: {exc}", file=sys.stderr)
                return 2
        if not result["rows"] and not args.manifest:
            print(
                "[score_asr] error: no bench samples matched the reference map "
                f"(unmatched audio: {result['unmatched_audio']})",
                file=sys.stderr,
            )
            return 1
        if args.json:
            rendered = json.dumps(result, ensure_ascii=False, indent=2)
        else:
            rendered = render_manifest_markdown(result) if args.manifest else render_bench_markdown(result)
            if args.baseline:
                rendered += "\n" + render_baseline_markdown(result["baseline"])
        _emit_report(rendered, args)
        return 0 if result["rows"] else 1

    # --- Pair mode ---
    if args.hyp is not None or args.ref is not None:
        if args.hyp is None or args.ref is None:
            print("[score_asr] error: pair mode requires both --hyp and --ref", file=sys.stderr)
            return 2
        scored = score_pair(args.ref, args.hyp, opts)
        if args.json:
            out = {"mode": "pair", "reference": args.ref, "hypothesis": args.hyp, **scored}
            _emit_report(json.dumps(out, ensure_ascii=False, indent=2), args)
        else:
            _emit_report(render_pair_markdown(args.ref, args.hyp, scored, opts), args)
        return 0

    print(
        "[score_asr] error: nothing to do. Use --self-test, or pair mode "
        "(--hyp/--ref), or bench mode (--bench/--ref). See --help.",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
