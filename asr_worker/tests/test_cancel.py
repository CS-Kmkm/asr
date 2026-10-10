from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from typing import Any, Iterator
from unittest.mock import patch

from asr_worker.backends import BackendError, FasterWhisperBackend, MockBackend, VibeVoiceBackend
from asr_worker.worker import Worker, serve

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]


class _BlockingBackend(MockBackend):
    """Transcribes until cancelled, or for `seconds` when it cannot stop."""

    def __init__(self, *, honors_cancel: bool = True, seconds: float = 5.0) -> None:
        super().__init__()
        self.honors_cancel = honors_cancel
        self.seconds = seconds
        self.started = threading.Event()

    def transcribe(self, audio_path, prompt, language=None):  # type: ignore[no-untyped-def]
        self.started.set()
        deadline = time.monotonic() + self.seconds
        while time.monotonic() < deadline:
            if self.honors_cancel:
                self.raise_if_cancelled()
            elif self.is_cancel_requested():
                # Finish quickly once cancelled, like a backend whose
                # in-flight call happens to return right after the cancel.
                break
            time.sleep(0.01)
        return "late text", [{"start": 0.0, "end": 0.0, "speaker": None, "text": "late text"}]


def _cancel_once_started(backend: _BlockingBackend, audio: str) -> Iterator[str]:
    yield json.dumps({"id": 1, "command": "load"}) + "\n"
    yield json.dumps({"id": 2, "command": "transcribe", "audio_path": audio}) + "\n"
    if not backend.started.wait(5):
        raise AssertionError("transcription never started")
    yield json.dumps({"id": 3, "command": "cancel", "target_id": 2}) + "\n"


def _serve(backend: MockBackend, lines: Any) -> list[dict[str, Any]]:
    output = io.StringIO()
    serve(Worker(backend), lines, output)
    return [json.loads(line) for line in output.getvalue().splitlines()]


def _by_id(messages: list[dict[str, Any]]) -> dict[Any, dict[str, Any]]:
    return {message["id"]: message for message in messages}


class CancelOperationTests(unittest.TestCase):
    def test_running_transcription_stops_and_cancel_is_acknowledged(self) -> None:
        backend = _BlockingBackend()
        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as audio:
            pass
        try:
            started = time.monotonic()
            messages = _by_id(_serve(backend, _cancel_once_started(backend, audio.name)))
            elapsed = time.monotonic() - started
        finally:
            os.unlink(audio.name)

        self.assertLess(elapsed, 3)
        self.assertEqual(messages[3], {"id": 3, "ok": True, "cancelled": True})
        self.assertFalse(messages[2]["ok"])
        self.assertEqual(messages[2]["error"]["code"], "cancelled")
        self.assertIsNone(backend.cancel_requested)

    def test_result_of_a_backend_that_cannot_stop_is_discarded(self) -> None:
        backend = _BlockingBackend(honors_cancel=False)
        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as audio:
            pass
        try:
            messages = _by_id(_serve(backend, _cancel_once_started(backend, audio.name)))
        finally:
            os.unlink(audio.name)

        self.assertEqual(messages[2]["error"]["code"], "cancelled")
        self.assertNotIn("text", messages[2])

    def test_cancel_of_an_unknown_request_reports_nothing_cancelled(self) -> None:
        # Also the reply when the target was answered before the cancel arrived.
        requests = (
            json.dumps({"id": 1, "command": "load"})
            + "\n"
            + json.dumps({"id": 2, "command": "cancel", "target_id": 99})
            + "\n"
        )
        messages = _by_id(_serve(MockBackend(), io.StringIO(requests)))

        self.assertTrue(messages[1]["ok"])
        self.assertEqual(messages[2], {"id": 2, "ok": True, "cancelled": False})

    def test_cancel_requires_a_target(self) -> None:
        messages = _serve(MockBackend(), io.StringIO('{"id": 4, "command": "cancel"}\n'))
        self.assertEqual(messages[0]["id"], 4)
        self.assertEqual(messages[0]["error"]["code"], "invalid_request")

        response, stop = Worker(MockBackend()).handle({"id": 5, "command": "cancel", "target_id": 1})
        self.assertEqual(response, {"id": 5, "ok": True, "cancelled": False})
        self.assertFalse(stop)

    def test_unsupported_operation_lists_cancel(self) -> None:
        response, _ = Worker(MockBackend()).handle({"id": 1, "command": "hello"})
        self.assertEqual(response["error"]["code"], "unsupported_operation")
        self.assertIn("cancel", response["error"]["message"])


