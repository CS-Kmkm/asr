from __future__ import annotations

import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("summarize_bench", ROOT / "scripts/summarize_bench.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class SummaryTests(unittest.TestCase):
    def test_score_preserves_groups_conditions_warnings_and_missing_values(self):
        score = {
            "mode": "manifest", "rows": [{"id": str(i), "text": "private"} for i in range(200)],
            "overall": {"normalized": {"cer": {"cer": .05}, "wer": None}},
            "groups": [{"source": "fixture", "files": 200}],
            "warnings": ["missing items"], "missing_items": ["missing"],
            "normalization": {"cer_nfkc": True}, "baseline": {"differing_conditions": ["device"]},
            "config": {"audio": ["private.wav"] * 200, "audio_format": [{"rate": 16000}] * 200,
                       "worker_cmd": ["secret"], "worker_health": {"device": "cuda"}, "model": "fixture"},
        }
        before = copy.deepcopy(score)
        result = MODULE.summarize(score)
        self.assertEqual(score, before)
        for field in ("overall", "groups", "warnings", "normalization", "baseline"):
            self.assertEqual(result[field], score[field])
        self.assertEqual(result["item_issue_counts"]["missing_items"], 1)
        self.assertNotIn("missing_items", result)
        self.assertEqual(result["config"]["audio_formats"], [{"rate": 16000}])
        self.assertEqual(result["config"]["audio_count"], 200)
        self.assertNotIn("private", json.dumps(result))
        self.assertLess(len(json.dumps(result)), len(json.dumps(score)) / 4)

    def test_timing_keeps_all_aggregate_metrics_without_transcripts(self):
        report = {"config": {"model": "fixture"},
                  "warm": {"wall_ms": {"p50": 20, "p95": 30}, "samples": [{"text": "private"}]},
                  "cold": {"load_ms": {"p50": None}, "samples": []}}
        result = MODULE.summarize(report)
        self.assertEqual(result["warm"]["wall_ms"], report["warm"]["wall_ms"])
        self.assertIsNone(result["cold"]["load_ms"]["p50"])
        self.assertNotIn("private", json.dumps(result))

    def test_committed_comparison_matches_all_models_and_domains(self):
        path = ROOT / "docs/eval_set/results/issue-89-qwen3-holdout-v1.json"
        report = json.loads(path.read_text(encoding="utf-8"))
        result = MODULE.summarize(report)
        self.assertEqual(len(result["rows"]), len(report["models"]))
        for row in result["rows"]:
            values = dict(zip(result["columns"], row))
            model = report["models"][values["id"]]
            self.assertEqual(values["norm_CER_pct"], round(model["overall"]["normalized"]["cer"]["cer"] * 100, 3))
            self.assertEqual(values["warm_p50_ms"], model["warm_wall_ms"]["p50"])
            self.assertEqual(values["failed"], model["failed_requests"])
        self.assertEqual(len(result["groups"]), sum(len(m["groups"]) for m in report["models"].values()))

    def test_missing_is_not_zero_and_unknown_notes_are_preserved(self):
        result = MODULE.summarize({"schema": "asr-japanese-comparison/1", "models": {
            "m": {"overall": {}, "warning": "unmeasured", "failed_requests": 2}}})
        values = dict(zip(result["columns"], result["rows"][0]))
        self.assertIsNone(values["norm_CER_pct"])
        self.assertIsNone(values["warm_p50_ms"])
        self.assertEqual(values["failed"], 2)
        self.assertEqual(result["model_notes"]["m"]["warning"], "unmeasured")

    def test_cli_does_not_overwrite_input_or_existing_output(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "in.json"
            source.write_text('{"mode":"bench","rows":[],"overall":{}}', encoding="utf-8")
            original = source.read_bytes()
            self.assertEqual(MODULE.main([str(source), "--output", str(source)]), 2)
            self.assertEqual(source.read_bytes(), original)
            target = Path(directory) / "out.json"
            self.assertEqual(MODULE.main([str(source), "--output", str(target)]), 0)
            self.assertEqual(json.loads(target.read_text())["source"], "in.json")
            target_before = target.read_bytes()
            self.assertEqual(MODULE.main([str(source), "--output", str(target)]), 2)
            self.assertEqual(target.read_bytes(), target_before)

    def test_unsupported_schema_is_rejected(self):
        with self.assertRaises(ValueError):
            MODULE.summarize({"schema": "future/1"})

    def test_unknown_comparison_extensions_cannot_export_item_transcripts(self):
        for field in ("samples", "transcripts", "rows", "custom_conditions"):
            report = {"schema": "asr-japanese-comparison/1", "models": {"m": {"overall": {}, field: [{"text": "private"}]}}}
            with self.assertRaises(ValueError):
                MODULE.summarize(report)
            with self.assertRaises(ValueError):
                MODULE.summarize({"schema": "asr-japanese-comparison/1", "models": {}, field: ["private"]})
        for field in ("warnings", "limitations"):
            with self.assertRaises(ValueError):
                MODULE.summarize({"schema": "asr-japanese-comparison/1", "models": {}, field: [{"text": "private"}]})
