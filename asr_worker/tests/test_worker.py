from __future__ import annotations

import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from asr_worker.backends import MockBackend, map_backend_exception, vibevoice_dependency_error
from asr_worker.worker import Worker, serve


class WorkerTests(unittest.TestCase):
    def test_protocol_success(self) -> None:
        worker = Worker(MockBackend())
        response, stop = worker.handle({"id": 1, "command": "load"})
        self.assertTrue(response["ok"])
        self.assertEqual(response["quantization"], "4bit")
        self.assertFalse(stop)

    def test_op_remains_compatible(self) -> None:
        response, _ = Worker(MockBackend()).handle({"id": 1, "op": "load"})
        self.assertTrue(response["ok"])

    def test_malformed_request(self) -> None:
        output = io.StringIO()
        serve(Worker(MockBackend()), io.StringIO("{bad json}\n"), output)
        response = json.loads(output.getvalue())
        self.assertEqual(response["id"], None)
        self.assertEqual(response["error"]["code"], "invalid_json")

    def test_load_required_before_transcribe(self) -> None:
        response, _ = Worker(MockBackend()).handle(
            {"id": "request", "op": "transcribe", "audio_path": "unused.wav"}
        )
        self.assertEqual(response["error"]["code"], "model_not_loaded")

    def test_oom_mapping(self) -> None:
        error = map_backend_exception(RuntimeError("CUDA out of memory"), "load")
        self.assertEqual(error.code, "gpu_oom")

    def test_missing_vibevoice_dependency_has_install_instructions(self) -> None:
        error = vibevoice_dependency_error(
            ModuleNotFoundError("No module named 'transformers'", name="transformers")
        )
        self.assertEqual(error.code, "backend_unavailable")
        self.assertIn("transformers", error.message)
        self.assertIn("uv sync --extra vibevoice", error.message)

    def test_mock_transcription(self) -> None:
        worker = Worker(MockBackend())
        worker.handle({"id": 1, "op": "load", "quantization": "8bit"})
        with tempfile.NamedTemporaryFile(suffix=".wav") as audio:
            with patch.dict(os.environ, {"ASR_WORKER_MOCK_TEXT": "hello test"}):
                response, _ = worker.handle(
                    {"id": 2, "op": "transcribe", "audio_path": str(Path(audio.name))}
                )
        self.assertTrue(response["ok"])
        self.assertEqual(response["text"], "hello test")
        self.assertEqual(response["segments"][0]["text"], "hello test")
        self.assertIsInstance(response["duration_ms"], int)

    def test_shutdown_unloads_backend_and_stops_worker(self) -> None:
        backend = MockBackend()
        worker = Worker(backend)
        worker.handle({"id": 1, "command": "load"})

        response, stop = worker.handle({"id": 2, "command": "shutdown"})

        self.assertTrue(response["ok"])
        self.assertTrue(stop)
        self.assertFalse(worker.loaded)
        self.assertFalse(backend.loaded)

    def test_prompt_accepts_dictionary_term_list(self) -> None:
        worker = Worker(MockBackend())
        worker.handle({"id": 1, "command": "load"})
        with tempfile.NamedTemporaryFile(suffix=".wav") as audio:
            response, _ = worker.handle(
                {
                    "id": 2,
                    "command": "transcribe",
                    "audio_path": audio.name,
                    "prompt": ["VibeVoice", "Tauri"],
                }
            )
        self.assertTrue(response["ok"])

    def test_prompt_rejects_non_string_dictionary_terms(self) -> None:
        worker = Worker(MockBackend())
        worker.handle({"id": 1, "command": "load"})
        with tempfile.NamedTemporaryFile(suffix=".wav") as audio:
            response, _ = worker.handle(
                {
                    "id": 2,
                    "command": "transcribe",
                    "audio_path": audio.name,
                    "prompt": ["valid", 3],
                }
            )
        self.assertEqual(response["error"]["code"], "invalid_request")

    def test_language_validation(self) -> None:
        worker = Worker(MockBackend())
        worker.handle({"id": 1, "command": "load"})
        with tempfile.NamedTemporaryFile(suffix=".wav") as audio:
            response, _ = worker.handle(
                {
                    "id": 2,
                    "command": "transcribe",
                    "audio_path": audio.name,
                    "language": ["en"],
                }
            )
        self.assertEqual(response["error"]["code"], "invalid_request")


if __name__ == "__main__":
    unittest.main()
