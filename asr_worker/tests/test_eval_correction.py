"""Regression tests for the offline correction evaluation (Issue #85)."""
from __future__ import annotations

import copy
import csv
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

_SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "eval_correction.py"
_SPEC = importlib.util.spec_from_file_location("eval_correction_script", _SCRIPT)
assert _SPEC is not None and _SPEC.loader is not None
evaluation = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = evaluation
_SPEC.loader.exec_module(evaluation)


def _case(**overrides):
    case = {
        "id": "synthetic-01",
        "language": "ja",
        "categories": ["negation"],
        "correction_mode": "conservative",
        "transcript": "えーと、明日は10時に送らないでください。",
        "reference": "明日は10時に送らないでください。",
        "allowed_changes": ["Remove the filler"],
        "forbidden_changes": ["Flip the negation"],
        "rules": {
            "require": [
                {"kind": "number", "any": ["10時"]},
                {"kind": "negation", "any": ["送らないで"]},
            ],
            "forbid": [{"kind": "filler", "any": ["えーと"]}],
        },
        "fixtures": [{"id": "flipped", "output": "明日は10時に送ってください。", "expect_failures": ["dropped:negation"], "guard": "reject"}],
    }
    case.update(overrides)
    return case


def _dataset(*cases):
    return {
        "schema": "correction-eval/1",
        "name": "synthetic",
        "provenance": "test",
        "license": "test",
        "cases": list(cases) or [_case()],
    }


class DatasetSchemaTests(unittest.TestCase):
    def test_bundled_dataset_is_valid_and_covers_every_required_category(self):
        dataset = evaluation.load_dataset(evaluation.DEFAULT_DATASET)
        categories = {category for case in dataset["cases"] for category in case["categories"]}
        self.assertTrue({
            "self_correction", "slip", "technical_terms", "negation", "numbers_units",
            "url_code", "long_memo", "ambiguous_instruction",
        } <= categories)
        languages = [case["language"] for case in dataset["cases"]]
        self.assertGreater(languages.count("ja"), languages.count("en") + languages.count("mixed"))
        self.assertIn("en", languages)
        self.assertIn("mixed", languages)
        self.assertEqual({"conservative", "intent_aware"}, {case["correction_mode"] for case in dataset["cases"]})
        for case in dataset["cases"]:
            self.assertTrue(case["allowed_changes"] and case["forbidden_changes"], case["id"])

    def test_bundled_fixtures_match_their_expected_verdicts(self):
        dataset = evaluation.load_dataset(evaluation.DEFAULT_DATASET)
        rows, mismatches = evaluation.check_fixtures(dataset)
        self.assertEqual([], mismatches)
        # Every case has at least one fixture that the rules reject.
        failing = {row["case"] for row in rows if row["output_id"] != "reference" and not row["score"]["pass"]}
        self.assertEqual({case["id"] for case in dataset["cases"]}, failing)

    def test_schema_violations_are_all_reported(self):
        broken = _case(id="Bad Id", language="fr", categories=[], correction_mode="creative")
        broken["rules"] = {"require": [{"kind": "vibes", "any": []}], "unexpected": 1}
        broken["fixtures"] = [{"id": "reference", "output": 3, "expect_failures": ["dropped:vibes"], "guard": "maybe"}]
        with self.assertRaises(evaluation.DatasetError) as raised:
            evaluation.validate_dataset(_dataset(broken))
        message = str(raised.exception)
        for fragment in (".id", ".language", ".categories", ".correction_mode", "unknown keys", ".kind", ".any", "fixtures[0].id", ".output", ".guard", "expect_failures"):
            self.assertIn(fragment, message)

    def test_duplicate_ids_and_missing_rules_are_rejected(self):
        with self.assertRaisesRegex(evaluation.DatasetError, "duplicated"):
            evaluation.validate_dataset(_dataset(_case(), _case()))
        without_rules = _case()
        without_rules["rules"] = {"require": [], "forbid": []}
        with self.assertRaisesRegex(evaluation.DatasetError, "at least one require or forbid"):
            evaluation.validate_dataset(_dataset(without_rules))
        with self.assertRaisesRegex(evaluation.DatasetError, "schema"):
            evaluation.validate_dataset({**_dataset(), "schema": "other/1"})


