from __future__ import annotations

import argparse
import importlib.util
import json
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
    # Dataclasses resolve their module through sys.modules while the class is built.
    sys.modules[spec.name] = module
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
    def test_language_is_optional_and_explicit_locale_reaches_each_request(self):
        class Session:
            def __init__(self): self.requests = []
            def request(self, request):
                self.requests.append(request)
                return {"text": "sample", "duration_ms": 10.0}, 20.0

        for language in (None, "ja-JP"):
            # The legacy shape is the request earlier bench runs sent.
            session = Session()
            conditions = bench_asr.RequestConditions(shape="legacy", prompt=["term"], language=language)
            bench_asr.run_transcribe_loop(session, [Path("audio.wav")], 2, conditions)
            self.assertEqual(len(session.requests), 2)
            for request in session.requests:
                self.assertEqual(request["prompt"], ["term"])
                if language is None:
                    self.assertNotIn("language", request)
                else:
                    self.assertEqual(request["language"], language)
            # The production shape always carries the locale, null when unset.
            session = Session()
            conditions = bench_asr.RequestConditions(prompt="term", language=language)
            bench_asr.run_transcribe_loop(session, [Path("audio.wav")], 2, conditions)
            self.assertTrue(all(r["language"] == language and r["prompt"] == "term" for r in session.requests))

    def test_language_is_preserved_in_cold_warm_requests_and_report(self):
        sessions = []
        class Session:
            def __init__(self, *args, **kwargs):
                self.requests = []
                sessions.append(self)
            def __enter__(self): return self
            def __exit__(self, *args): return None
            def request(self, request):
                self.requests.append(request)
                return {"text": "sample", "model": "mock", "duration_ms": 10.0}, 20.0

        args = bench_asr.parse_args(["--language", "ja-JP", "--cold-runs", "2", "--repeat", "3"])
        with patch.object(bench_asr, "WorkerSession", Session):
            report = bench_asr.run_benchmark(args, [Path("audio.wav")])
        self.assertEqual(report["config"]["language"], "ja-JP")
        self.assertEqual([len([r for r in s.requests if r["command"] == "transcribe"]) for s in sessions], [1, 1, 3])
        self.assertTrue(all(r["language"] == "ja-JP" for s in sessions for r in s.requests if r["command"] == "transcribe"))

    def test_language_cli_rejects_invalid_length_before_starting_worker(self):
        self.assertIsNone(bench_asr.parse_args([]).language)
        self.assertEqual(bench_asr.parse_args(["--language", "x" * 32]).language, "x" * 32)
        for value in ("", "   ", "x" * 33):
            with patch.object(bench_asr, "WorkerSession") as session:
                with self.assertRaises(SystemExit):
                    bench_asr.main(["--language", value])
                session.assert_not_called()

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
        records = bench_asr.run_transcribe_loop(session, [Path("audio.wav")], 2, bench_asr.RequestConditions(), True)
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



# A worker that records every raw request line, to compare bytes with the app.
_RECORDING_WORKER = r"""
import json, sys
log = open(sys.argv[1], "ab")
for raw in sys.stdin.buffer:
    log.write(raw)
    log.flush()
    request = json.loads(raw)
    print(json.dumps({"id": request["id"], "ok": True, "text": "", "duration_ms": 1}), flush=True)
    if request.get("command") == "shutdown":
        break
"""

Entry = bench_asr.DictionaryEntry
OPENAI = Entry(reading="おーぷんえーあい", surface="OpenAI", aliases=("ChatGPT",))


