"""Create a small, derived ASR score report for language-model context.

Inputs remain unchanged. This is a presentation view, not a score/baseline file.
Supported inputs: score_asr bench/manifest JSON, bench_asr timing JSON, and
asr-japanese-comparison/1 snapshots. Only the standard library is used.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sys
from typing import Any


def compact_config(config: dict[str, Any]) -> dict[str, Any]:
    result = {k: v for k, v in config.items() if k not in {"audio", "audio_format", "worker_cmd"}}
    if "audio" in config:
        result["audio_count"] = len(config["audio"])
    if "audio_format" in config:
        formats = []
        for item in config["audio_format"]:
            if item not in formats:
                formats.append(item)
        result["audio_formats"] = formats
    return result


def comparison_view(report: dict[str, Any]) -> dict[str, Any]:
    # Column arrays avoid repeating the same long metric keys per model.
    def check_notes(entry: dict[str, Any]) -> None:
        for key in ("warning", "warnings", "limitations"):
            if key in entry and not (
                isinstance(entry[key], str)
                or isinstance(entry[key], list) and all(isinstance(v, str) for v in entry[key])
            ):
                raise ValueError(f"{key} must be a string or list of warning strings")

    root_fields = {
        "schema", "issue", "manifest", "manifest_sha256", "lock_sha256", "unique_audio",
        "warm_repeats", "restarted_process_passes", "accuracy_sample", "timing_scope",
        "gpu_scope", "literal_probe_policy", "transcripts_included", "models",
        "warnings", "limitations",
    }
    if set(report) - root_fields:
        raise ValueError("unrecognized comparison fields; inspect source before extending the summary")
    check_notes(report)
    result = {key: value for key, value in report.items() if key != "models"}
    result["columns"] = [
        "id", "model", "backend", "runtime", "revision", "device", "compute_type",
        "items", "raw_CER_pct", "norm_CER_pct", "warm_n", "warm_p50_ms", "warm_p95_ms",
        "load_p50_ms", "restart_first_ms", "completed", "failed", "empty",
        "GPU_baseline_MiB", "GPU_peak_MiB",
    ]
    result["rows"] = []
    result["groups"] = []
    result["group_columns"] = ["id", "source", "language", "items", "raw_CER_pct", "norm_CER_pct"]
    result["literal_probes"] = {}
    result["model_notes"] = {}

    def cer(entry: dict[str, Any], policy: str) -> float | None:
        value = ((entry.get(policy) or {}).get("cer") or {}).get("cer")
        return None if value is None else round(value * 100, 3)

    known = {
        "source_result_sha256", "backend", "model", "model_revision", "runtime", "worker_health",
        "observed_devices", "overall", "groups", "warm_wall_ms", "warm_wall_ms_by_audio_duration",
        "warm_real_time_factor", "load_request_ms", "first_request_after_restart_wall_ms",
        "completed_requests", "failed_requests", "empty_first_outputs", "literal_probes",
        "gpu_memory_used_mib_baseline", "gpu_memory_used_mib_max", "gpu_utilization_percent_max",
        "warning", "warnings", "limitations",
    }
    for name, model in report["models"].items():
        if set(model) - known:
            raise ValueError(f"unrecognized model fields for {name}; inspect source before extending the summary")
        check_notes(model)
        overall = model["overall"]
        health = model.get("worker_health") or {}
        warm = model.get("warm_wall_ms") or {}
        result["rows"].append([
            name, model.get("model"), model.get("backend"), model.get("runtime"),
            model.get("model_revision"), health.get("device"), health.get("compute_type"),
            overall.get("files"), cer(overall, "raw"), cer(overall, "normalized"),
            warm.get("count"), warm.get("p50"), warm.get("p95"),
            (model.get("load_request_ms") or {}).get("p50"),
            model.get("first_request_after_restart_wall_ms"), model.get("completed_requests"),
            model.get("failed_requests"), model.get("empty_first_outputs"),
            model.get("gpu_memory_used_mib_baseline"), model.get("gpu_memory_used_mib_max"),
        ])
        for group in model.get("groups", []):
            result["groups"].append([
                name, group.get("source"), group.get("language"), group.get("files"),
                cer(group, "raw"), cer(group, "normalized"),
            ])
        if "literal_probes" in model:
            result["literal_probes"][name] = model["literal_probes"]
        notes = {key: model[key] for key in ("warning", "warnings", "limitations") if key in model}
        # Keep evidence of actual-device differences, rather than only policy.
        if model.get("observed_devices"):
            notes["observed_devices"] = model["observed_devices"]
        if notes:
            result["model_notes"][name] = notes
    result["view_note"] = (
        "CER in percent rounded to 3 decimals; timings unchanged. null=unrecorded. "
        "Aggregate overview omits timing distributions, per-model hashes and detailed error counts; "
        "read source for full evidence. No ranking or non-Japanese/dictionary benefit is inferred."
    )
    return result


def summarize(report: dict[str, Any]) -> dict[str, Any]:
    if report.get("schema") == "asr-japanese-comparison/1":
        return comparison_view(report)
    if report.get("mode") in {"bench", "manifest"} and "overall" in report and "rows" in report:
        item_lists = {"missing_items", "unmatched_audio", "divergent_audio"}
        result = {key: value for key, value in report.items() if key not in {"rows", "config"} | item_lists}
        result["config"] = compact_config(report.get("config", {}))
        result["omitted_item_rows"] = len(report["rows"])
        result["item_issue_counts"] = {key: len(report[key]) for key in sorted(item_lists) if key in report}
        # Keep group aggregates, normalization, baseline differences and every warning.
        return result
    if "config" in report and "warm" in report and "cold" in report:
        return {
            "config": compact_config(report["config"]),
            **{phase: {key: value for key, value in report[phase].items() if key != "samples"}
               for phase in ("warm", "cold")},
            "omitted_samples": {phase: len(report[phase].get("samples", [])) for phase in ("warm", "cold")},
            "timing_scope": "worker request/response, not desktop stop-to-paste",
        }
    raise ValueError("unsupported input: use score_asr, bench_asr, or asr-japanese-comparison/1 JSON")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    parser.add_argument("--output", type=Path, help="new UTF-8 file; existing files are never overwritten")
    args = parser.parse_args(argv)
    try:
        raw = args.input.read_bytes()
        report = json.loads(raw)
        if not isinstance(report, dict):
            raise ValueError("report must be a JSON object")
        result = {
            "view": "asr-benchmark-context/1", "source": args.input.name,
            "source_sha256": hashlib.sha256(raw).hexdigest(),
            "summary": summarize(report),
        }
        rendered = json.dumps(result, ensure_ascii=False, separators=(",", ":"), allow_nan=False) + "\n"
        if args.output:
            with args.output.open("x", encoding="utf-8", newline="\n") as stream:
                stream.write(rendered)
        else:
            print(rendered, end="")
        return 0
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as exc:
        print(f"[summarize_bench] {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
