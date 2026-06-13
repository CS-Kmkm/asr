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
import json
import sys
import unicodedata
from pathlib import Path
from typing import Any, Iterable

# Punctuation we strip when --strip-punctuation is active. Covers ASCII and the
# common Japanese full-width marks; not exhaustive but matches docs §3.4 intent
# ("空白除去 ... 句読点の扱いを明文化して固定").
_PUNCT_CHARS = set(
    "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~"
    "、。，．・「」『』（）〔〕［］｛｝〈〉《》【】〜ー―‐-…！？："
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


def render_bench_markdown(result: dict[str, Any]) -> str:
    cfg = result.get("config", {})
    lines = ["# ASR accuracy (bench)", ""]
    if cfg:
        lines.append(
            f"- backend: `{cfg.get('backend', '?')}`  quantization: "
            f"`{cfg.get('quantization', '?')}`  model: `{cfg.get('model', '?')}`"
        )
        lines.append("")
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
    mode.add_argument("--ref", metavar="TEXT_OR_FILE", help="reference: text (pair mode) or path to JSON/TSV map (bench mode)")
    mode.add_argument("--hyp", metavar="TEXT", help="hypothesis text (pair mode)")

    p.add_argument("--terms", metavar="FILE", help="term list file (one term per line) for term recall")
    p.add_argument("--json", action="store_true", help="emit JSON instead of markdown")
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

    args._terms = load_terms(Path(args.terms)) if args.terms else []
    opts = Options(args)

    # --- Bench mode ---
    if args.bench:
        if not args.ref:
            print("[score_asr] error: --bench requires --ref pointing to a JSON/TSV reference map", file=sys.stderr)
            return 2
        bench_path = Path(args.bench)
        ref_path = Path(args.ref)
        if not bench_path.is_file():
            print(f"[score_asr] error: bench file not found: {bench_path}", file=sys.stderr)
            return 2
        if not ref_path.is_file():
            print(f"[score_asr] error: reference map not found: {ref_path}", file=sys.stderr)
            return 2
        try:
            bench = json.loads(bench_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as exc:
            print(f"[score_asr] error: {bench_path} is not valid JSON: {exc}", file=sys.stderr)
            return 2
        refs = load_reference_map(ref_path)
        result = score_bench(bench, refs, opts)
        if not result["rows"]:
            print(
                "[score_asr] error: no bench samples matched the reference map "
                f"(unmatched audio: {result['unmatched_audio']})",
                file=sys.stderr,
            )
            return 1
        if args.json:
            print(json.dumps(result, ensure_ascii=False, indent=2))
        else:
            print(render_bench_markdown(result))
        return 0

    # --- Pair mode ---
    if args.hyp is not None or args.ref is not None:
        if args.hyp is None or args.ref is None:
            print("[score_asr] error: pair mode requires both --hyp and --ref", file=sys.stderr)
            return 2
        scored = score_pair(args.ref, args.hyp, opts)
        if args.json:
            out = {"mode": "pair", "reference": args.ref, "hypothesis": args.hyp, **scored}
            print(json.dumps(out, ensure_ascii=False, indent=2))
        else:
            print(render_pair_markdown(args.ref, args.hyp, scored, opts))
        return 0

    print(
        "[score_asr] error: nothing to do. Use --self-test, or pair mode "
        "(--hyp/--ref), or bench mode (--bench/--ref). See --help.",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
