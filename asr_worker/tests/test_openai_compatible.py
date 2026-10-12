from __future__ import annotations

import json
import os
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

import httpx

from asr_worker.api import _subtitle, _verbose_response
from asr_worker.backends import BackendError, OpenAICompatibleBackend, create_backend
from asr_worker.backends import MockBackend


class OpenAICompatibleBackendTests(unittest.TestCase):
    def create_local_backend(self) -> OpenAICompatibleBackend:
        with patch.dict(
            os.environ,
            {
                "ASR_API_BASE_URL": "http://127.0.0.1:8000/v1",
                "ASR_MODEL_ID": "local-asr",
            },
            clear=True,
        ):
            backend = create_backend("openai-compatible")
        backend.load("4bit")
        return backend

    def test_openai_endpoint_requires_key(self) -> None:
        with patch.dict(os.environ, {}, clear=True):
            backend = OpenAICompatibleBackend()
        with self.assertRaises(BackendError) as context:
            backend.load("4bit")
        self.assertEqual(context.exception.code, "missing_api_key")

    def test_transcribe_uses_openai_multipart_contract(self) -> None:
        captured_body = b""

        def handler(request: httpx.Request) -> httpx.Response:
            nonlocal captured_body
            captured_body = request.read()
            return httpx.Response(
                200,
                json={
                    "text": "hello API",
                    "segments": [{"start": 0, "end": 1.5, "text": "hello API"}],
                },
            )

        backend = self.create_local_backend()
        backend.client = httpx.Client(
            base_url="http://127.0.0.1:8000/v1/",
            transport=httpx.MockTransport(handler),
        )
        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as audio:
            audio.write(b"RIFF-test")
            audio_path = Path(audio.name)
        try:
            text, segments = backend.transcribe(audio_path, "product names", "en-GB")
        finally:
            audio_path.unlink(missing_ok=True)

        self.assertEqual(text, "hello API")
        self.assertEqual(segments[0]["text"], "hello API")
        self.assertIsNone(segments[0]["speaker"])
        self.assertIn(b'name="model"', captured_body)
        self.assertIn(b"local-asr", captured_body)
        self.assertIn(b'name="file"', captured_body)
        self.assertIn(b'name="prompt"', captured_body)
        self.assertIn(b'name="language"', captured_body)
        self.assertIn(b"\r\n\r\nen\r\n", captured_body)
        self.assertNotIn(b"en-GB", captured_body)
        self.assertIn(b'name="response_format"', captured_body)
        self.assertIn(b"json", captured_body)

    def test_api_error_is_mapped_without_echoing_body(self) -> None:
        backend = self.create_local_backend()
        backend.client = httpx.Client(
            base_url="http://127.0.0.1:8000/v1/",
            transport=httpx.MockTransport(
                lambda _request: httpx.Response(
                    401,
                    content=b"not-json secret response body",
                )
            ),
        )
        with tempfile.NamedTemporaryFile(suffix=".wav") as audio:
            with self.assertRaises(BackendError) as context:
                backend.transcribe(Path(audio.name), None)
        self.assertEqual(context.exception.code, "api_request_failed")
        self.assertNotIn("secret response body", context.exception.message)


class ApiFormattingTests(unittest.TestCase):
    def test_verbose_response_is_openai_shaped(self) -> None:
        payload = _verbose_response(
            "hello",
            [{"start": 1.25, "end": 2.5, "text": "hello", "speaker": None}],
            "en",
        )
        self.assertEqual(payload["task"], "transcribe")
        self.assertEqual(payload["language"], "en")
        self.assertEqual(payload["segments"][0]["start"], 1.25)
        json.dumps(payload)

    def test_srt_and_vtt_timestamps(self) -> None:
        segments = [{"start": 1.25, "end": 2.5, "text": "hello"}]
        self.assertIn("00:00:01,250 --> 00:00:02,500", _subtitle(segments, "", vtt=False))
        self.assertIn("00:00:01.250 --> 00:00:02.500", _subtitle(segments, "", vtt=True))


