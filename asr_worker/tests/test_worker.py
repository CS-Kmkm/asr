from __future__ import annotations

import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from asr_worker.backends import MockBackend, map_backend_exception, vibevoice_dependency_error
from asr_worker.worker import Worker, _configure_protocol_stdio, serve


class WorkerTests(unittest.TestCase):
    def test_protocol_stdio_is_forced_to_utf8(self) -> None:
        input_bytes = io.BytesIO()
        output_bytes = io.BytesIO()
        input_stream = io.TextIOWrapper(input_bytes, encoding="cp932")
        output_stream = io.TextIOWrapper(output_bytes, encoding="cp932")

        with patch("asr_worker.worker.sys.stdin", input_stream), patch(
            "asr_worker.worker.sys.stdout", output_stream
        ):
            _configure_protocol_stdio()

        self.assertEqual(input_stream.encoding.lower().replace("_", "-"), "utf-8")
        self.assertEqual(output_stream.encoding.lower().replace("_", "-"), "utf-8")

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

    def test_response_replaces_lone_surrogates_with_replacement_character(self) -> None:
        class SurrogateBackend(MockBackend):
            def transcribe(self, audio_path, prompt, language=None):  # type: ignore[no-untyped-def]
                text = "emoji:\ud83d\ude00 before\ud800after"
                return text, [
                    {
                        "start": 0.0,
                        "end": 1.0,
                        "speaker": "voice\udfff",
                        "text": text,
                    }
                ]

        worker = Worker(SurrogateBackend())
        with tempfile.NamedTemporaryFile(suffix=".wav") as audio:
            requests = (
                json.dumps({"id": 1, "command": "load"})
                + "\n"
                + json.dumps(
                    {"id": 2, "command": "transcribe", "audio_path": audio.name}
                )
                + "\n"
            )
            output = io.StringIO()
            serve(worker, io.StringIO(requests), output)

        response_line = output.getvalue().splitlines()[1]
        self.assertFalse(any(0xD800 <= ord(character) <= 0xDFFF for character in response_line))
        response = json.loads(response_line)
        self.assertEqual(response["text"], "emoji:\U0001f600 before\ufffdafter")
        self.assertEqual(response["segments"][0]["speaker"], "voice\ufffd")
        self.assertEqual(response["segments"][0]["text"], "emoji:\U0001f600 before\ufffdafter")

    def test_load_required_before_transcribe(self) -> None:
        response, _ = Worker(MockBackend()).handle(
            {"id": "request", "op": "transcribe", "audio_path": "unused.wav"}
        )
        self.assertEqual(response["error"]["code"], "model_not_loaded")

    def test_oom_mapping(self) -> None:
        error = map_backend_exception(RuntimeError("CUDA out of memory"), "load")
        self.assertEqual(error.code, "gpu_oom")

    def test_gated_hugging_face_model_reports_token_instructions(self) -> None:
        class GatedRepoError(Exception):
            pass

        cause = GatedRepoError("Access to this model is restricted; secret-token-must-not-leak")
        wrapper = OSError("Unable to load model")
        wrapper.__cause__ = cause

        error = map_backend_exception(wrapper, "load", "faster-whisper")

        self.assertEqual(error.code, "hf_auth_required")
        self.assertIn("HF_TOKEN", error.message)
        self.assertIn("gated-model access terms", error.message)
        self.assertNotIn("secret-token-must-not-leak", error.message)

    def test_hugging_face_rate_limit_reports_actionable_error(self) -> None:
        error = map_backend_exception(
            RuntimeError("429 Client Error: Too Many Requests (rate limit reached)"),
            "load",
        )

        self.assertEqual(error.code, "hf_rate_limited")
        self.assertIn("HF_TOKEN", error.message)
        self.assertIn("try again", error.message)

    def test_private_or_missing_hugging_face_model_reports_both_causes(self) -> None:
        class RepositoryNotFoundError(Exception):
            pass

        error = map_backend_exception(RepositoryNotFoundError("Repository Not Found"), "load")

        self.assertEqual(error.code, "hf_repository_unavailable")
        self.assertIn("model ID", error.message)
        self.assertIn("HF_TOKEN", error.message)

    def test_hugging_face_network_failure_is_distinct_from_auth_failure(self) -> None:
        class ConnectTimeout(Exception):
            pass

        error = map_backend_exception(ConnectTimeout("timed out"), "load")

        self.assertEqual(error.code, "hf_download_failed")
        self.assertIn("network connection", error.message)

    def test_unclassified_load_failure_keeps_generic_error(self) -> None:
        error = map_backend_exception(ValueError("invalid model configuration"), "load")

        self.assertEqual(error.code, "model_load_failed")
        self.assertIn("ValueError", error.message)

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


class _ProgressBackend(MockBackend):
    def load(self, quantization: str) -> None:
        self.report_progress("download", model="org/repo", completed_bytes=4, total_bytes=8)
        self.report_progress("load")
        super().load(quantization)


class LoadProgressTests(unittest.TestCase):
    def test_load_reports_progress_before_its_response(self) -> None:
        output = io.StringIO()
        serve(
            Worker(_ProgressBackend()),
            io.StringIO('{"id": 3, "command": "load"}\n'),
            output,
        )
        messages = [json.loads(line) for line in output.getvalue().splitlines()]

        self.assertEqual(
            [message.get("event") for message in messages],
            ["progress", "progress", None],
        )
        self.assertEqual(messages[0]["id"], 3)
        self.assertEqual(messages[0]["stage"], "download")
        self.assertEqual(messages[0]["total_bytes"], 8)
        self.assertEqual(messages[1]["stage"], "load")
        self.assertTrue(messages[2]["ok"])

    def test_progress_is_withheld_from_a_client_that_did_not_ask(self) -> None:
        # The worker runs from this repository, so a desktop build that predates
        # progress notifications must not receive them.
        output = io.StringIO()
        serve(
            Worker(_ProgressBackend()),
            io.StringIO('{"id": 3, "command": "load"}\n'),
            output,
            notify_progress=False,
        )
        messages = [json.loads(line) for line in output.getvalue().splitlines()]

        self.assertEqual(len(messages), 1)
        self.assertTrue(messages[0]["ok"])

    def test_progress_sink_is_released_after_the_load(self) -> None:
        backend = _ProgressBackend()
        worker = Worker(backend, notify=lambda message: None)

        worker.handle({"id": 1, "command": "load"})

        self.assertIsNone(backend.progress)


if __name__ == "__main__":
    unittest.main()