class BackendCancellationTests(unittest.TestCase):
    def test_faster_whisper_stops_between_segments(self) -> None:
        class _Segment:
            def __init__(self, text: str) -> None:
                self.start = 0.0
                self.end = 1.0
                self.text = text

        decoded: list[str] = []

        def segments() -> Iterator[_Segment]:
            for text in ("one", "two", "three"):
                decoded.append(text)
                yield _Segment(text)

        class _Model:
            def transcribe(self, *args: Any, **kwargs: Any) -> Any:
                return segments(), None

        backend = FasterWhisperBackend()
        backend.model = _Model()
        backend.cancel_requested = lambda: bool(decoded)
        with self.assertRaises(BackendError) as raised:
            backend.transcribe(Path("audio.wav"), None)

        self.assertEqual(raised.exception.code, "cancelled")
        self.assertEqual(decoded, ["one"])

    def test_vibevoice_generation_gets_a_stopping_criterion_and_is_not_parsed(self) -> None:
        import contextlib
        import types

        class _Ids:
            shape = (1, 3)

            def __getitem__(self, key: Any) -> "_Ids":
                return self

        class _Inputs(dict):
            def to(self, *args: Any) -> "_Inputs":
                return self

        class _Processor:
            decoded = False

            def apply_transcription_request(self, audio: str, prompt: str | None) -> _Inputs:
                return _Inputs(input_ids=_Ids())

            def decode(self, *args: Any, **kwargs: Any) -> Any:
                self.decoded = True
                return [[]]

        class _Model:
            device = "cpu"
            dtype = "bf16"
            options: dict[str, Any] = {}

            def generate(self, **options: Any) -> _Ids:
                _Model.options = options
                return _Ids()

        torch = types.ModuleType("torch")
        torch.inference_mode = contextlib.nullcontext  # type: ignore[attr-defined]
        backend = VibeVoiceBackend()
        backend.processor = _Processor()
        backend.model = _Model()
        backend.cancel_requested = lambda: True
        with patch.dict(sys.modules, {"torch": torch}), patch(
            "asr_worker.backends._cancel_stopping_criteria", return_value="criteria"
        ):
            with self.assertRaises(BackendError) as raised:
                backend.transcribe(Path("missing.wav"), None)

        self.assertEqual(raised.exception.code, "cancelled")
        self.assertEqual(_Model.options["stopping_criteria"], "criteria")
        self.assertFalse(backend.processor.decoded)


class WorkerProcessTests(unittest.TestCase):
    """The real `python -m asr_worker` process, as the desktop app runs it."""

    def test_cancel_keeps_the_process_and_shutdown_exits_cleanly(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            audio = Path(directory) / "take.wav"
            audio.write_bytes(b"")
            hold = Path(directory) / "hold"
            hold.write_bytes(b"")
            environment = {
                **os.environ,
                "PYTHONUTF8": "1",
                "ASR_WORKER_MOCK_HOLD_FILE": str(hold),
            }
            process = subprocess.Popen(
                [sys.executable, "-m", "asr_worker", "--backend", "mock"],
                cwd=REPOSITORY_ROOT,
                env=environment,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
            )
            assert process.stdin is not None and process.stdout is not None
            try:

                def send(message: dict[str, Any]) -> None:
                    process.stdin.write(json.dumps(message) + "\n")
                    process.stdin.flush()

                hold.unlink()
                send({"id": 1, "command": "load"})
                self.assertTrue(json.loads(process.stdout.readline())["ok"])
                hold.write_bytes(b"")
                started = time.monotonic()
                send({"id": 2, "command": "transcribe", "audio_path": str(audio)})
                time.sleep(0.2)
                send({"id": 3, "command": "cancel", "target_id": 2})
                replies = _by_id([json.loads(process.stdout.readline()) for _ in range(2)])
                self.assertLess(time.monotonic() - started, 10)
                self.assertTrue(replies[3]["cancelled"])
                self.assertEqual(replies[2]["error"]["code"], "cancelled")

                hold.unlink()
                send({"id": 4, "command": "shutdown"})
                self.assertTrue(json.loads(process.stdout.readline())["ok"])
                # The input thread is still blocked reading stdin; exiting must
                # not abort on its lock.
                self.assertEqual(process.wait(timeout=10), 0)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                process.stdin.close()
                process.stdout.close()
                assert process.stderr is not None
                process.stderr.close()


if __name__ == "__main__":
    unittest.main()