class ApiServerTests(unittest.TestCase):
    def test_transcriptions_endpoint_and_auth(self) -> None:
        from fastapi.testclient import TestClient

        from asr_worker.api import create_app

        with patch.dict(os.environ, {"ASR_WORKER_MOCK_TEXT": "served locally"}):
            client = TestClient(create_app(MockBackend(), api_key="test-key", served_model="local-asr"))
            unauthorized = client.post(
                "/v1/audio/transcriptions",
                data={"model": "local-asr"},
                files={"file": ("sample.wav", b"RIFF-test", "audio/wav")},
            )
            response = client.post(
                "/v1/audio/transcriptions",
                headers={"Authorization": "Bearer test-key"},
                data={"model": "local-asr", "response_format": "json", "language": "en"},
                files={"file": ("sample.wav", b"RIFF-test", "audio/wav")},
            )
        self.assertEqual(unauthorized.status_code, 401)
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json(), {"text": "served locally"})

    def test_models_and_verbose_json(self) -> None:
        from fastapi.testclient import TestClient

        from asr_worker.api import create_app

        client = TestClient(create_app(MockBackend(), served_model="local-asr"))
        models = client.get("/v1/models")
        response = client.post(
            "/v1/audio/transcriptions",
            data={"model": "local-asr", "response_format": "verbose_json"},
            files={"file": ("sample.wav", b"RIFF-test", "audio/wav")},
        )
        self.assertEqual(models.json()["data"][0]["id"], "local-asr")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["task"], "transcribe")

    @staticmethod
    def stream_events(response: httpx.Response) -> list[dict]:
        return [
            json.loads(line[len("data: "):])
            for line in response.text.splitlines()
            if line.startswith("data: ")
        ]

    def test_streaming_sends_one_delta_per_segment_then_done(self) -> None:
        from fastapi.testclient import TestClient

        from asr_worker.api import create_app

        seen: list[Path] = []

        class SegmentedBackend(MockBackend):
            def transcribe_segments(self, audio_path, prompt, language=None):
                seen.append(Path(audio_path))
                yield {"start": 0.0, "end": 1.0, "speaker": None, "text": " Hello"}
                yield {"start": 1.0, "end": 2.0, "speaker": None, "text": " world."}

        client = TestClient(create_app(SegmentedBackend(), served_model="local-asr"))
        response = client.post(
            "/v1/audio/transcriptions",
            data={"model": "local-asr", "stream": "true"},
            files={"file": ("sample.wav", b"RIFF-test", "audio/wav")},
        )
        self.assertEqual(response.status_code, 200)
        self.assertTrue(response.headers["content-type"].startswith("text/event-stream"))
        events = self.stream_events(response)
        self.assertEqual(
            [event["type"] for event in events],
            ["transcript.text.delta", "transcript.text.delta", "transcript.text.done"],
        )
        self.assertEqual([event["delta"] for event in events[:2]], ["Hello", " world."])
        self.assertEqual(events[-1]["text"], "Hello world.")
        # The producer deletes the upload once decoding finishes.
        for _ in range(100):
            if not seen[0].exists():
                break
            time.sleep(0.01)
        self.assertFalse(seen[0].exists())

    def test_streaming_failure_after_text_ends_with_an_error_event(self) -> None:
        from fastapi.testclient import TestClient

        from asr_worker.api import create_app

        class FailsMidway(MockBackend):
            def transcribe_segments(self, audio_path, prompt, language=None):
                yield {"start": 0.0, "end": 1.0, "speaker": None, "text": " Hello"}
                raise BackendError("gpu_oom", "GPU out of memory while operating the ASR model")

        client = TestClient(create_app(FailsMidway(), served_model="local-asr"))
        response = client.post(
            "/v1/audio/transcriptions",
            data={"model": "local-asr", "stream": "true"},
            files={"file": ("sample.wav", b"RIFF-test", "audio/wav")},
        )
        # The status was already sent with the first delta.
        self.assertEqual(response.status_code, 200)
        events = self.stream_events(response)
        self.assertEqual([event["type"] for event in events], ["transcript.text.delta", "error"])
        self.assertEqual(events[1]["error"]["code"], "gpu_oom")

    def test_streaming_whole_transcript_backends_send_a_single_delta(self) -> None:
        from fastapi.testclient import TestClient

        from asr_worker.api import create_app

        with patch.dict(os.environ, {"ASR_WORKER_MOCK_TEXT": "served locally"}):
            client = TestClient(create_app(MockBackend(), served_model="local-asr"))
            response = client.post(
                "/v1/audio/transcriptions",
                data={"model": "local-asr", "stream": "true", "response_format": "text"},
                files={"file": ("sample.wav", b"RIFF-test", "audio/wav")},
            )
        events = self.stream_events(response)
        self.assertEqual(events[0], {"type": "transcript.text.delta", "delta": "served locally", "logprobs": []})
        self.assertEqual(events[1]["text"], "served locally")

    def test_streaming_rejects_segment_formats_and_reports_early_failures(self) -> None:
        from fastapi.testclient import TestClient

        from asr_worker.api import create_app

        class FailingBackend(MockBackend):
            def transcribe_segments(self, audio_path, prompt, language=None):
                raise BackendError("gpu_oom", "GPU out of memory while operating the ASR model")
                yield  # pragma: no cover - makes this a generator

        client = TestClient(create_app(FailingBackend(), served_model="local-asr"))
        unsupported = client.post(
            "/v1/audio/transcriptions",
            data={"model": "local-asr", "stream": "true", "response_format": "srt"},
            files={"file": ("sample.wav", b"RIFF-test", "audio/wav")},
        )
        failed = client.post(
            "/v1/audio/transcriptions",
            data={"model": "local-asr", "stream": "true"},
            files={"file": ("sample.wav", b"RIFF-test", "audio/wav")},
        )
        self.assertEqual(unsupported.status_code, 400)
        self.assertEqual(failed.status_code, 500)
        self.assertEqual(failed.json()["error"]["code"], "gpu_oom")

    def test_invalid_and_oversized_requests_use_openai_error_shape(self) -> None:
        from fastapi.testclient import TestClient

        from asr_worker.api import create_app

        client = TestClient(create_app(MockBackend(), max_upload_bytes=4))
        missing_file = client.post("/v1/audio/transcriptions", data={"model": "local-asr"})
        too_large = client.post(
            "/v1/audio/transcriptions",
            data={"model": "local-asr"},
            files={"file": ("sample.wav", b"12345", "audio/wav")},
        )
        self.assertEqual(missing_file.status_code, 400)
        self.assertIn("error", missing_file.json())
        self.assertEqual(too_large.status_code, 413)
        self.assertEqual(too_large.json()["error"]["code"], "invalid_request")


if __name__ == "__main__":
    unittest.main()
