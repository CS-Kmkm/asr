from __future__ import annotations

import argparse
import importlib.util
import os
import sys
import unittest
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


class WorkerStderrTests(unittest.TestCase):
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


class WorkerEnvTests(unittest.TestCase):
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