class ScorerTests(unittest.TestCase):
    def test_reference_passes_and_metrics_are_reported(self):
        score = evaluation.score_output(_case(), "明日は10時に送らないでください。")
        self.assertTrue(score["pass"])
        metrics = score["metrics"]
        self.assertFalse(metrics["information_dropped"])
        self.assertFalse(metrics["information_added"])
        self.assertTrue(metrics["negation_preserved"])
        self.assertTrue(metrics["names_numbers_match"])
        self.assertIsNone(metrics["self_correction_resolved"])
        self.assertTrue(metrics["fillers_removed"])
        self.assertGreater(metrics["edit_ratio"], 0)

    def test_negation_flip_number_change_and_filler_are_separate_failures(self):
        score = evaluation.score_output(_case(), "えーと、明日は11時に送ってください。")
        self.assertEqual(["added:number", "dropped:negation", "dropped:number", "forbidden:filler"], score["failures"])
        self.assertFalse(score["metrics"]["negation_preserved"])
        self.assertFalse(score["metrics"]["names_numbers_match"])
        self.assertFalse(score["metrics"]["fillers_removed"])

    def test_normalization_accepts_width_case_and_thousands_separators(self):
        case = _case(transcript="予算は3000円、担当は ＧｉｔＨｕｂ です。", reference="予算は3,000円、担当はGitHubです。")
        case["rules"] = {"require": [{"kind": "number", "any": ["3000円", "3,000円"]}, {"kind": "name", "any": ["GitHub"]}]}
        self.assertTrue(evaluation.score_output(case, "予算は3,000円、担当はgithubです。")["pass"])
        self.assertEqual(["added:number", "dropped:number"], evaluation.score_output(case, "予算は3,500円、担当はGitHubです。")["failures"])

    def test_added_names_ignore_english_sentence_starts_but_not_new_names(self):
        case = _case(language="en", transcript="please send the report to the team", reference="Please send the report to the team.")
        case["rules"] = {"require": [{"kind": "proposition", "any": ["report"]}]}
        self.assertTrue(evaluation.score_output(case, "Please send the report to the team.")["pass"])
        self.assertEqual(["added:name"], evaluation.score_output(case, "Please send the report to the team on Monday.")["failures"])
        self.assertIn("added:name", evaluation.score_output(case, "Slackでreportを送る")["failures"])

    def test_urls_and_code_are_compared_exactly(self):
        case = _case(transcript="Open https://example.com/a and run `make test`.", reference="Open https://example.com/a and run `make test`.")
        case["rules"] = {"require": [{"kind": "url", "any": ["https://example.com/a"]}, {"kind": "code", "any": ["`make test`"]}]}
        self.assertTrue(evaluation.score_output(case, case["reference"])["pass"])
        self.assertEqual(
            ["added:code", "added:url", "dropped:code", "dropped:url"],
            evaluation.score_output(case, "Open https://example.com/b and run `make build`.")["failures"],
        )

    def test_code_is_case_sensitive_and_urls_only_fold_scheme_and_host(self):
        case = _case(language="en", transcript="Run `cargo test` and open https://Example.com/Docs/Setup now.",
                     reference="Run `cargo test` and open https://Example.com/Docs/Setup now.")
        case["rules"] = {"require": [{"kind": "code", "any": ["`cargo test`"]}, {"kind": "url", "any": ["https://Example.com/Docs/Setup"]}]}
        self.assertTrue(evaluation.score_output(case, "Run `cargo test` and open HTTPS://example.COM/Docs/Setup now.")["pass"])
        self.assertEqual(
            ["added:code", "dropped:code"],
            evaluation.score_output(case, "Run `cargo TEST` and open https://Example.com/Docs/Setup now.")["failures"],
        )
        self.assertEqual(
            ["added:url", "dropped:url"],
            evaluation.score_output(case, "Run `cargo test` and open https://Example.com/docs/Setup now.")["failures"],
        )
        # allow_new URLs normalize exactly like output URLs.
        allowed = _case(language="en", transcript="Read the guide.", reference="Read the guide.")
        allowed["rules"] = {"require": [{"kind": "proposition", "any": ["guide"]}], "allow_new": ["https://Example.com/Docs"]}
        self.assertEqual([], [code for code in evaluation.score_output(allowed, "Read the guide at https://example.com/Docs.")["failures"] if code.startswith("added")])
        self.assertIn("added:url", evaluation.score_output(allowed, "Read the guide at https://example.com/docs.")["failures"])
        # Prose stays case-insensitive.
        self.assertTrue(evaluation.contains("Please SEND it", "please send"))

    def test_edge_whitespace_patterns_match_only_as_words(self):
        self.assertTrue(evaluation.contains("um so please", "um "))
        self.assertFalse(evaluation.contains("summary first", "um "))
        self.assertTrue(evaluation.contains("React のコンポーネント", "Reactのコンポーネント"))

    def test_edit_ratio_limit_depends_on_mode_and_override(self):
        case = _case()
        rewrite = "全く別の内容を10時に送らないでください。追加の説明がたくさん続きます。"
        self.assertIn("edit_ratio", evaluation.score_output(case, rewrite)["failures"])
        case["rules"]["max_edit_ratio"] = 5
        self.assertNotIn("edit_ratio", evaluation.score_output(case, rewrite)["failures"])
        self.assertEqual(0, evaluation.edit_ratio("同じ。", "同じ"))

    def test_judge_fields_stay_unscored_unless_supplied(self):
        dataset = _dataset()
        rows = evaluation.score_outputs(dataset, [
            {"id": "synthetic-01", "output": "明日は10時に送らないでください。"},
            {"id": "synthetic-01", "output": "明日は10時に送らないでください。", "judge": {"readability": 4, "tone": 5, "source": "human"}},
        ])
        self.assertEqual({"readability": None, "tone": None, "source": None, "status": "not_scored"}, rows[0]["judge"])
        self.assertEqual({"readability": 4, "tone": 5, "source": "human", "status": "supplied"}, rows[1]["judge"])
        with self.assertRaises(evaluation.DatasetError):
            evaluation.score_outputs(dataset, [{"id": "missing", "output": ""}])

    def test_guard_summary_separates_catches_from_false_rejections(self):
        dataset = _dataset()
        rows = evaluation.score_outputs(dataset, [
            {"id": "synthetic-01", "output": "えーと、明日は10時に送らないでください。", "candidate": "明日は10時に送ってください。", "accepted": False},
            {"id": "synthetic-01", "output": "えーと、明日は10時に送らないでください。", "candidate": "明日は10時に送らないでください。", "accepted": False},
            {"id": "synthetic-01", "output": "明日は11時に送らないでください。", "candidate": "明日は11時に送らないでください。", "accepted": True},
        ])
        self.assertEqual(
            {"candidates": 3, "rejected": 2, "caught": 1, "false_rejections": 1, "accepted_failing": 1},
            evaluation.guard_summary(rows),
        )


class RunnerTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def test_fake_providers_run_without_any_model(self):
        for provider in ("identity", "reference", "fixtures"):
            out = self.root / f"{provider}.json"
            self.assertEqual(0, evaluation.main(["run", "--provider", provider, "--out", str(out)]), provider)
            report = json.loads(out.read_text(encoding="utf-8"))
            self.assertEqual(provider, report["provider"])
            self.assertIn("not scored", report["automatic_metrics"])
        reference = json.loads((self.root / "reference.json").read_text(encoding="utf-8"))
        overall = reference["summary"]["overall"]
        self.assertEqual(overall["outputs"], overall["passed"])

    def test_fixture_mismatch_fails_the_run(self):
        dataset = copy.deepcopy(evaluation.load_dataset(evaluation.DEFAULT_DATASET))
        dataset["cases"][0]["fixtures"][0]["expect_failures"] = []
        path = self.root / "dataset.json"
        path.write_text(json.dumps(dataset, ensure_ascii=False), encoding="utf-8")
        self.assertEqual(1, evaluation.main(["--dataset", str(path), "run", "--provider", "fixtures"]))

    def test_score_command_reads_outputs_jsonl(self):
        outputs = self.root / "outputs.jsonl"
        outputs.write_text(
            json.dumps({"id": "ja-negation-request-01", "output": "このファイルはまだ送ってください。確認が終わってから送ります。"}, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )
        out = self.root / "report.json"
        self.assertEqual(0, evaluation.main(["score", "--outputs", str(outputs), "--out", str(out)]))
        row = json.loads(out.read_text(encoding="utf-8"))["rows"][0]
        self.assertFalse(row["score"]["metrics"]["negation_preserved"])

    def test_blind_sheet_hides_system_names_and_writes_a_separate_key(self):
        a = self.root / "a.jsonl"
        b = self.root / "b.jsonl"
        a.write_text(json.dumps({"id": "ja-url-01", "output": "system-a text"}) + "\n", encoding="utf-8")
        b.write_text(json.dumps({"id": "ja-url-01", "output": "system-b text"}) + "\n", encoding="utf-8")
        sheet, key = self.root / "sheet.csv", self.root / "key.json"
        self.assertEqual(0, evaluation.main(["blind", "--system", f"app={a}", "--system", f"other={b}", "--sheet", str(sheet), "--key", str(key)]))
        with sheet.open(encoding="utf-8-sig") as handle:
            rows = list(csv.DictReader(handle))
        self.assertEqual({"A", "B"}, {row["label"] for row in rows})
        self.assertNotIn("app", sheet.read_text(encoding="utf-8-sig"))
        self.assertEqual({"app", "other"}, set(json.loads(key.read_text(encoding="utf-8"))["ja-url-01"].values()))

    def test_invalid_dataset_exits_with_usage_error(self):
        path = self.root / "bad.json"
        path.write_text(json.dumps({"schema": "nope"}), encoding="utf-8")
        self.assertEqual(2, evaluation.main(["--dataset", str(path), "validate"]))


if __name__ == "__main__":
    unittest.main()
