from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import math
import os
import socket
import subprocess
import sys
import tempfile
import unittest
import wave
from pathlib import Path
from unittest.mock import MagicMock, Mock, patch


_SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "bench_whisper_server.py"


def _load_bench_module():  # noqa: ANN202
    spec = importlib.util.spec_from_file_location("bench_whisper_server", _SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


bench = _load_bench_module()

_SERVER = r'''
import argparse
import io
import json
import os
import sys
import time
import wave
from email.parser import BytesParser
from email.policy import default
from http.server import BaseHTTPRequestHandler, HTTPServer

parser = argparse.ArgumentParser()
parser.add_argument("--events", required=True)
parser.add_argument("--mode", default="ok")
parser.add_argument("--model", required=True)
parser.add_argument("--host", required=True)
parser.add_argument("--port", type=int, required=True)
parser.add_argument("--threads", type=int)
parser.add_argument("--language", required=True)
parser.add_argument("--max-len", required=True)
args = parser.parse_args()
assert args.host == "127.0.0.1" and not 8178 <= args.port <= 8199
assert os.path.isabs(args.model)
assert args.max_len == "4096" and args.language == "ja"
assert os.environ["CUDA_DEVICE_ORDER"] == "PCI_BUS_ID"

def record(event):
    with open(args.events, "a", encoding="utf-8") as stream:
        stream.write(json.dumps(event) + "\n")

record({"argv": sys.argv[1:], "pid": os.getpid(), "port": args.port})
print("server diagnostics on stdout", flush=True)
print("server diagnostics on stderr", file=sys.stderr, flush=True)
if args.mode == "exit":
    sys.exit(7)

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        assert self.path == "/inference"
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        message = BytesParser(policy=default).parsebytes(
            ("Content-Type: " + self.headers["Content-Type"] + "\r\n\r\n").encode() + raw)
        fields = {}
        audio = None
        for part in message.iter_parts():
            name = part.get_param("name", header="content-disposition")
            assert name not in fields
            if name == "file":
                assert part.get_content_type() == "audio/wav"
                audio = part.get_payload(decode=True)
            else:
                fields[name] = part.get_payload(decode=True).decode("utf-8")
        assert fields["language"] == "ja"
        assert fields["entropy_thold"] == "2.8"
        assert fields["logprob_thold"] == "-1.25"
        assert fields["response_format"] == "json"
        assert set(fields) <= {"language", "entropy_thold", "logprob_thold", "response_format", "beam_size"}
        assert audio is not None
        with wave.open(io.BytesIO(audio), "rb") as wav:
            assert (wav.getframerate(), wav.getnchannels(), wav.getsampwidth()) == (16000, 1, 2)
            frames = wav.readframes(wav.getnframes())
        record({"fields": fields, "frames": frames.hex()})
        if args.mode == "http-error":
            self.send_error(500, "synthetic error")
            return
        self.send_response(201 if args.mode == "created" else 302 if args.mode == "redirect" else 200)
        if args.mode == "redirect":
            self.send_header("Location", "http://must-never-be-contacted.invalid/")
        self.end_headers()
        response = {"text": "  日本語  "}
        if args.mode == "bad-text":
            response["text"] = None
        elif args.mode == "not-object":
            response = []
        self.wfile.write(b"not JSON" if args.mode == "malformed" else json.dumps(response).encode())

server = HTTPServer((args.host, args.port), Handler)
if args.mode == "cpu":
    print("whisper_backend_init_gpu: no GPU found", file=sys.stderr, flush=True)
    print("whisper_model_load: CPU total size = 100 MB", file=sys.stderr, flush=True)
elif args.mode != "unknown-device":
    print("whisper_backend_init_gpu: found GPU device 0: CUDA0", file=sys.stderr, flush=True)
    print("whisper_model_load:        CUDA0 total size = 100 MB", file=sys.stderr, flush=True)
print(f"listening at http://127.0.0.1:{args.port}", file=sys.stderr, flush=True)
server.serve_forever()
'''


class WhisperServerBenchTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="test_whisper_server_")
        self.addCleanup(self.temp.cleanup)
        # Windows CI may expose TEMP through an 8.3 alias (RUNNER~1), while
        # the harness resolves model/executable paths to their long form.
        self.root = Path(self.temp.name).resolve()
        self.stub = self.root / "stub.py"
        self.stub.write_text(_SERVER, encoding="utf-8")
        self.model = self.root / "ggml-synthetic.bin"
        self.model.write_bytes(b"synthetic model; never loaded")
        self.manifest = self.root / "manifest.json"
        self.output = self.root / "result.json"
        self.events = self.root / "events.jsonl"
        items = []
        for index in range(2):
            audio = self.root / f"ja-{index}.wav"
            with wave.open(str(audio), "wb") as wav:
                wav.setnchannels(1)
                wav.setsampwidth(2)
                wav.setframerate(16000)
                wav.writeframes(bytes([index, 0]) * 160)
            items.append({"id": audio.stem, "audio_path": audio.name,
                          "reference": "日本語", "language": "ja-JP" if index == 0 else "ja",
                          "source": "synthetic"})
        self.manifest.write_text(json.dumps({"schema": "asr-bench-manifest/1", "name": "synthetic-ja",
                                             "sources": [{"source": "synthetic"}], "items": items}), encoding="utf-8")

    def argv(self, *extra: str, mode: str = "ok") -> list[str]:
        return ["--manifest", str(self.manifest), "--model", str(self.model),
                "--output", str(self.output), "--repeat", "2", "--threads", "6", "--timeout", "30",
                *extra, "--server-cmd", sys._base_executable, str(self.stub), "--events", str(self.events), "--mode", mode]

    def run_main(self, argv: list[str]) -> tuple[int, str, str]:
        stdout, stderr = io.StringIO(), io.StringIO()
        processes = []
        original = subprocess.Popen

        def start(*args, **kwargs):  # noqa: ANN002, ANN003, ANN202
            proc = original(*args, **kwargs)
            processes.append(proc)
            return proc

        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr), \
                patch.object(bench.subprocess, "Popen", side_effect=start), \
                patch.object(bench, "free_port", side_effect=self.spare_port):
            code = bench.main(argv)
        self.assertTrue(all(proc.poll() is not None for proc in processes), "server must always be reaped")
        return code, stdout.getvalue(), stderr.getvalue()

    def spare_port(self) -> int:
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        self.assertFalse(8178 <= port <= 8199)
        return port

    def records(self) -> list[dict]:
        return [json.loads(line) for line in self.events.read_text(encoding="utf-8").splitlines()]

    def test_launch_multipart_warmup_and_scorer_compatibility(self) -> None:
        code, stdout, stderr = self.run_main(self.argv("--model-sha256", "a" * 64))
        self.assertEqual(code, 0, stderr)
        self.assertEqual(stdout, "")
        result = json.loads(self.output.read_text(encoding="utf-8"))
        cfg, warm, cold = result["config"], result["warm"], result["cold"]
        self.assertEqual(cfg["backend"], "openwhispr-whisper-server")
        self.assertEqual(cfg["engine_version"], "openwhispr-whisper.cpp-0.0.11")
        self.assertEqual((cfg["model"], cfg["quantization"], cfg["device"]), (self.model.name, "f16", "cuda"))
        self.assertEqual((cfg["decode"], cfg["threads"], cfg["language_source"]), ("greedy-bestof2", 6, "manifest"))
        self.assertEqual(cfg["model_sha256"], "a" * 64)
        self.assertEqual(cfg["manifest"]["item_count"], 2)
        records = self.records()
        self.assertEqual(cfg["worker_cmd"][2:], records[0]["argv"])
        self.assertEqual(cfg["worker_cmd"][-12:], ["--model", str(self.model.resolve()), "--host", "127.0.0.1",
                                                   "--port", str(records[0]["port"]), "--threads", "6",
                                                   "--language", "ja", "--max-len", "4096"])
        self.assertEqual(cfg["observed_devices"], [{"device": "cuda", "compute_type": "f16"}])
        self.assertIsNone(cfg["whisper_decode_profile"])
        self.assertEqual(result["warnings"], [])
        self.assertEqual(len(records[1:]), 5)  # One discarded warmup + 2 x 2 warm.
        self.assertTrue(all("beam_size" not in r["fields"] for r in records[1:]))
        self.assertEqual([r["frames"] for r in records[1:]], ["0000" * 160] * 3 + ["0100" * 160] * 2)
        self.assertEqual(len(warm["samples"]), 4)
        self.assertTrue(all(s["text"] == "日本語" and s["language"] == "ja" for s in warm["samples"]))
        self.assertTrue(all(s["wall_ms"] >= 0 and math.isnan(s["duration_ms"]) for s in warm["samples"]))
        self.assertEqual(cold["load_ms"]["count"], 1)
        self.assertGreater(cold["load_ms"]["p50"], 0)
        self.assertEqual(cold["samples"], [])
        logs = list(self.root.glob("*.server.log"))
        self.assertEqual(len(logs), 1)
        self.assertIn("server diagnostics on stdout", logs[0].read_text())
        self.assertIn("server diagnostics on stderr", logs[0].read_text())
        self.assertNotIn("server diagnostics", self.output.read_text(encoding="utf-8"))
        scored = self.root / "score.json"
        comparison = self.root / "compare.json"
        for destination, baseline in ((scored, []), (comparison, ["--baseline", str(scored)])):
            process = subprocess.run([sys.executable, str(_SCRIPT.with_name("score_asr.py")),
                                      "--bench", str(self.output), "--manifest", str(self.manifest), "--json",
                                      "--output", str(destination), *baseline], capture_output=True, text=True, timeout=15)
            self.assertEqual(process.returncode, 0, process.stderr)
        report = json.loads(scored.read_text(encoding="utf-8"))
        self.assertEqual(report["missing_items"], [])
        self.assertEqual(report["overall"]["normalized"]["cer"]["cer"], 0)

    def test_beam_language_override_and_no_warmup(self) -> None:
        code, _, stderr = self.run_main(self.argv("--beam-size", "5", "--language", "ja-JP", "--warmup", "0", "--threads", "4"))
        self.assertEqual(code, 0, stderr)
        result = json.loads(self.output.read_text(encoding="utf-8"))
        self.assertEqual(result["config"]["decode"], "beam5")
        self.assertEqual(result["config"]["language_source"], "argument")
        self.assertEqual(result["config"]["worker_cmd"][-6:], ["--threads", "4", "--language", "ja", "--max-len", "4096"])
        self.assertEqual(result["config"]["threads"], 4)
        self.assertEqual(len(self.records()), 5)
        self.assertTrue(all(r["fields"]["beam_size"] == "5" for r in self.records()[1:]))

    def test_thread_rule_and_language_mapping(self) -> None:
        for count, expected in ((None, 4), (1, 4), (6, 4), (7, 5), (8, 6), (16, 12), (64, 12)):
            with self.subTest(count=count), patch.object(bench.os, "cpu_count", return_value=count):
                self.assertEqual(bench.default_threads(), expected)
                args = bench.parse_args(["--model", str(self.model), "--manifest", str(self.manifest)])
                self.assertEqual(bench.effective_threads(args), expected if expected > 4 else None)
                self.assertEqual("--threads" in bench.build_server_cmd(args, 8178, "ja"), expected > 4)
        for locale in ("ja", "ja-JP", "JA_jp"):
            self.assertEqual(bench.primary_language(locale), "ja")
        args = bench.parse_args(self.argv("--threads", "3"))
        self.assertEqual(bench.build_server_cmd(args, 8178, "ja")[-6:],
                         ["--threads", "3", "--language", "ja", "--max-len", "4096"])

    def test_bad_responses_name_item_remove_reservation_and_terminate(self) -> None:
        for mode in ("http-error", "created", "redirect", "malformed", "bad-text", "not-object"):
            with self.subTest(mode=mode):
                self.output = self.root / f"{mode}.json"
                code, stdout, stderr = self.run_main(self.argv(mode=mode))
                self.assertEqual(code, 1, stderr)
                self.assertIn("item 'ja-0'", stderr)
                self.assertEqual(stdout, "")
                self.assertFalse(self.output.exists())

    def test_startup_exit_timeout_and_spawn_failure(self) -> None:
        code, _, stderr = self.run_main(self.argv(mode="exit"))
        self.assertEqual(code, 1, stderr)
        self.assertFalse(self.output.exists())
        self.assertIn("server exited before ready", stderr)
        proc = Mock()
        proc.poll.return_value = None
        with self.output.open("wb") as log, patch.object(bench.subprocess, "Popen", return_value=proc), \
                patch.object(bench, "attach_windows_job"), patch.object(bench, "stop_server") as stop, \
                patch.object(bench.time, "perf_counter", side_effect=[0.0, 301.0]):
            with self.assertRaisesRegex(bench.BenchError, "readiness timed out"):
                with bench.server_session(["synthetic"], 300, self.spare_port(), log):
                    self.fail("unready server yielded")
        stop.assert_called_once_with(proc)
        self.output.unlink()
        opener = Mock()
        opener.open.side_effect = TimeoutError("synthetic request timeout")
        with patch.object(bench.urllib.request, "build_opener", return_value=opener):
            code, _, stderr = self.run_main(self.argv())
        self.assertEqual(code, 1, stderr)
        self.assertIn("item 'ja-0'", stderr)
        self.assertFalse(self.output.exists())
        args = self.argv()
        args = args[:args.index("--server-cmd")] + ["--server-cmd", str(self.root / "missing.exe")]
        code, _, _ = self.run_main(args)
        self.assertEqual(code, 1)
        self.assertFalse(self.output.exists())

    def test_ctrl_c_terminates_server(self) -> None:
        with patch.object(bench, "transcribe", side_effect=KeyboardInterrupt):
            code, _, stderr = self.run_main(self.argv())
        self.assertEqual(code, 130)
        self.assertIn("interrupted", stderr)
        self.assertFalse(self.output.exists())

    def test_existing_output_is_never_changed_or_server_started(self) -> None:
        for content in (b"existing", b""):
            with self.subTest(content=content):
                self.output.write_bytes(content)
                code, _, _ = self.run_main(self.argv())
                self.assertEqual(code, 1)
                self.assertEqual(self.output.read_bytes(), content)
                self.assertFalse(self.events.exists())

    def test_invalid_inputs_do_not_create_output_or_start_server(self) -> None:
        for extra in (("--repeat", "0"), ("--warmup", "-1"), ("--threads", "0"), ("--beam-size", "0"), ("--beam-size", "1"),
                      ("--timeout", "nan"), ("--timeout", "0"), ("--language", "-"), ("--model-sha256", "bad")):
            with self.subTest(extra=extra):
                code, _, _ = self.run_main(self.argv(*extra))
                self.assertEqual(code, 1)
                self.assertFalse(self.output.exists())
                self.assertFalse(self.events.exists())
        for bad in ("missing-model", "missing-wav", "bad-format", "bad-manifest"):
            with self.subTest(bad=bad):
                if bad == "missing-model":
                    args = self.argv("--model", str(self.root / "absent.bin"))
                elif bad == "missing-wav":
                    args = self.argv()
                    (self.root / "ja-0.wav").unlink()
                elif bad == "bad-format":
                    args = self.argv()
                    with wave.open(str(self.root / "ja-0.wav"), "wb") as wav:
                        wav.setparams((1, 2, 24000, 1, "NONE", "not compressed"))
                        wav.writeframes(b"\0\0")
                else:
                    args = self.argv()
                    self.manifest.write_text("{}", encoding="utf-8")
                code, _, _ = self.run_main(args)
                self.assertEqual(code, 1)
                self.assertFalse(self.output.exists())

    def test_occupied_port_is_skipped(self) -> None:
        first, second = Mock(), Mock()
        first.__enter__ = Mock(return_value=first)
        first.__exit__ = Mock(return_value=False)
        second.__enter__ = Mock(return_value=second)
        second.__exit__ = Mock(return_value=False)
        first.bind.side_effect = OSError("occupied")
        with patch.object(bench.socket, "socket", side_effect=[first, second]):
            self.assertEqual(bench.free_port(), 8179)
        first.bind.assert_called_once_with(("127.0.0.1", 8178))
        second.bind.assert_called_once_with(("127.0.0.1", 8179))
        first.bind.side_effect = OSError("occupied")
        with patch.object(bench.socket, "socket", return_value=first):
            with self.assertRaisesRegex(bench.BenchError, "8178-8199"):
                bench.free_port()

    def test_relative_exe_and_model_are_absolute_at_spawn(self) -> None:
        (self.root / "engines").mkdir()
        (self.root / "engines" / "whisper-server.exe").write_bytes(b"synthetic executable; never launched")
        args = self.argv("--server-exe", "engines/whisper-server.exe", "--model", self.model.name)
        args = args[:args.index("--server-cmd")]
        with patch.object(bench.os, "getcwd", return_value=str(self.root)), \
                patch.object(bench.subprocess, "Popen", side_effect=OSError("synthetic spawn failure")) as start:
            code, _, stderr = self.run_main(args)
        self.assertEqual(code, 1, stderr)
        command = start.call_args.args[0]
        self.assertEqual(command[0], str(self.root / "engines" / "whisper-server.exe"))
        self.assertEqual(command[1:3], ["--model", str(self.model)])

    def test_cpu_device_mismatch_and_unknown_device_are_recorded(self) -> None:
        for mode, expected in (("cpu", [{"device": "cpu", "compute_type": "f16"}]), ("unknown-device", [])):
            with self.subTest(mode=mode):
                self.output = self.root / f"{mode}.json"
                code, _, stderr = self.run_main(self.argv(mode=mode))
                self.assertEqual(code, 0, stderr)
                result = json.loads(self.output.read_text(encoding="utf-8"))
                self.assertEqual(result["config"]["observed_devices"], expected)
                self.assertEqual(len(result["warnings"]), 1)
                self.assertIn(result["warnings"][0], stderr)

    def test_device_log_markers(self) -> None:
        for marker, expected in (("using CUDA0 backend", "cuda"),
                                 ("whisper_backend_init_gpu: found GPU device 0: CUDA0", "cuda"),
                                 ("whisper_model_load:        CUDA0 total size = 12 MB", "cuda"),
                                 ("using CPU backend", "cpu")):
            with self.subTest(marker=marker), self.output.open("wb") as log:
                log.write(marker.encode())
                log.flush()
                self.assertEqual(bench.observed_device(log), expected)

    def test_pid_ownership_rejects_foreign_listener_even_with_ready_marker(self) -> None:
        proc = Mock(pid=1234)
        port = self.spare_port()
        output = (f" TCP 127.0.0.1:{port} 0.0.0.0:0 LISTENING 9999\n"
                  f" TCP 127.0.0.1:{port} 127.0.0.1:12 ESTABLISHED 1234\n"
                  " TCP 0.0.0.0:42 0.0.0.0:0 LISTENING 1234\n")
        with self.output.open("wb") as log:
            log.write(f"listening at http://127.0.0.1:{port}".encode())
            log.flush()
            with patch.object(bench, "IS_WINDOWS", True), \
                    patch.object(bench.subprocess, "run", return_value=Mock(stdout=output)):
                owned, reason = bench.listener_owned(proc, port, log, 30)
                self.assertFalse(owned)
                self.assertIn("listener PID [9999] != server PID 1234", reason)
            with patch.object(bench, "IS_WINDOWS", True), \
                    patch.object(bench.subprocess, "run", return_value=Mock(stdout=output.replace("9999", "1234"))):
                self.assertTrue(bench.listener_owned(proc, port, log, 30)[0])
            with patch.object(bench, "IS_WINDOWS", True), \
                    patch.object(bench.subprocess, "run", side_effect=OSError("netstat unavailable")):
                self.assertTrue(bench.listener_owned(proc, port, log, 30)[0])
            with patch.object(bench, "IS_WINDOWS", False):
                self.assertFalse(bench.listener_owned(proc, port + 1, log, 30)[0])

    def test_readiness_timeout_names_last_ownership_result(self) -> None:
        port = self.spare_port()
        for output, expected in (("", "no LISTENING row found"),
                                 ("unparsable netstat output", "netstat output may be unparsable"),
                                 (f"TCP 127.0.0.1:{port} 0.0.0.0:0 LISTENING 1234", "listener PID [1234] != server PID 5678")):
            with self.subTest(output=output), (self.root / "ownership.log").open("wb") as log:
                proc = Mock(pid=5678)
                proc.poll.return_value = None
                with patch.object(bench, "IS_WINDOWS", True), \
                        patch.object(bench.subprocess, "Popen", return_value=proc), \
                        patch.object(bench.subprocess, "run", return_value=Mock(stdout=output)), \
                        patch.object(bench, "attach_windows_job", return_value=None), \
                        patch.object(bench, "stop_server") as stop, \
                        patch.object(bench.socket, "create_connection", return_value=MagicMock()), \
                        patch.object(bench.time, "sleep"), \
                        patch.object(bench.time, "perf_counter", side_effect=[0, 0, 0.1, 0.1, 0.2, 301]):
                    with self.assertRaises(bench.BenchError) as raised:
                        with bench.server_session(["synthetic"], 300, port, log):
                            self.fail("unverified listener yielded")
                self.assertIn("readiness timed out", str(raised.exception))
                self.assertIn(expected, str(raised.exception))
                stop.assert_called_once_with(proc)

    def test_missing_exe_or_model_fails_before_output_reservation(self) -> None:
        for flag, missing in (("--server-exe", "not-on-path.exe"), ("--model", "missing-model.bin")):
            with self.subTest(flag=flag):
                args = self.argv(flag, str(self.root / missing))
                args = args[:args.index("--server-cmd")]
                with patch.object(bench.subprocess, "Popen") as start:
                    code, _, stderr = self.run_main(args)
                self.assertEqual(code, 1, stderr)
                self.assertIn("file not found", stderr)
                self.assertIn(str(self.root / missing), stderr)
                self.assertIn("bare names are not searched on PATH", stderr)
                self.assertFalse(self.output.exists())
                start.assert_not_called()

    def test_readiness_failure_stops_server_before_any_request(self) -> None:
        with patch.object(bench, "listener_owned", side_effect=bench.BenchError("listener PID mismatch")):
            code, _, stderr = self.run_main(self.argv())
        self.assertEqual(code, 1, stderr)
        self.assertIn("PID mismatch", stderr)
        self.assertFalse(self.output.exists())
        self.assertEqual(len(self.records()), 1)

    def test_process_exit_checks_cover_readiness_warmup_requests_and_write(self) -> None:
        original = bench.ensure_running
        for stage in ("after readiness", "after warmup item", "after warmup", "after request", "before writing"):
            with self.subTest(stage=stage):
                self.output = self.root / (stage.replace(" ", "-") + ".json")

                def check(proc, current):  # noqa: ANN001, ANN202
                    if current.startswith(stage):
                        proc.terminate()
                        proc.wait(timeout=15)
                    original(proc, current)

                with patch.object(bench, "ensure_running", side_effect=check):
                    code, _, stderr = self.run_main(self.argv())
                self.assertEqual(code, 1, stderr)
                self.assertIn("server exited " + stage, stderr)
                self.assertFalse(self.output.exists())

    def test_output_write_failure_preserves_json_on_stdout(self) -> None:
        original = Path.open

        class FailingWriter:
            def __init__(self, stream):  # noqa: ANN001
                self.stream = stream

            def fileno(self):  # noqa: ANN201
                return self.stream.fileno()

            def write(self, text):  # noqa: ANN001, ANN201
                raise OSError("synthetic full disk")

            def close(self):  # noqa: ANN201
                self.stream.close()

        def open_path(path, *args, **kwargs):  # noqa: ANN001, ANN002, ANN003, ANN202
            stream = original(path, *args, **kwargs)
            return FailingWriter(stream) if path == self.output and args == ("x",) else stream

        with patch.object(Path, "open", open_path):
            code, stdout, stderr = self.run_main(self.argv())
        self.assertEqual(code, 1, stderr)
        self.assertIn("synthetic full disk", stderr)
        self.assertEqual(len(json.loads(stdout)["warm"]["samples"]), 4)
        self.assertFalse(self.output.exists())

    def test_cleanup_preserves_replaced_empty_file(self) -> None:
        def replace_output(*args, **kwargs):  # noqa: ANN002, ANN003, ANN202
            # The original remains open on Windows, so simulate a replacement
            # identity rather than renaming an open file in this test.
            raise bench.BenchError("synthetic failure after reservation")

        with patch.object(bench, "run_benchmark", side_effect=replace_output), \
                patch.object(bench.os.path, "samestat", return_value=False) as same:
            code, _, stderr = self.run_main(self.argv())
        self.assertEqual(code, 1, stderr)
        self.assertEqual(self.output.read_bytes(), b"")
        same.assert_called_once()

    def test_windows_job_sets_kill_on_close_and_handles_denied_assignment(self) -> None:
        kernel = Mock()
        kernel.CreateJobObjectW.return_value = 77
        kernel.SetInformationJobObject.return_value = True
        kernel.AssignProcessToJobObject.return_value = True
        proc = Mock(_handle=123)
        job = bench.attach_windows_job(proc, kernel)
        self.assertIsNotNone(job)
        limits = kernel.SetInformationJobObject.call_args.args
        self.assertEqual(limits[1], 9)
        self.assertEqual(limits[2]._obj.BasicLimitInformation.LimitFlags, 0x2000)
        self.assertEqual(kernel.AssignProcessToJobObject.call_args.args[1].value, 123)
        job.close()
        job.close()
        kernel.CloseHandle.assert_called_once_with(77)
        kernel.reset_mock()
        kernel.AssignProcessToJobObject.return_value = False
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertIsNone(bench.attach_windows_job(proc, kernel))
        kernel.CloseHandle.assert_called_once_with(77)

    def test_windows_graceful_stop_falls_back_to_terminate_and_kill(self) -> None:
        proc = Mock()
        proc.poll.return_value = None
        proc.wait.side_effect = [subprocess.TimeoutExpired("stub", 2), subprocess.TimeoutExpired("stub", 2), 0]
        with patch.object(bench, "IS_WINDOWS", True):
            bench.stop_server(proc)
        proc.send_signal.assert_called_once()
        proc.terminate.assert_called_once()
        proc.kill.assert_called_once()

    def test_final_wait_timeout_warns_without_raising(self) -> None:
        proc = Mock(pid=5678)
        proc.poll.return_value = None
        proc.wait.side_effect = subprocess.TimeoutExpired("stub", 2)
        stderr = io.StringIO()
        with patch.object(bench, "IS_WINDOWS", True), contextlib.redirect_stderr(stderr):
            bench.stop_server(proc)
        self.assertIn("warning: server PID 5678 did not exit after kill", stderr.getvalue())
        proc.kill.assert_called_once()


if __name__ == "__main__":
    unittest.main()
