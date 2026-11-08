from __future__ import annotations

import contextlib
import copy
import csv
import importlib.util
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

_SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "score_asr.py"
_SPEC = importlib.util.spec_from_file_location("score_manifest_script", _SCRIPT)
assert _SPEC is not None and _SPEC.loader is not None
score_asr = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = score_asr
_SPEC.loader.exec_module(score_asr)


class ManifestScoreTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.path = self.root / "manifests" / "synthetic-smoke.json"
        self.path.parent.mkdir()
        self.manifest = {
            "schema": "asr-bench-manifest/1", "name": "synthetic-smoke",
            "created_by": "test", "selection": {"profile": "smoke", "seed": 1},
            "sources": [{"source": "fixture", "revision": "synthetic", "license": "test"}],
            "items": [
                self.item("ja", " あ い。 ", "ja-JP"),
                self.item("en", "hello, world!", "en-US"),
                self.item("short", "a", "en-US"),
                self.item("missing", "unseen", "en-US"),
            ],
        }
        self.path.write_text(json.dumps(self.manifest, ensure_ascii=False), encoding="utf-8")
        self.bench = {
            "config": {
                "backend": "mock", "model": "mock", "quantization": "4bit",
                "whisper_decode_profile": "standard", "device_policy": "cpu",
                "worker_health": {"device": "cpu", "compute_type": "int8"},
                "manifest": {"name": "synthetic-smoke", "item_count": 4}, "timestamp": "now",
            },
            "warm": {"samples": [
                {"audio": "elsewhere/ja.wav", "text": "あい"},
                {"audio": "elsewhere/en.wav", "text": "hello world"},
                {"audio": "elsewhere/short.wav", "text": "b"},
                {"audio": "elsewhere/extra.wav", "text": "extra"},
                {"audio": "elsewhere/en.wav", "text": "different"},
            ]},
            "cold": {"samples": [{"audio": "en.wav", "text": "cold differs"}]},
        }

    def item(self, item_id: str, reference: str, language: str) -> dict:
        return {
            "id": item_id, "audio_path": f"../audio/{item_id}.wav", "reference": reference,
            "language": language, "source": "fixture", "duration": 0.01,
            "split": "test", "pool": "dev", "dataset_id": "synthetic", "revision": "synthetic",
            "locale": language, "item_id": item_id, "group": item_id, "speaker": None,
            "audio_sha256": "0" * 64, "source_audio_sha256": "0" * 64,
            "provenance": "synthetic", "license": "test",
        }

    def options(self, *flags: str):  # noqa: ANN202
        args = score_asr.parse_args(list(flags))
        args._terms = []
        return score_asr.Options(args)

    def score(self, *flags: str) -> dict:
        return score_asr.score_manifest(self.bench, score_asr.load_manifest(self.path), self.path, self.options(*flags))

    def cli(self, *flags: str) -> tuple[int, str, str]:
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            status = score_asr.main(list(flags))
        return status, stdout.getvalue(), stderr.getvalue()

    def test_loader_resolves_relative_and_absolute_paths(self) -> None:
        loaded = score_asr.load_manifest(self.path)
        self.assertEqual(Path(loaded["items"][0]["audio_path"]), self.root / "audio" / "ja.wav")
        self.manifest["items"][0]["audio_path"] = str(self.root / "ja.wav")
        self.path.write_text(json.dumps(self.manifest), encoding="utf-8")
        self.assertEqual(score_asr.load_manifest(self.path)["items"][0]["audio_path"], str(self.root / "ja.wav"))

    def test_loader_rejects_schema_duplicates_and_malformed_fields(self) -> None:
        invalid = []
        for field, value in (("schema", "asr-bench-manifest/2"), ("items", {}), ("items", []), ("sources", None), ("name", 1)):
            data = copy.deepcopy(self.manifest)
            data[field] = value
            invalid.append(data)
        data = copy.deepcopy(self.manifest)
        data["items"].append(data["items"][0])
        invalid.append(data)
        for field, value in (("reference", None), ("language", ""), ("audio_path", "wrong.wav")):
            data = copy.deepcopy(self.manifest)
            data["items"][0][field] = value
            invalid.append(data)
        for data in invalid:
            with self.subTest(data=data):
                self.path.write_text(json.dumps(data), encoding="utf-8")
                with self.assertRaises(ValueError):
                    score_asr.load_manifest(self.path)

    def test_raw_normalized_japanese_rule_and_micro_averages(self) -> None:
        result = self.score()
        ja, en, short = result["rows"]
        self.assertEqual(ja["raw"]["cer"], {"errors": 2, "ref_len": 4, "cer": 0.5})
        self.assertEqual(ja["normalized"]["cer"]["cer"], 0.0)
        for policy in ("raw", "normalized"):
            self.assertIsNone(ja[policy]["wer"])
            self.assertEqual(ja[policy]["wer_reason"], "ja: no word segmentation; CER is primary")
        self.assertEqual(en["raw"]["wer"]["wer"], 1.0)
        self.assertEqual(en["normalized"]["wer"]["wer"], 0.0)
        overall = result["overall"]
        self.assertEqual(overall["normalized"]["cer"]["cer"], 1 / 13)
        self.assertEqual(overall["normalized"]["wer"]["wer"], 1 / 3)
        self.assertEqual(overall["normalized"]["wer_files"], 2)
        for policy in ("raw", "normalized"):
            for metric in ("cer", "wer"):
                included = [r[policy][metric] for r in result["rows"] if r[policy][metric] is not None]
                self.assertEqual(overall[policy][metric][metric],
                                 sum(m["errors"] for m in included) / sum(m["ref_len"] for m in included))
        groups = {g["language"]: g for g in result["groups"]}
        self.assertIsNone(groups["ja-JP"]["raw"]["wer"])
        self.assertEqual(groups["en-US"]["normalized"]["cer"]["cer"], 1 / 11)
        self.assertEqual(short["raw"]["cer"]["cer"], 1.0)

    def test_raw_preserves_internal_whitespace_case_and_width_but_applies_nfc(self) -> None:
        opts = self.options()
        metrics = score_asr._manifest_metrics("  e\u0301  Ａ!  ", "é A", "en-US", opts)
        self.assertEqual(metrics["raw"]["cer"]["ref_len"], 5)
        self.assertEqual(metrics["raw"]["cer"]["errors"], 3)
        self.assertEqual(metrics["normalized"]["cer"]["cer"], 0.0)
        self.assertGreater(metrics["raw"]["wer"]["wer"], 0)

    def test_normalization_flags_and_all_japanese_wer(self) -> None:
        row = self.score("--cer-keep-punctuation", "--wer-keep-case", "--wer-keep-punctuation")["rows"][1]
        self.assertGreater(row["normalized"]["cer"]["cer"], 0)
        self.assertEqual(row["normalized"]["wer"]["wer"], 1.0)
        metrics = score_asr._manifest_metrics("東京", "東京", "JA_jp", self.options())
        overall = score_asr._aggregate_manifest([metrics])
        self.assertIsNone(overall["normalized"]["wer"])
        self.assertEqual(overall["raw"]["wer_reason"], score_asr.JA_WER_REASON)

    def test_missing_extra_and_divergent_samples_use_first_warm(self) -> None:
        result = self.score()
        self.assertEqual(result["missing_items"], ["missing"])
        self.assertEqual(result["unmatched_audio"], ["extra.wav"])
        self.assertEqual(result["divergent_audio"], ["en.wav"])
        self.assertEqual(result["rows"][1]["n_samples"], 3)
        self.assertEqual(result["rows"][1]["normalized"]["wer"]["wer"], 0)
        self.assertEqual(result["config"], self.bench["config"])
        self.assertNotIn("conditions", result)

    def test_empty_references_have_consistent_aggregate_rates(self) -> None:
        for hypothesis, expected in (("", 0.0), ("inserted", 1.0)):
            row = score_asr._manifest_metrics("", hypothesis, "en-US", self.options())
            overall = score_asr._aggregate_manifest([row])
            for policy in ("raw", "normalized"):
                for metric in ("cer", "wer"):
                    self.assertEqual(overall[policy][metric][metric], expected)

    def test_json_csv_baseline_and_markdown_cli(self) -> None:
        bench_path = self.root / "bench.json"
        bench_path.write_text(json.dumps(self.bench), encoding="utf-8")
        csv_path = self.root / "score.csv"
        flags = ["--bench", str(bench_path), "--manifest", str(self.path)]
        status, stdout, stderr = self.cli(*flags, "--json", "--csv", str(csv_path))
        self.assertEqual(status, 0)
        self.assertIn("warning: 1 manifest item(s) missing", stderr)
        report = json.loads(stdout)
        with csv_path.open(encoding="utf-8", newline="") as stream:
            rows = list(csv.DictReader(stream))
        self.assertEqual(len(rows), 3)
        self.assertEqual(rows[0]["raw_wer"], "")
        self.assertEqual(rows[0]["raw_wer_reason"], score_asr.JA_WER_REASON)
        self.assertEqual(float(rows[1]["normalized_wer"]), 0)
        saved = csv_path.read_bytes()
        self.assertEqual(self.cli(*flags, "--csv", str(csv_path))[0], 2)
        self.assertEqual(csv_path.read_bytes(), saved)
        baseline_path = self.root / "previous.json"
        report["config"] = copy.deepcopy(report["config"])
        report["config"].update(backend="previous", model="previous", quantization="8bit",
                                    whisper_decode_profile="fast", device_policy="cuda", timestamp="old")
        report["config"]["manifest"]["name"] = "previous"
        report["overall"]["normalized"]["cer"]["cer"] = 0.5
        baseline_path.write_text(json.dumps(report), encoding="utf-8")
        status, stdout, stderr = self.cli(*flags, "--json", "--baseline", str(baseline_path))
        self.assertEqual(status, 0)
        self.assertIn("warning: 1 manifest item(s) missing", stderr)
        comparison = json.loads(stdout)["baseline"]
        self.assertAlmostEqual(comparison["overall"]["normalized"]["cer"], 1 / 13 - 0.5)
        self.assertEqual(comparison["groups"][0]["normalized"]["cer"], 0)
        keys = {d["condition"] for d in comparison["differing_conditions"]}
        self.assertTrue({"backend", "model", "quantization", "whisper_decode_profile", "device_policy", "manifest.name"} <= keys)
        self.assertNotIn("timestamp", keys)
        status, markdown, _ = self.cli(*flags, "--baseline", str(baseline_path))
        self.assertEqual(status, 0)
        for text in ("raw CER", "normalized CER", "fixture / ja-JP", "missing_items", "extra.wav", "Baseline deltas", "condition `backend`"):
            self.assertIn(text, markdown)

    def test_no_matches_still_reports_missing_and_unmatched(self) -> None:
        self.bench["warm"]["samples"] = [{"audio": "extra.wav", "text": "x"}]
        self.bench["cold"] = {}
        path = self.root / "empty.json"
        path.write_text(json.dumps(self.bench), encoding="utf-8")
        csv_path = self.root / "no-matches.csv"
        status, stdout, _ = self.cli("--bench", str(path), "--manifest", str(self.path), "--json", "--csv", str(csv_path))
        self.assertEqual(status, 1)
        report = json.loads(stdout)
        self.assertEqual(len(report["missing_items"]), 4)
        self.assertEqual(report["unmatched_audio"], ["extra.wav"])
        with csv_path.open(encoding="utf-8", newline="") as stream:
            reader = csv.DictReader(stream)
            self.assertIn("normalized_cer", reader.fieldnames)
            self.assertIn("raw_wer_reason", reader.fieldnames)
            self.assertEqual(list(reader), [])

    def test_empty_manifest_is_rejected_with_a_specific_message(self) -> None:
        self.manifest["items"] = []
        self.path.write_text(json.dumps(self.manifest), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "manifest must contain at least one item"):
            score_asr.load_manifest(self.path)
        bench_path = self.root / "bench.json"
        bench_path.write_text(json.dumps(self.bench), encoding="utf-8")
        status, _, stderr = self.cli("--bench", str(bench_path), "--manifest", str(self.path))
        self.assertEqual(status, 2)
        self.assertIn("manifest must contain at least one item", stderr)

    def test_legacy_bench_with_no_matches_also_writes_csv_header(self) -> None:
        bench_path = self.root / "bench.json"
        bench_path.write_text(json.dumps(self.bench), encoding="utf-8")
        refs_path = self.root / "refs.json"
        refs_path.write_text(json.dumps({"absent.wav": "not recorded"}), encoding="utf-8")
        csv_path = self.root / "no-legacy-matches.csv"
        status, _, _ = self.cli("--bench", str(bench_path), "--ref", str(refs_path), "--csv", str(csv_path))
        self.assertEqual(status, 1)
        with csv_path.open(encoding="utf-8", newline="") as stream:
            reader = csv.DictReader(stream)
            self.assertIn("cer_ref_len", reader.fieldnames)
            self.assertEqual(list(reader), [])

    def test_manifest_mismatch_and_missing_items_warn_without_failing(self) -> None:
        self.bench["config"]["manifest"] = {"name": "different", "item_count": 10}
        bench_path = self.root / "bench.json"
        bench_path.write_text(json.dumps(self.bench), encoding="utf-8")
        status, stdout, stderr = self.cli("--bench", str(bench_path), "--manifest", str(self.path), "--json")
        self.assertEqual(status, 0)
        report = json.loads(stdout)
        self.assertEqual(len(report["warnings"]), 3)
        for warning in report["warnings"]:
            self.assertIn("warning: " + warning, stderr)
        self.bench["config"]["manifest"] = {"name": "synthetic-smoke", "item_count": 4}
        self.bench["warm"]["samples"].append({"audio": "missing.wav", "text": "unseen"})
        with contextlib.redirect_stderr(io.StringIO()) as stream:
            report = self.score()
        self.assertEqual(report["warnings"], [])
        self.assertEqual(stream.getvalue(), "")

    def test_malformed_baselines_raise_value_error_and_exit_two(self) -> None:
        result = self.score()
        bench_path = self.root / "bench.json"
        bench_path.write_text(json.dumps(self.bench), encoding="utf-8")
        baseline_path = self.root / "invalid-baseline.json"
        invalid = [[], None, {"mode": "pair"}]
        for keys, value in (
            (("groups",), None), (("groups",), {}), (("groups", 0), "bad"),
            (("groups", 0), {}), (("groups", 0, "source"), []), (("groups", 0, "language"), None),
            (("groups", 0, "raw"), []), (("groups", 0, "files"), "bad"),
            (("groups", 0, "normalized", "cer"), 1),
            (("groups", 0, "normalized", "wer"), {"errors": 0, "ref_len": 1}),
            (("overall",), []), (("overall", "files"), -1), (("overall", "raw"), None),
            (("overall", "normalized", "cer", "cer"), "bad"),
            (("overall", "normalized", "cer", "cer"), None),
            (("overall", "normalized", "cer", "cer"), True),
            (("overall", "normalized", "cer", "cer"), float("nan")),
            (("overall", "raw", "cer", "errors"), -1),
            (("config",), []), (("config", "manifest"), []),
            (("manifest",), []), (("normalization",), "bad"), (("missing_items",), [None]),
        ):
            data = copy.deepcopy(result)
            target = data
            for key in keys[:-1]:
                target = target[key]
            target[keys[-1]] = value
            invalid.append(data)
        duplicate = copy.deepcopy(result)
        duplicate["groups"].append(duplicate["groups"][0])
        invalid.append(duplicate)
        for data in invalid:
            with self.subTest(baseline=data):
                with self.assertRaises(ValueError):
                    score_asr.compare_baseline(result, data)
                baseline_path.write_text(json.dumps(data), encoding="utf-8")
                status, _, stderr = self.cli("--bench", str(bench_path), "--manifest", str(self.path),
                                             "--baseline", str(baseline_path))
                self.assertEqual(status, 2)
                self.assertIn("baseline score JSON", stderr)
                self.assertNotIn("Traceback", stderr)

    def test_baseline_prioritizes_conditions_summarizes_lists_and_compares_counts(self) -> None:
        result = self.score()
        result["config"]["worker_cmd"] = ["local-python", "-m", "asr_worker"]
        result["config"]["audio"] = [f"local/{i}.wav" for i in range(100)]
        result["config"]["audio_format"] = [{"sample_rate": 16000}]
        previous = copy.deepcopy(result)
        previous["config"].update(backend="old", model="old", quantization="8bit",
                                   whisper_decode_profile="fast", device_policy="cuda", worker_cmd=["remote-python"],
                                   audio=[f"remote/{i}.wav" for i in range(90)])
        previous["config"]["audio_format"] = [{"sample_rate": 24000}]
        previous["config"]["manifest"].update(name="old", path="remote/manifest.json")
        previous["manifest"]["path"] = "remote/scoring.json"
        previous["overall"]["files"] = 2
        previous["missing_items"].append("another")
        comparison = score_asr.compare_baseline(result, previous)
        differences = comparison["differing_conditions"]
        self.assertEqual([d["condition"] for d in differences[:6]],
                         ["backend", "model", "quantization", "whisper_decode_profile", "device_policy", "manifest.name"])
        by_key = {d["condition"]: d for d in differences}
        for key in ("worker_cmd", "audio", "manifest.path", "scoring_manifest.path"):
            self.assertNotIn(key, by_key)
        self.assertIn("audio_format", by_key)
        self.assertEqual(by_key["scored_item_count"], {"condition": "scored_item_count", "baseline": 2, "current": 3})
        self.assertEqual(by_key["missing_item_count"]["baseline"], 2)
        self.assertEqual(by_key["missing_item_count"]["current"], 1)
        informational = {d["key"]: d for d in comparison["informational"]}
        self.assertEqual(set(informational), {"worker_cmd", "audio", "manifest.path", "scoring_manifest.path"})
        summary = informational["audio"]["current"]
        self.assertEqual(summary["count"], 100)
        self.assertEqual(len(summary["sha256"]), 12)
        self.assertNotIn("local/0.wav", score_asr.render_baseline_markdown(comparison))
        self.assertNotIn("remote/0.wav", json.dumps(comparison))
        markdown = score_asr.render_baseline_markdown(comparison)
        self.assertIn("### Informational (machine-local)", markdown)
        self.assertIn("- `audio`:", markdown)
        self.assertIn("- `worker_cmd`:", markdown)
        self.assertNotIn("condition `audio`", markdown)

    def test_only_machine_local_changes_are_informational(self) -> None:
        result = self.score()
        result["config"]["audio"] = [str(self.root / "audio" / "en.wav")]
        result["config"]["audio_format"] = [{"sample_rate": 16000}]
        result["config"]["worker_cmd"] = ["local-python"]
        previous = copy.deepcopy(result)
        previous["config"]["audio"] = [str(self.root / "other-pc" / "en.wav")]
        previous["config"]["worker_cmd"] = ["other-python"]
        previous["config"]["manifest"]["path"] = "other/manifest.json"
        previous["manifest"]["path"] = "other/scoring.json"
        comparison = score_asr.compare_baseline(result, previous)
        self.assertEqual(comparison["differing_conditions"], [])
        self.assertEqual(len(comparison["informational"]), 4)
        markdown = score_asr.render_baseline_markdown(comparison)
        self.assertIn("Differing conditions: none", markdown)
        self.assertIn("Informational (machine-local)", markdown)

    def test_audio_format_changes_are_comparison_conditions(self) -> None:
        result = self.score()
        result["config"]["audio_format"] = [{"sample_rate": 16000, "channels": 1, "sample_width_bytes": 2}]
        for field, value in (("sample_rate", 24000), ("channels", 2), ("sample_width_bytes", 3)):
            with self.subTest(field=field):
                previous = copy.deepcopy(result)
                previous["config"]["audio_format"][0][field] = value
                comparison = score_asr.compare_baseline(result, previous)
                self.assertEqual([item["condition"] for item in comparison["differing_conditions"]], ["audio_format"])
                self.assertNotIn("Differing conditions: none", score_asr.render_baseline_markdown(comparison))

    def test_manifest_markdown_reuses_header_and_distinguishes_counts_and_terms(self) -> None:
        opts = self.options()
        opts.terms = ["hello", "world"]
        report = score_asr.score_manifest(self.bench, score_asr.load_manifest(self.path), self.path, opts)
        legacy = score_asr.score_bench(self.bench, {"en.wav": "hello, world!"}, opts)
        manifest_md = score_asr.render_manifest_markdown(report)
        legacy_md = score_asr.render_bench_markdown(legacy)
        for line in score_asr.render_condition_header(self.bench["config"]):
            self.assertIn(line, manifest_md)
            self.assertIn(line, legacy_md)
        for text in ("backend: `mock`", "model: `mock`", "quantization: `4bit`", "profile: `standard`",
                     "device: `cpu`", "policy: `cpu`", "| samples | items |", "normalized term recall (%)",
                     "| en | 3 | - |", "| **overall** | - | 3 |", "100.00 (2/2)"):
            self.assertIn(text, manifest_md)

    def test_output_writes_reports_and_never_overwrites(self) -> None:
        bench_path = self.root / "bench.json"
        bench_path.write_text(json.dumps(self.bench), encoding="utf-8")
        flags = ["--bench", str(bench_path), "--manifest", str(self.path)]
        for suffix, extra in (("json", ["--json"]), ("md", [])):
            output = self.root / f"report.{suffix}"
            status, stdout, _ = self.cli(*flags, *extra, "--output", str(output))
            self.assertEqual((status, stdout), (0, ""))
            saved = output.read_bytes()
            if suffix == "json":
                report = json.loads(saved.decode("utf-8"))
                self.assertEqual(report["config"], self.bench["config"])
                self.assertNotIn("conditions", report)
            else:
                self.assertIn("# ASR accuracy (manifest)", saved.decode("utf-8"))
            self.assertEqual(self.cli(*flags, "--output", str(output))[0], 2)
            self.assertEqual(output.read_bytes(), saved)
        output = self.root / "pair.json"
        self.assertEqual(self.cli("--ref", "a", "--hyp", "b", "--json", "--output", str(output))[:2], (0, ""))
        self.assertEqual(json.loads(output.read_text(encoding="utf-8"))["mode"], "pair")
        self.assertEqual(self.cli(*flags, "--output", str(self.root / "absent" / "report.json"))[0], 2)

    def test_invalid_arguments_or_inputs_do_not_reserve_output_and_allow_retry(self) -> None:
        bench_path = self.root / "bench.json"
        bench_path.write_text(json.dumps(self.bench), encoding="utf-8")
        invalid_baseline = self.root / "bad-baseline.json"
        invalid_baseline.write_text("{}", encoding="utf-8")
        invalid_manifest = self.root / "bad-manifest.json"
        invalid_manifest.write_text("{}", encoding="utf-8")
        output = self.root / "retry.json"
        for flags in (
            [], ["--hyp", "unpaired"], ["--csv", "no-bench.csv"],
            ["--bench", str(self.root / "absent.json"), "--manifest", str(self.path)],
            ["--bench", str(bench_path), "--manifest", str(invalid_manifest)],
            ["--bench", str(bench_path), "--manifest", str(self.path), "--baseline", str(invalid_baseline)],
        ):
            with self.subTest(flags=flags):
                self.assertEqual(self.cli(*flags, "--output", str(output))[0], 2)
                self.assertFalse(output.exists())
        self.assertEqual(self.cli("--hyp", "a", "--ref", "a", "--json", "--output", str(output))[0], 0)
        self.assertEqual(json.loads(output.read_text(encoding="utf-8"))["cer"]["cer"], 0)

    def test_failed_output_removes_only_its_own_empty_file(self) -> None:
        original_open = Path.open
        for failure in ("empty", "partial", "replacement"):
            with self.subTest(failure=failure):
                output = self.root / f"failed-{failure}.json"

                def failing_open(path, mode="r", *args, **kwargs):  # noqa: ANN001, ANN202
                    stream = original_open(path, mode, *args, **kwargs)
                    if mode == "x":
                        def fail_write(text):  # noqa: ANN001, ANN202
                            if failure == "partial":
                                os.write(stream.fileno(), b"partial report")
                            elif failure == "replacement":
                                stream.close()
                                path.rename(self.root / "original-output.json")
                                path.write_bytes(b"")
                            raise OSError("synthetic write failure")

                        stream.write = fail_write
                    return stream

                with patch.object(Path, "open", new=failing_open):
                    status, stdout, stderr = self.cli("--ref", "a", "--hyp", "a", "--json", "--output", str(output))
                self.assertEqual(status, 2)
                self.assertIn("synthetic write failure", stderr)
                self.assertEqual(json.loads(stdout)["mode"], "pair")
                if failure == "empty":
                    self.assertFalse(output.exists())
                    self.assertEqual(self.cli("--ref", "a", "--hyp", "a", "--output", str(output))[0], 0)
                else:
                    self.assertTrue(output.exists())
                    self.assertEqual(output.read_bytes(), b"partial report" if failure == "partial" else b"")
        existing = self.root / "existing-empty.json"
        existing.write_bytes(b"")
        self.assertEqual(self.cli("--ref", "a", "--hyp", "a", "--output", str(existing))[0], 2)
        self.assertTrue(existing.exists())

    def test_invalid_cli_and_existing_pair_bench_defaults(self) -> None:
        for flags in (("--manifest", str(self.path)), ("--csv", "new.csv"), ("--baseline", "old.json"), ("--bench", "x")):
            self.assertEqual(self.cli(*flags)[0], 2)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            score_asr.parse_args(["--manifest", str(self.path), "--ref", "ref.json"])
        status, stdout, _ = self.cli("--hyp", "hello world", "--ref", "Hello, World!", "--json")
        self.assertEqual(status, 0)
        self.assertEqual(json.loads(stdout)["wer"]["wer"], 0)
        self.assertEqual(self.cli("--self-test")[0], 0)
        refs = {"en.wav": "Hello, WORLD!"}
        legacy = score_asr.score_bench(self.bench, refs, self.options())
        self.assertEqual(legacy["overall"]["wer"], 0)
        self.assertEqual(legacy["rows"][0]["n_samples"], 3)
        self.assertEqual(score_asr.compare_baseline(legacy, legacy)["overall"], {"cer": 0, "wer": 0})


if __name__ == "__main__":
    unittest.main()