class ProductionRequestShapeTests(IsolatedWorkerTestCase):
    def test_request_lines_are_the_bytes_the_app_sends(self) -> None:
        # src-tauri/src/asr.rs transcribe_request: serde_json sorts keys, keeps
        # non-ASCII text, always sends prompt and language, ends with LF, and
        # only a greedy preview request carries decoding.
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "requests.jsonl"
            cmd = [sys.executable, "-c", _RECORDING_WORKER, str(log)]
            prompt = bench_asr.render_asr_prompt([OPENAI], "openai-compatible")
            live = bench_asr.LivePreviewLoad(greedy=True)
            with bench_asr.WorkerSession(cmd, timeout_s=30) as session:
                session.request(bench_asr.transcribe_payload(Path("rec.wav"), bench_asr.RequestConditions()))
                conditions = bench_asr.RequestConditions(prompt=prompt, language="ja-JP", live=live)
                session.request(bench_asr.transcribe_payload(Path("rec.wav"), conditions))
                session.request(bench_asr.transcribe_payload(Path("rec.wav"), conditions, preview=True))
            raw = log.read_bytes()
        self.assertNotIn(b"\r", raw)
        self.assertEqual(
            raw.split(b"\n")[:3],
            [
                b'{"audio_path":"rec.wav","command":"transcribe","id":1,"language":null,"prompt":null}',
                '{"audio_path":"rec.wav","command":"transcribe","id":2,"language":"ja-JP",'
                '"prompt":"おーぷんえーあい => OpenAI (aliases: ChatGPT)"}'.encode("utf-8"),
                '{"audio_path":"rec.wav","command":"transcribe","decoding":"greedy","id":3,'
                '"language":"ja-JP","prompt":"おーぷんえーあい => OpenAI (aliases: ChatGPT)"}'.encode("utf-8"),
            ],
        )

    def test_standard_preview_request_has_no_decoding_field(self) -> None:
        conditions = bench_asr.RequestConditions(live=bench_asr.LivePreviewLoad(greedy=False))
        preview = bench_asr.transcribe_payload(Path("rec.wav"), conditions, preview=True)
        self.assertEqual(preview, {"command": "transcribe", "audio_path": "rec.wav", "prompt": None, "language": None})

    def test_prompt_rendering_matches_the_app_storage_tests(self) -> None:
        # Expectations ported from src-tauri/src/storage.rs asr_prompt_* and
        # hotwords_* tests.
        render = bench_asr.render_asr_prompt
        self.assertEqual(render([OPENAI], "openai-compatible"), "おーぷんえーあい => OpenAI (aliases: ChatGPT)")
        self.assertEqual(render([OPENAI], "faster-whisper"), "OpenAI, ChatGPT, おーぷんえーあい")
        self.assertIsNone(render([], "faster-whisper"))

        long_reading = "x" * 100
        entries = [
            Entry(reading=long_reading, surface=f"Term{index:02}", priority=index)
            for index in (3, 12, 1, 7, 10, 2, 5, 11, 4, 9, 6, 8)
        ]
        prompt = render(entries, "openai-compatible")
        self.assertLessEqual(len(prompt), 1200)
        self.assertEqual(prompt.split("\n"), [f"{long_reading} => Term{index:02}" for index in range(12, 2, -1)])
        surfaces = ", ".join(f"Term{index:02}" for index in range(12, 0, -1))
        self.assertEqual(render(entries, "faster-whisper"), f"{surfaces}, {long_reading}")

        auto = Entry(reading="auto term", surface="AutoTerm", source="auto")
        self.assertEqual(render([auto], "openai-compatible"), "AutoTerm")
        self.assertEqual(render([auto], "faster-whisper"), "AutoTerm")

        hotwords = [
            Entry(reading="high reading", surface="High", aliases=("High Alias",), priority=9),
            Entry(reading="high alias", surface="Mid", priority=5),
            Entry(reading="low", surface="Low", priority=1),
            auto,
        ]
        self.assertEqual(render(hotwords, "faster-whisper"), "High, Mid, Low, AutoTerm, High Alias, high reading")
        budget = [Entry(reading="x" * 190, surface="Alpha", priority=9), Entry(reading="b", surface="Beta", priority=1)]
        self.assertEqual(render(budget, "faster-whisper"), "Alpha, Beta, b")

    def test_scoped_entries_follow_the_app_context(self) -> None:
        entries = [
            Entry(reading=reading, surface=surface, aliases=("global-alias",), priority=1, app_scope=scope)
            for reading, surface, scope in [
                ("global-alias", "Global", None),
                ("app-alias", "App", "app:code"),
                ("category-alias", "Category", "category:development"),
                ("other-alias", "Other", "app:other"),
            ]
        ]
        scoped = bench_asr.render_asr_prompt(entries, "openai-compatible", app_key="code", app_category="development")
        self.assertEqual(
            sorted(line.split(" => ")[1].split(" ")[0] for line in scoped.split("\n")),
            ["App", "Category", "Global"],
        )
        self.assertEqual(
            bench_asr.render_asr_prompt(entries, "openai-compatible"),
            "global-alias => Global (aliases: global-alias)",
        )

    def test_dictionary_csv_uses_the_app_import_format(self) -> None:
        header = "reading,surface,category,aliases,priority,app_scope"
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "dictionary.csv"
            path.write_text(
                header + "\n"
                '"read, ing", Surface ,group,"one| two",3,app:code\nnext,Other,,,0,global\nblank,Blank,,,,\n',
                encoding="utf-8",
            )
            entries = bench_asr.load_dictionary_csv(path)
            self.assertEqual(
                entries,
                [
                    Entry(reading="read, ing", surface="Surface", aliases=("one", "two"), priority=3, app_scope="app:code"),
                    Entry(reading="next", surface="Other"),
                    Entry(reading="blank", surface="Blank"),
                ],
            )
            path.write_text(header + ",source\nauto term,AutoTerm,,,,,auto\n", encoding="utf-8")
            self.assertEqual(bench_asr.load_dictionary_csv(path), [Entry(reading="auto term", surface="AutoTerm", source="auto")])
            for invalid in (
                "surface,reading\nx,y\n",
                header + "\nx,y,,,high,\n",
                header + "\n ,y,,,,\n",
                header + "\nx,y,,,\n",
                header + ",source\nx,y,,,,,other\n",
            ):
                path.write_text(invalid, encoding="utf-8")
                with self.assertRaises(bench_asr.BenchError):
                    bench_asr.load_dictionary_csv(path)

    def test_prompt_terms_and_csv_render_together_for_the_backend(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "dictionary.csv"
            path.write_text(
                "reading,surface,category,aliases,priority,app_scope\nおーぷんえーあい,OpenAI,,ChatGPT,1,\n",
                encoding="utf-8",
            )
            whisper = bench_asr.parse_args(
                ["--backend", "faster-whisper", "--dictionary-csv", str(path), "--prompt", "VibeVoice", "--prompt", "Tauri"]
            )
            conditions, config = bench_asr.request_conditions(whisper)
        self.assertEqual(conditions.prompt, "OpenAI, Tauri, VibeVoice, ChatGPT, おーぷんえーあい")
        self.assertEqual((config["prompt"]["entries"], config["prompt"]["terms"]), (3, 2))
        self.assertEqual(config["prompt"]["chars"], len(conditions.prompt))
        # The dictionary text itself is not stored in results.
        self.assertNotIn("ChatGPT", json.dumps(config, ensure_ascii=False))
        other = bench_asr.parse_args(["--backend", "vibevoice", "--prompt", "VibeVoice", "--prompt", "Tauri"])
        self.assertEqual(bench_asr.request_conditions(other)[0].prompt, "Tauri => Tauri\nVibeVoice => VibeVoice")

    def test_legacy_shape_keeps_the_old_request_and_refuses_production_only_options(self) -> None:
        conditions, config = bench_asr.request_conditions(bench_asr.parse_args(["--request-shape", "legacy", "--prompt", "a"]))
        self.assertEqual((conditions.prompt, config["request_shape"]), (["a"], "legacy"))
        for extra in (["--dictionary-csv", "x.csv"], ["--app-key", "code"], ["--live-preview"]):
            with self.assertRaises(bench_asr.BenchError):
                bench_asr.request_conditions(bench_asr.parse_args(["--request-shape", "legacy", *extra]))
        with self.assertRaises(bench_asr.BenchError):
            bench_asr.request_conditions(bench_asr.parse_args(["--live-preview-greedy"]))
        for invalid in (["--idle-gap-s", "-1"], ["--idle-gap-s", "nan"], ["--live-recording-s", "0"]):
            with self.assertRaises(SystemExit):
                bench_asr.parse_args(invalid)


class FakeClock:
    """A simulated clock: sleeps and worker requests advance it."""

    def __init__(self) -> None:
        self.now = 0.0
        self.events: list[tuple[str, float]] = []

    def sleep(self, seconds: float) -> None:
        self.events.append(("sleep", seconds))
        self.now += seconds


class TimedSession:
    """A worker whose requests take simulated time; preview audio is preview.wav."""

    def __init__(self, clock: FakeClock, costs: dict[str, float]) -> None:
        self.clock = clock
        self.costs = costs
        self.requests: list[dict] = []

    def __enter__(self) -> "TimedSession":
        return self

    def __exit__(self, *args: object) -> None:
        return None

    def request(self, request: dict) -> tuple[dict, float]:
        self.requests.append(request)
        kind = request["command"]
        if kind == "transcribe":
            kind = "preview" if request["audio_path"] == "preview.wav" else "final"
        cost = self.costs.get(kind, 0.0)
        self.clock.now += cost
        self.clock.events.append((kind, self.clock.now))
        if kind == "health":
            return {"health": {"backend": "mock", "device": "cuda", "computeType": "float16"}}, cost * 1000.0
        return {"text": "t", "model": "mock", "duration_ms": cost * 1000.0}, cost * 1000.0


class IdleAndLiveLoadTests(IsolatedWorkerTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.clock = FakeClock()
        for name, value in (("_now", lambda: self.clock.now), ("_sleep", self.clock.sleep)):
            patcher = patch.object(bench_asr, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def _session_class(self, costs: dict[str, float], sessions: list) -> type:
        clock = self.clock

        class Session(TimedSession):
            def __init__(self, *args: object, **kwargs: object) -> None:
                super().__init__(clock, costs)
                sessions.append(self)

        return Session

    def _live(self, **overrides: object) -> "bench_asr.RequestConditions":
        live = dict(interval_s=0.15, prep_s=0.0, recording_s=1.0, greedy=True, audio=Path("preview.wav"))
        live.update(overrides)
        return bench_asr.RequestConditions(language="ja-JP", prompt="p", live=bench_asr.LivePreviewLoad(**live))

    def test_idle_gap_precedes_each_warm_request_and_is_not_timed(self) -> None:
        sessions: list = []
        args = bench_asr.parse_args(["--idle-gap-s", "2.5", "--repeat", "3", "--cold-runs", "1"])
        with patch.object(bench_asr, "WorkerSession", self._session_class({"final": 0.1}, sessions)):
            report = bench_asr.run_benchmark(args, [Path("audio.wav")])
        _, warm = sessions
        self.assertEqual([r["command"] for r in warm.requests], ["load", "health", "transcribe", "transcribe", "transcribe"])
        # Cold: load then the request at once; warm: idle before every request.
        self.assertEqual(
            [kind for kind, _ in self.clock.events],
            ["load", "final", "load", "health", "sleep", "final", "sleep", "final", "sleep", "final"],
        )
        self.assertTrue(all(value == 2.5 for kind, value in self.clock.events if kind == "sleep"))
        self.assertAlmostEqual(report["warm"]["wall_ms"]["max"], 100.0)
        self.assertEqual(report["config"]["idle_gap_s"], 2.5)
        self.assertEqual(report["config"]["worker_health"], {"backend": "mock", "device": "cuda", "compute_type": "float16"})
        self.assertEqual(report["config"]["request_shape"], "production")

    def test_stop_waits_for_an_in_flight_preview_before_the_final_request(self) -> None:
        session = TimedSession(self.clock, {"preview": 0.2, "final": 0.1})
        records = bench_asr.run_transcribe_loop(session, [Path("final.wav")], 1, self._live())
        # Sleep 0.15, preview to 0.35; to 0.7; to 1.05: stop at 1.0 waits 50 ms.
        self.assertEqual(
            [kind for kind, _ in self.clock.events],
            ["sleep", "preview", "sleep", "preview", "sleep", "preview", "final"],
        )
        record = records[0]
        self.assertEqual(record["preview_requests"], 3)
        self.assertAlmostEqual(record["live_drain_ms"], 50.0)
        self.assertAlmostEqual(record["stop_to_final_ms"], 150.0)
        self.assertAlmostEqual(record["wall_ms"], 100.0)
        previews = [r for r in session.requests if r["audio_path"] == "preview.wav"]
        self.assertTrue(all(r["decoding"] == "greedy" and r["language"] == "ja-JP" and r["prompt"] == "p" for r in previews))
        self.assertEqual(
            session.requests[-1], {"command": "transcribe", "audio_path": "final.wav", "prompt": "p", "language": "ja-JP"}
        )

    def test_stop_during_the_sleep_needs_no_drain(self) -> None:
        session = TimedSession(self.clock, {"preview": 0.05, "final": 0.1})
        record = bench_asr.run_transcribe_loop(session, [Path("final.wav")], 1, self._live(recording_s=0.5))[0]
        self.assertEqual(record["preview_requests"], 2)
        self.assertEqual(record["live_drain_ms"], 0.0)
        self.assertAlmostEqual(self.clock.events[-1][1], 0.6)  # final sent at stop (0.5)

    def test_stop_during_preparation_waits_for_it_and_sends_no_preview(self) -> None:
        session = TimedSession(self.clock, {"preview": 0.01, "final": 0.1})
        conditions = self._live(recording_s=0.4, prep_s=0.3)
        record = bench_asr.run_transcribe_loop(session, [Path("final.wav")], 1, conditions)[0]
        self.assertEqual(record["preview_requests"], 0)
        self.assertAlmostEqual(record["live_drain_ms"], 50.0)

    def test_cold_runs_have_no_idle_gap_or_live_load(self) -> None:
        sessions: list = []
        args = bench_asr.parse_args(["--idle-gap-s", "1", "--live-preview", "--live-recording-s", "0.5", "--repeat", "2"])
        with patch.object(bench_asr, "WorkerSession", self._session_class({"final": 0.1, "preview": 0.05}, sessions)):
            report = bench_asr.run_benchmark(args, [Path("final.wav")])
        cold, warm = sessions
        self.assertEqual([r["command"] for r in cold.requests], ["load", "transcribe"])
        self.assertGreater(len([r for r in warm.requests if r["command"] == "transcribe"]), 2)
        self.assertNotIn("live_drain_ms", report["cold"]["samples"][0])
        self.assertEqual(report["warm"]["stop_to_final_ms"]["count"], 2)
        self.assertEqual(report["config"]["live_preview"]["recording_s"], 0.5)
        self.assertFalse(report["config"]["live_preview"]["greedy"])
        # Preview texts are not transcription samples, so scoring ignores them.
        self.assertEqual(len(report["warm"]["samples"]), 2)

    def test_health_failure_is_reported_as_unknown(self) -> None:
        class Session(TimedSession):
            def __init__(self, *args: object, **kwargs: object) -> None:
                super().__init__(FakeClock(), {})

            def request(self, request: dict) -> tuple[dict, float]:
                if request["command"] == "health":
                    raise bench_asr.BenchError("unsupported_operation")
                return super().request(request)

        with patch.object(bench_asr, "WorkerSession", Session):
            report = bench_asr.run_benchmark(bench_asr.parse_args(["--repeat", "1"]), [Path("audio.wav")])
        self.assertIsNone(report["config"]["worker_health"])


class SmokeParityTests(IsolatedWorkerTestCase):
    def test_mock_worker_run_records_every_condition(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "result.json"
            argv = [
                "--smoke", "--json", "--output", str(output), "--repeat", "2", "--cold-runs", "1",
                "--language", "ja-JP", "--prompt", "Tauri", "--idle-gap-s", "0.01",
                "--live-preview", "--live-preview-greedy", "--live-recording-s", "0.4", "--record-device",
            ]
            self.assertEqual(bench_asr.main(argv), 0)
            report = json.loads(output.read_text(encoding="utf-8"))
        config = report["config"]
        self.assertEqual(config["request_shape"], "production")
        self.assertEqual(config["language"], "ja-JP")
        self.assertEqual(config["idle_gap_s"], 0.01)
        self.assertTrue(config["live_preview"]["greedy"])
        self.assertEqual(config["prompt"]["entries"], 1)
        self.assertEqual(config["worker_health"]["backend"], "mock")
        self.assertEqual(config["audio_format"], [{"sample_rate": 24000, "channels": 1, "sample_width_bytes": 2}])
        self.assertEqual(report["warm"]["stop_to_final_ms"]["count"], 2)
        self.assertGreaterEqual(report["warm"]["preview_wall_ms"]["count"], 2)
        self.assertTrue(all("device" in sample for sample in report["warm"]["samples"]))
        self.assertIn("live preview load", bench_asr.render_markdown(report))

class ManifestBenchTests(IsolatedWorkerTestCase):
    def _fixture(self, root: Path) -> Path:
        import wave

        items = []
        for item_id, language in (("tiny-ja", "ja-JP"), ("tiny-en", "en-US")):
            audio = root / "audio" / f"{item_id}.wav"
            audio.parent.mkdir(exist_ok=True)
            with wave.open(str(audio), "wb") as stream:
                stream.setnchannels(1)
                stream.setsampwidth(2)
                stream.setframerate(16000)
                stream.writeframes(b"\0\0" * 160)
            items.append({
                "id": item_id, "audio_path": f"../audio/{item_id}.wav", "reference": "synthetic",
                "language": language, "source": "fixture", "duration": 0.01,
                "split": "test", "pool": "dev", "dataset_id": "synthetic", "revision": "synthetic",
                "locale": language, "item_id": item_id, "group": item_id, "speaker": None,
                "audio_sha256": "0" * 64, "source_audio_sha256": "0" * 64,
                "provenance": "synthetic", "license": "test",
            })
        manifest = root / "manifests" / "synthetic-smoke.json"
        manifest.parent.mkdir(exist_ok=True)
        manifest.write_text(json.dumps({
            "schema": "asr-bench-manifest/1", "name": "synthetic-smoke", "created_by": "test",
            "selection": {"profile": "smoke", "seed": 1},
            "sources": [{"source": "fixture", "license": "test", "revision": "synthetic"}], "items": items,
        }), encoding="utf-8")
        return manifest

    def test_manifest_mock_benchmark_and_language_override(self) -> None:
        for override in ([], ["--language", "fr-FR"]):
            with self.subTest(override=override), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp).resolve()
                manifest = self._fixture(root)
                output = root / "result.json"
                self.assertEqual(bench_asr.main([
                    "--manifest", str(manifest), "--repeat", "2", "--cold-runs", "1",
                    "--json", "--output", str(output), *override,
                ]), 0)
                result = json.loads(output.read_text(encoding="utf-8"))
                expected = ["fr-FR"] * 4 if override else ["ja-JP", "ja-JP", "en-US", "en-US"]
                self.assertEqual([r["language"] for r in result["warm"]["samples"]], expected)
                expected_cold = ["fr-FR"] * 2 if override else ["ja-JP", "en-US"]
                self.assertEqual([r["language"] for r in result["cold"]["samples"]], expected_cold)
                self.assertEqual(Path(result["config"]["audio"][0]), root / "audio" / "tiny-ja.wav")
                self.assertEqual(result["config"]["manifest"], {
                    "name": "synthetic-smoke", "schema": "asr-bench-manifest/1", "path": str(manifest),
                    "sources": [{"source": "fixture", "license": "test", "revision": "synthetic"}], "item_count": 2,
                })
                self.assertEqual(result["config"]["language_source"], "argument" if override else "manifest")
                markdown = bench_asr.render_markdown(result)
                self.assertIn("manifest: `synthetic-smoke`", markdown)
                self.assertIn("language: `fr-FR`" if override else "language: `per-item (manifest)`", markdown)

    def test_manifest_language_reaches_final_and_preview_requests(self) -> None:
        class Session:
            def __init__(self) -> None:
                self.requests = []

            def request(self, payload: dict) -> tuple[dict, float]:
                self.requests.append(payload)
                return {"text": "synthetic", "duration_ms": 0}, 1.0

        session = Session()
        paths = [Path("ja.wav"), Path("en.wav")]
        conditions = bench_asr.RequestConditions(
            item_languages=dict(zip(paths, ("ja-JP", "en-US"))),
            live=bench_asr.LivePreviewLoad(recording_s=0.01),
        )

        def preview(worker, audio, item_conditions):  # noqa: ANN001, ANN202
            worker.request(bench_asr.transcribe_payload(audio, item_conditions, preview=True))
            return {"live_drain_ms": 0.0, "preview_wall_ms": [1.0]}

        with patch.object(bench_asr, "simulate_live_preview", side_effect=preview):
            bench_asr.run_transcribe_loop(session, paths, 1, conditions)
        self.assertEqual([r["language"] for r in session.requests], ["ja-JP", "ja-JP", "en-US", "en-US"])
        self.assertTrue(all(set(r) == {"command", "audio_path", "prompt", "language"} for r in session.requests))

    def test_manifest_rejects_conflicting_audio_inputs_and_bad_manifest(self) -> None:
        import contextlib
        import io

        for flags in (["--audio", "x.wav"], ["x.wav"], ["--smoke"]):
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                bench_asr.parse_args(["--manifest", "m.json", *flags])
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            bench_asr.parse_args(["x.wav"])
        self.assertEqual(bench_asr.parse_args(["--audio", "x.wav", "--audio", "y.wav"]).audio, ["x.wav", "y.wav"])
        with tempfile.TemporaryDirectory() as tmp:
            path = self._fixture(Path(tmp))
            data = json.loads(path.read_text(encoding="utf-8"))
            for change in ("schema", "duplicate", "missing_audio", "empty"):
                invalid = json.loads(json.dumps(data))
                if change == "schema":
                    invalid["schema"] = "unknown"
                elif change == "duplicate":
                    invalid["items"].append(invalid["items"][0])
                elif change == "empty":
                    invalid["items"] = []
                else:
                    invalid["items"][0]["id"] = "missing"
                    invalid["items"][0]["audio_path"] = "audio/missing.wav"
                path.write_text(json.dumps(invalid), encoding="utf-8")
                with contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(bench_asr.main(["--manifest", str(path)]), 1)

    def test_manifest_output_never_overwrites(self) -> None:
        import contextlib
        import io

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = self._fixture(root)
            output = root / "result.json"
            output.write_text("keep", encoding="utf-8")
            with contextlib.redirect_stderr(io.StringIO()), patch.object(bench_asr, "run_benchmark") as run:
                self.assertEqual(bench_asr.main(["--manifest", str(path), "--output", str(output)]), 1)
                run.assert_not_called()
            self.assertEqual(output.read_text(encoding="utf-8"), "keep")

    def test_manifest_loader_registers_and_caches_scorer(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(sys.modules):
            path = self._fixture(Path(tmp).resolve())
            sys.modules.pop("_bench_score_manifest", None)
            original = bench_asr.importlib.util.spec_from_file_location
            with patch.object(bench_asr.importlib.util, "spec_from_file_location", wraps=original) as make_spec:
                bench_asr.load_manifest(path)
                scorer = sys.modules["_bench_score_manifest"]
                bench_asr.load_manifest(path)
            self.assertEqual(make_spec.call_count, 1)
            self.assertIs(scorer, sys.modules[scorer.__name__])

    def test_non_manifest_language_source_is_auto_or_argument(self) -> None:
        for flags, expected in (([], "auto"), (["--language", "ja-JP"], "argument")):
            with tempfile.TemporaryDirectory() as tmp:
                output = Path(tmp) / "result.json"
                self.assertEqual(bench_asr.main([
                    "--smoke", "--cold-runs", "0", "--repeat", "1", "--json", "--output", str(output), *flags,
                ]), 0)
                result = json.loads(output.read_text(encoding="utf-8"))
                self.assertEqual(result["config"]["language_source"], expected)
                self.assertNotIn("manifest", result["config"])


if __name__ == "__main__":
    unittest.main()
