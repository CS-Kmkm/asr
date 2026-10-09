from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

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


if __name__ == "__main__":
    unittest.main()
