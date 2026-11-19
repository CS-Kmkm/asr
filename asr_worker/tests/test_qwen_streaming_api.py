import os
import struct
import wave
from unittest.mock import patch

import httpx
import pytest

from asr_worker.backends import BackendError, create_backend
from asr_worker.qwen_streaming_api import QwenStreamingApiBackend
from asr_worker.streaming import StreamingSession


def backend(handler):
    with patch.dict(os.environ, {"ASR_API_BASE_URL": "http://127.0.0.1:8000", "ASR_MODEL_ID": "Qwen/Qwen3-ASR-1.7B"}):
        result = QwenStreamingApiBackend()
    result.client = httpx.Client(base_url="http://127.0.0.1:8000/", transport=httpx.MockTransport(handler))
    return result


def test_official_demo_routes_pcm_and_forbids_unverified_final_reuse():
    requests = []
    def handle(request):
        requests.append(request)
        if request.url.path == "/api/start": return httpx.Response(200, json={"session_id": "server-one"})
        return httpx.Response(200, json={"text": "確定文" if request.url.path == "/api/finish" else "途中文"})
    service = backend(handle)
    session = StreamingSession(service)
    session.handle({"command": "stream_start", "session_id": "client-one", "sample_rate": 16000})
    response = session.handle({"command": "stream_push", "session_id": "client-one", "offset": 0, "samples": [0.25, -0.5]})
    assert response["text"] == "途中文"
    assert requests[1].content == struct.pack("<2f", 0.25, -0.5)
    final = session.handle({"command": "stream_flush", "session_id": "client-one", "offset": 2})
    assert final["text"] == "確定文" and final["final"] and final["reusable"] is False
    assert len(requests) == 3
    assert requests[1].url.params["session_id"] == "server-one"
    service.unload()


def test_qwen_full_audio_fallback_resamples_and_closes_its_session(tmp_path):
    payloads = []
    def handle(request):
        if request.url.path == "/api/start": return httpx.Response(200, json={"session_id": "one"})
        if request.url.path == "/api/chunk": payloads.append(request.content)
        return httpx.Response(200, json={"text": "日本語"})
    service = backend(handle)
    path = tmp_path / "audio.wav"
    with wave.open(str(path), "wb") as audio:
        audio.setnchannels(1); audio.setsampwidth(2); audio.setframerate(24000)
        audio.writeframes(struct.pack("<4800h", *([10] * 4800)))
    text, _ = service.transcribe(path, None)
    assert text == "日本語"
    assert sum(map(len, payloads)) == 3200 * 4
    with pytest.raises(BackendError) as caught: service.create_online_stream("ja-JP")
    assert caught.value.code == "language_unsupported"
    service.unload()


def test_invalid_wav_header_is_rejected_before_opening_a_session(tmp_path):
    def handle(request):
        pytest.fail(f"Unexpected request: {request.url.path}")

    service = backend(handle)
    path = tmp_path / "invalid.wav"
    path.write_bytes(b"RIFF")
    with pytest.raises(BackendError) as caught:
        service.transcribe(path, None)
    assert caught.value.code == "audio_unsupported"
    service.unload()


@pytest.mark.parametrize("url", ["https://api.example", "http://localhost:8000", "http://192.168.1.3:8000", "http://127.0.0.1:8000/v1", "http://user:pass@127.0.0.1:8000"])
def test_connector_rejects_unapproved_remote_or_ambiguous_endpoints(url):
    with patch.dict(os.environ, {"ASR_API_BASE_URL": url}): service = QwenStreamingApiBackend()
    with pytest.raises(BackendError): service.load("bf16")


def test_protocol_selection_is_explicit_and_preserves_the_existing_backend_by_default():
    with patch.dict(os.environ, {"ASR_QWEN_DEMO_STREAMING": "0"}):
        assert not isinstance(create_backend("openai-compatible"), QwenStreamingApiBackend)
    with patch.dict(os.environ, {"ASR_QWEN_DEMO_STREAMING": "1"}):
        assert isinstance(create_backend("openai-compatible"), QwenStreamingApiBackend)
