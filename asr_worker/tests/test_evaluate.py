"""Check evaluation scheduling and evidence handling without a model or VM."""
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
try:
    import evaluate
finally:
    sys.path.pop(0)


class EvaluationTests(unittest.TestCase):
    def test_local_settings_supply_paths_and_cli_can_override_them(self):
        with tempfile.TemporaryDirectory() as scratch:
            settings = Path(scratch) / "settings.json"
            evaluate.new_json(settings, {"schema": "asr-evaluation-settings/1",
                                         "app_binary": "staged.exe", "credential": "guest.xml"})
            args = evaluate.parse_args(["--profile", "vm", "--settings", str(settings),
                                        "--app-binary", "override.exe"])
            self.assertEqual(args.app_binary.name, "override.exe")
            self.assertEqual(args.credential, evaluate.ROOT / "guest.xml")
            self.assertIsNone(args.output)

    def test_resume_reuses_only_matching_pass_with_evidence(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            directory = root / "cuda-smoke" / "one"
            directory.mkdir(parents=True)
            cer = {"cer": .1}
            bench = {"warm": {"samples": [{}], "wall_ms": {}}}
            score = {"overall": {"files": 1, "raw": {"cer": cer}, "normalized": {"cer": cer}}}
            evaluate.new_json(directory / "bench.json", bench)
            evaluate.new_json(directory / "score.json", score)
            (directory / "score.csv").write_text("score", encoding="utf-8")
            record = {"status": "PASS", "inputs": {"manifest_sha256": "original"},
                      "metrics": evaluate.cuda_metrics(bench, score),
                      "evidence_sha256": evaluate.evidence_hashes("cuda-smoke", directory)}
            evaluate.new_json(directory / "stage.json", record)
            match = evaluate.matching_pass(root, "cuda-smoke", record["inputs"], "smoke")
            self.assertEqual(match["metrics"], record["metrics"])
            self.assertIsNone(evaluate.matching_pass(root, "cuda-smoke", {"manifest_sha256": "changed"}, "smoke"))
            stage_path = directory / "stage.json"
            saved = stage_path.read_text(encoding="utf-8")
            stage_path.write_text(saved.replace('"utterances": 1', '"utterances": 2'), encoding="utf-8")
            self.assertIsNone(evaluate.matching_pass(root, "cuda-smoke", record["inputs"], "smoke"))
            stage_path.write_text(saved, encoding="utf-8")
            (directory / "score.json").write_text("changed", encoding="utf-8")
            self.assertIsNone(evaluate.matching_pass(root, "cuda-smoke", record["inputs"], "smoke"))

    def test_vm_interrupt_requests_cleanup_and_waits_for_controller(self):
        with tempfile.TemporaryDirectory() as scratch:
            directory = Path(scratch)

            def wait(timeout):
                if not (directory / "cancel.requested").exists():
                    signal.getsignal(signal.SIGINT)(signal.SIGINT, None)
                    raise subprocess.TimeoutExpired("test", timeout)
                vm = directory / "vm"
                vm.mkdir()
                evaluate.new_json(vm / "host.json", {"status": "BLOCKED"})
                return 2

            with patch.object(subprocess, "CREATE_NO_WINDOW", 512, create=True), patch.object(
                subprocess, "Popen", return_value=SimpleNamespace(pid=42, wait=wait)
            ), self.assertRaises(KeyboardInterrupt):
                evaluate.invoke_vm(directory, ["unused"], timeout=10)
            self.assertTrue((directory / "vm" / "host.json").is_file())

    def test_unconfirmed_cleanup_prevents_next_vm_stage(self):
        args = SimpleNamespace(profile="vm", suite="smoke", vm_mode="both", resume=False)
        with patch.object(evaluate, "inputs_for", return_value={}), patch.object(
            evaluate, "execute", return_value={"status": "BLOCKED", "vm_cleanup_unconfirmed": True}
        ) as execute:
            stages, status = evaluate.run_plan(args, Path("unused"))
        self.assertEqual(status, "BLOCKED")
        self.assertEqual(execute.call_count, 1)
        self.assertEqual(stages["vm-real"]["status"], "NOT_RUN")

    def test_vm_stages_cannot_use_different_application_binaries(self):
        args = SimpleNamespace(profile="vm", suite="smoke", vm_mode="both", resume=False)
        with patch.object(evaluate, "inputs_for", side_effect=[
            {"app_sha256": "mock-binary"}, {"app_sha256": "different-binary"},
        ]), patch.object(evaluate, "execute", return_value={"status": "PASS"}) as execute:
            stages, status = evaluate.run_plan(args, Path("unused"))
        self.assertEqual(status, "BLOCKED")
        self.assertEqual(execute.call_count, 1)
        self.assertEqual(stages["vm-real"]["reason"], "application binary changed between VM stages")

    def test_vm_evidence_must_bind_the_binary_and_desktop_session(self):
        args = SimpleNamespace(suite="smoke", vm_fixture="original", app_binary="app.exe",
                               app_revision="revision", credential="credential.xml", manifest_dir="manifests")
        inputs = {"vm_id": "vm", "app_sha256": "expected", "mock_on_real_profile": False}
        with tempfile.TemporaryDirectory() as scratch, patch.object(evaluate, "invoke_vm", return_value=0):
            directory = Path(scratch)
            vm = directory / "vm"
            vm.mkdir()
            evaluate.new_json(vm / "host.json", {"status": "PASS", "vm_id": "vm"})
            evidence = vm / "result.json"
            evaluate.new_json(evidence, {"exe_sha256": "different", "status": "PASS"})
            with self.assertRaisesRegex(evaluate.Blocked, "binary that differs from preflight"):
                evaluate.run_vm(args, "vm-mock", directory, inputs)
            evidence.write_text('{"requires_session_reset":true,"reason":"acceptance timed out"}', encoding="utf-8")
            with self.assertRaisesRegex(evaluate.CleanupUnconfirmed, "acceptance timed out"):
                evaluate.run_vm(args, "vm-mock", directory, inputs)

    def test_vm_interrupt_at_controller_exit_still_stops_suite(self):
        with tempfile.TemporaryDirectory() as scratch:
            directory = Path(scratch)
            vm = directory / "vm"
            vm.mkdir()
            evaluate.new_json(vm / "host.json", {"status": "PASS"})

            def wait(timeout):
                signal.getsignal(signal.SIGINT)(signal.SIGINT, None)
                return 0

            with patch.object(subprocess, "CREATE_NO_WINDOW", 512, create=True), patch.object(
                subprocess, "Popen", return_value=SimpleNamespace(pid=42, wait=wait)
            ), self.assertRaises(KeyboardInterrupt):
                evaluate.invoke_vm(directory, ["unused"], timeout=10)

    def test_mock_recognition_rejects_wrong_text_from_successful_process(self):
        with tempfile.TemporaryDirectory() as scratch:
            directory = Path(scratch)
            evaluate.new_json(directory / "data.json", {"warm": {"samples": [{"text": "wrong"}]}})
            with patch.object(evaluate, "invoke", return_value=0):
                status, _, _ = evaluate.run_headless("mock-recognition", directory)
            self.assertEqual(status, "FAIL")

    def test_independent_steps_continue_but_interrupt_defers_later_steps(self):
        args = SimpleNamespace(profile="headless", suite="smoke", vm_mode="both", resume=False)
        responses = [{"status": state} for state in ("FAIL", "INTERRUPTED")]
        with patch.object(evaluate, "execute", side_effect=responses) as execute:
            stages, status = evaluate.run_plan(args, Path("unused"))
        self.assertEqual(status, "INTERRUPTED")
        self.assertEqual(execute.call_count, 2)
        self.assertEqual(stages["correction-fixtures"]["status"], "NOT_RUN")
        self.assertEqual(stages["cuda-smoke"]["status"], "NOT_SELECTED")

    def test_guard_pass_is_not_a_confirmed_insertion(self):
        guest = {"cases": [
            {"scenario": "mock-insertion", "status": "PASS"},
            {"scenario": "focus-away", "status": "PASS"},
            {"scenario": "cancel", "status": "PASS"},
            {"scenario": "mock-notepad", "status": "BLOCKED"},
        ]}
        metrics = evaluate.vm_metrics(guest, requested=2)
        self.assertEqual(metrics["insertion"]["planned"], 2)
        self.assertEqual(metrics["insertion"]["confirmed"], 1)
        self.assertNotIn("warm_stop_median_ms", metrics["insertion"])
        self.assertEqual(metrics["guard_outcomes"], {"focus-away": "PASS", "cancel": "PASS"})

    def test_short_baseline_excludes_first_take_and_keeps_denominator(self):
        def take(latency):
            return {"status": "PASS", "acceptance": {"results": [{
                "stop_to_observed_insertion_ms": latency, "audio_duration_ms": 3191.4,
            }]}}

        trials = [take(999), *(take(ms) for ms in range(11, 20))]
        metrics = evaluate.vm_metrics({"cases": [{
            "scenario": "real-notepad", "trials": trials,
        }]}, requested=evaluate.planned_takes("vm-real", "full", "short"))["insertion"]
        self.assertEqual(metrics["planned"], 10)
        self.assertEqual(metrics["confirmed"], 10)
        self.assertEqual(metrics["warm_stop_samples"], 9)
        self.assertEqual(metrics["warm_stop_planned"], 9)
        self.assertEqual(metrics["warm_stop_median_ms"], 15)
        self.assertAlmostEqual(metrics["warm_stop_p95_ms"], 18.6)
        self.assertEqual(evaluate.planned_takes("vm-real", "full"), 6)
        partial = evaluate.vm_metrics({"cases": [{
            "scenario": "real-notepad", "trials": trials[:2],
        }]}, requested=10)["insertion"]
        self.assertEqual(partial["warm_stop_planned"], 9)
        self.assertEqual(partial["warm_stop_samples"], 1)
        self.assertEqual(partial["warm_stop_median_ms"], 11)
        self.assertIsNone(partial["warm_stop_p95_ms"])

    def test_full_matrix_keeps_guards_out_of_insertion_denominator(self):
        cases = [{"scenario": name, "status": "PASS"} for name in (
            "mock-insertion", "mock-notepad", "mock-edge", "mock-vscode", "mock-chrome", "mock-terminal",
            *evaluate.FULL_GUARDS,
        )]
        cases[2].update(status="BLOCKED", reason="owned_fixture_target_unproven")
        metrics = evaluate.vm_metrics({"cases": cases}, evaluate.planned_takes("vm-mock", "full"))
        self.assertEqual(metrics["insertion"]["planned"], 6)
        self.assertEqual(metrics["insertion"]["confirmed"], 5)
        self.assertEqual(metrics["guard_outcomes"], dict.fromkeys(evaluate.FULL_GUARDS, "PASS"))
        self.assertEqual(evaluate.planned_takes("vm-real", "full", override=12), 12)

    def test_external_real_takes_keep_missing_target_and_notepad_warm_denominators(self):
        def passed(latency):
            return {"status": "PASS", "acceptance": {"results": [{"stop_to_observed_insertion_ms": latency}]}}

        metrics = evaluate.vm_metrics({"cases": [
            {"scenario": "real-notepad", "trials": [passed(400), passed(80)]},
            {"scenario": "real-vscode", "trials": [passed(600), passed(120)]},
        ]}, requested=2, external=True)
        self.assertEqual(metrics["insertion"]["planned"], 6)
        self.assertEqual(metrics["insertion"]["confirmed"], 4)
        self.assertEqual(metrics["insertion"]["by_target"]["real-chrome"], {"planned": 2, "confirmed": 0})
        self.assertEqual(metrics["insertion"]["warm_stop_samples"], 1)
        self.assertEqual(metrics["insertion"]["warm_stop_median_ms"], 80)

    def test_real_failure_without_a_complete_cer_record_keeps_its_status(self):
        metrics = evaluate.vm_metrics({"cases": [{
            "scenario": "real-chrome", "status": "FAIL", "reason": "app_outcome_mismatch",
            "real_worker_proof": {"cer_raw": {"cer": 0.1}},
        }]}, requested=2, external=True)
        self.assertEqual(metrics["recognition"], [])
        self.assertEqual(metrics["insertion"]["trials"][0]["reason"], "app_outcome_mismatch")

    def test_markdown_displays_manifest_cer_and_insertion_separately(self):
        cer = {"cer": .1, "errors": 1, "ref_len": 10}
        report = {"status": "PASS", "stages": {
            "cuda-smoke": {"status": "PASS", "metrics": {"recognition": {
                "utterances": 1, "raw_cer": cer, "normalized_cer": cer,
            }}},
            "vm-real": {"status": "PASS", "directory": "vm-real/attempt", "metrics": evaluate.vm_metrics({"cases": [
                {"scenario": "real-notepad", "status": "PASS",
                 "real_worker_proof": {"cer_raw": cer, "cer_normalized": cer}},
            ]}, requested=1)},
            "cuda-holdout": {"status": "NOT_SELECTED"},
        }}
        rendered = evaluate.markdown(report)
        self.assertIn("raw CER 10.00%", rendered)
        self.assertIn("confirmed insertion 1/1", rendered)
        self.assertIn("trial median raw/normalized CER 10.00%/10.00% (n=1)", rendered)
        self.assertIn("2/2 stages passed", rendered)
        self.assertNotIn("cuda-holdout", rendered)
