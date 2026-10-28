from __future__ import annotations

import argparse
import importlib.util
import os
import sys
import unittest
import tempfile

from asr_worker.tests.support import IsolatedWorkerTestCase
from pathlib import Path
from unittest.mock import patch

_SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "bench_asr.py"


def _load_bench_module():  # noqa: ANN202
    spec = importlib.util.spec_from_file_location("bench_asr", _SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


bench_asr = _load_bench_module()

# A worker that writes far more to stderr than a pipe buffer holds (like
# transformers warnings or tqdm bars) before it answers each request.
_NOISY_WORKER = r"""
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    for index in range(4000):
        sys.stderr.write(f"warning {index}: " + "x" * 200 + "\n")
    sys.stderr.write("last diagnostic line\n")
    sys.stderr.flush()
    print(json.dumps({"id": request["id"], "ok": True}), flush=True)
    if request.get("command") == "shutdown":
        break
"""

# A worker whose stderr is in the Windows console code page, as a piped Python
# stderr is unless UTF-8 is requested: a Japanese path in a warning or
# traceback is not valid UTF-8.
_CP932_WORKER = r"""
import json, sys
sys.stderr.buffer.write("C:/ユーザー/モデル: warning\n".encode("cp932"))
sys.stderr.buffer.flush()
for line in sys.stdin:
    request = json.loads(line)
    for index in range(4000):
        sys.stderr.write(f"warning {index}: " + "x" * 200 + "\n")
    sys.stderr.write("last diagnostic line\n")
    sys.stderr.flush()
    print(json.dumps({"id": request["id"], "ok": True}), flush=True)
    if request.get("command") == "shutdown":
        break
"""

_FAILING_WORKER = r"""
import sys
for index in range(4000):
    sys.stderr.write(f"warning {index}: " + "x" * 200 + "\n")
sys.stderr.write("fatal: model missing\n")
sys.stderr.flush()
sys.exit(3)
"""


class WorkerStderrTests(IsolatedWorkerTestCase):
    def test_worker_flooding_stderr_does_not_block_requests(self) -> None:
        with bench_asr.WorkerSession([sys.executable, "-c", _NOISY_WORKER], timeout_s=30) as session:
            response, _ = session.request({"command": "load"})
            self.assertTrue(response["ok"])
            response, _ = session.request({"command": "transcribe"})
            self.assertTrue(response["ok"])
            tail = session._stderr_tail()

        # The tail is bounded but keeps the most recent diagnostics.
        self.assertIn("last diagnostic line", tail)
        self.assertNotIn("warning 0:", tail)
        self.assertLess(len(tail.splitlines()), 1000)

    def test_failure_report_includes_the_stderr_tail(self) -> None:
        with bench_asr.WorkerSession([sys.executable, "-c", _FAILING_WORKER], timeout_s=30) as session:
            with self.assertRaises(bench_asr.BenchError) as raised:
                session.request({"command": "load"})

        self.assertIn("fatal: model missing", str(raised.exception))

    def test_stderr_in_another_encoding_keeps_being_drained(self) -> None:
        with bench_asr.WorkerSession([sys.executable, "-c", _CP932_WORKER], timeout_s=30) as session:
            for command in ("load", "transcribe", "transcribe"):
                response, _ = session.request({"command": command})
                self.assertTrue(response["ok"])
            tail = session._stderr_tail()

        self.assertIn("last diagnostic line", tail)


class WorkerEnvTests(IsolatedWorkerTestCase):
    def test_bad_output_is_rejected_before_any_benchmark_work(self):
        with tempfile.TemporaryDirectory() as tmp:
            invalid = Path(tmp) / "missing" / "result.json"
            with patch.object(bench_asr, "run_benchmark") as run:
                self.assertEqual(bench_asr.main(["--smoke", "--output", str(invalid)]), 1)
                run.assert_not_called()

    def test_destination_is_exclusively_reserved_before_benchmark_starts(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "result.json"
            def run(args, paths):
                self.assertTrue(output.exists())
                self.assertEqual(output.read_bytes(), b"")
                return {"sample": "日本語"}
            with patch.object(bench_asr, "run_benchmark", side_effect=run):
                self.assertEqual(bench_asr.main(["--smoke", "--json", "--output", str(output)]), 0)
            original = output.read_bytes()
            self.assertIn("日本語".encode("utf-8"), original)
            with patch.object(bench_asr, "run_benchmark") as run:
                self.assertEqual(bench_asr.main(["--smoke", "--output", str(output)]), 1)
                run.assert_not_called()
            self.assertEqual(output.read_bytes(), original)

    def test_health_records_actual_device_after_each_inference_without_timing_health(self):
        class Session:
            def __init__(self): self.commands = []
            def request(self, request):
                self.commands.append(request["command"])
                if request["command"] == "health":
                    return {"health": {"device": "cpu", "computeType": "int8"}}, 1000.0
                return {"text": "sample", "duration_ms": 10.0}, 20.0
        session = Session()
        records = bench_asr.run_transcribe_loop(session, [Path("audio.wav")], 2, None, True)
        self.assertEqual(session.commands, ["transcribe", "health", "transcribe", "health"])
        self.assertEqual([record["wall_ms"] for record in records], [20.0, 20.0])
        self.assertTrue(all(record["device"] == "cpu" and record["compute_type"] == "int8" for record in records))

    def test_output_is_utf8_and_existing_results_are_not_overwritten(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "result.json"
            args = argparse.Namespace(json=True, output=str(output))
            bench_asr._emit({"sample": "日本語"}, args)
            original = output.read_bytes()
            self.assertIn("日本語".encode("utf-8"), original)
            with self.assertRaises(bench_asr.BenchError):
                bench_asr._emit({"sample": "replacement"}, args)
            self.assertEqual(output.read_bytes(), original)

    def test_whisper_profile_is_explicit_and_cannot_be_claimed_for_other_backends(self):
        with patch.dict(os.environ, {"ASR_WHISPER_DECODE_PROFILE": "fast"}):
            standard = bench_asr.worker_env(argparse.Namespace(backend="faster-whisper", whisper_profile="standard"))
            fast = bench_asr.worker_env(argparse.Namespace(backend="faster-whisper", whisper_profile="fast"))
        self.assertEqual(standard["ASR_WHISPER_DECODE_PROFILE"], "standard")
        self.assertEqual(fast["ASR_WHISPER_DECODE_PROFILE"], "fast")
        with self.assertRaises(bench_asr.BenchError):
            bench_asr.worker_env(argparse.Namespace(backend="mock", whisper_profile="fast"))

    def test_worker_stderr_is_requested_as_utf8(self) -> None:
        with patch.dict(os.environ, {}, clear=False):
            os.environ.pop("PYTHONIOENCODING", None)
            env = bench_asr.worker_env(argparse.Namespace(backend="mock"))

        self.assertEqual(env["PYTHONIOENCODING"], "utf-8")
        self.assertEqual(env["ASR_WORKER_BACKEND"], "mock")

    def test_explicit_io_encoding_is_kept(self) -> None:
        with patch.dict(os.environ, {"PYTHONIOENCODING": "cp932"}):
            env = bench_asr.worker_env(argparse.Namespace(backend="mock"))

        self.assertEqual(env["PYTHONIOENCODING"], "cp932")


if __name__ == "__main__":
    unittest.main()
