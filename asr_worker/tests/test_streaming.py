from __future__ import annotations

import math
from types import SimpleNamespace
from unittest.mock import patch

import pytest

from asr_worker.backends import BackendError, MockBackend
from asr_worker.streaming import StreamingSession
from asr_worker.worker import Worker
from asr_worker import sherpa_onnx_runtime as so


class OnlineBackend:
    supports_streaming = True
    model_name = "test:online"

    def create_online_stream(self, language=None):
        return []

    def decode_online(self, stream, samples, final):
        stream.extend(samples)
        return "確定" if final else f"仮説{len(stream)}"


def start(session, sid="a"):
    return session.handle({"command": "stream_start", "session_id": sid, "sample_rate": 16000})


def test_hypotheses_replace_and_flush_covers_exact_audio():
    session = StreamingSession(OnlineBackend())
    assert start(session)["offset"] == 0
    for offset in (0, 100):
        response = session.handle({"command": "stream_push", "session_id": "a", "offset": offset, "samples": [0.1] * 100})
        assert response["text"] == f"仮説{offset + 100}"
        assert not response["final"]
    response = session.handle({"command": "stream_flush", "session_id": "a", "offset": 200})
    assert response == {"session_id": "a", "offset": 200, "text": "確定", "model": "test:online", "final": True}
    assert session.stream is None
    start(session, "b")
    with pytest.raises(BackendError, match="older worker"):
        session.handle({"command": "stream_close", "session_id": "a"})
    assert session.session_id == "b"


@pytest.mark.parametrize("samples,offset", [([0], 1), ([0], True), ([], 0), ([math.nan], 0), ([math.inf], 0), ([2], 0), ([False], 0), ([0] * 32001, 0)])
def test_invalid_or_out_of_order_audio_invalidates_session(samples, offset):
    session = StreamingSession(OnlineBackend())
    start(session)
    with pytest.raises(BackendError):
        session.handle({"command": "stream_push", "session_id": "a", "offset": offset, "samples": samples})
    assert session.stream is None


def test_flush_failure_and_cancel_do_not_leave_reusable_partial_state():
    session = StreamingSession(OnlineBackend())
    start(session)
    with pytest.raises(BackendError):
        session.handle({"command": "stream_flush", "session_id": "a", "offset": 20})
    assert session.stream is None


def test_cancel_during_start_discards_the_created_stream():
    session = StreamingSession(OnlineBackend())
    checks = iter([False, True])
    with pytest.raises(BackendError) as caught:
        session.handle({"command": "stream_start", "session_id": "a", "sample_rate": 16000}, lambda: next(checks))
    assert caught.value.code == "cancelled"
    assert session.stream is None
    assert session.session_id is None
    start(session)
    with pytest.raises(BackendError) as caught:
        session.handle({"command": "stream_push", "session_id": "a", "offset": 0, "samples": [0.1]}, lambda: True)
    assert caught.value.code == "cancelled"
    assert session.stream is None


def test_batch_only_worker_rejects_stream_without_changing_batch_contract():
    worker = Worker(MockBackend())
    worker.handle({"id": 1, "command": "load", "quantization": "4bit"})
    result, _ = worker.handle({"id": 2, "command": "stream_start", "session_id": "a", "sample_rate": 16000})
    assert result["error"]["code"] == "stream_unsupported"
    assert worker.loaded


def test_online_runtime_keeps_one_stream_and_finishes_after_padding():
    backend = so.SherpaOnnxBackend()
    backend.catalog_id = so.NEMOTRON_STREAMING.id
    calls = []
    stream = SimpleNamespace(accept_waveform=lambda rate, values: calls.append((rate, len(values))),
                             input_finished=lambda: calls.append("finished"))
    ready = iter([True, False, True, False])
    backend.recognizer = SimpleNamespace(create_stream=lambda: stream, is_ready=lambda s: next(ready),
                                        decode_stream=lambda s: calls.append("decode"), get_result=lambda s: "日本語")
    assert backend.create_online_stream() is stream
    assert backend.decode_online(stream, [0.1] * 10, False) == "日本語"
    assert backend.decode_online(stream, [], True) == "日本語"
    assert calls == [(16000, 10), "decode", (16000, 16000), "finished", "decode"]


@pytest.mark.parametrize("language,expected", [("ja-JP", "ja"), ("en-GB", "en"), ("zh-TW", "zh"), ("es-MX", "es"), (None, None)])
def test_explicit_language_is_applied_before_live_and_batch_samples(language, expected):
    backend = so.SherpaOnnxBackend()
    backend.catalog_id = so.NEMOTRON_STREAMING.id
    options = []
    stream = SimpleNamespace(set_option=lambda key, value: options.append((key, value)),
                             accept_waveform=lambda rate, samples: None, input_finished=lambda: None)
    backend.recognizer = SimpleNamespace(create_stream=lambda: stream, is_ready=lambda s: False, get_result=lambda s: "日本語")
    session = StreamingSession(backend)
    session.handle({"command": "stream_start", "session_id": "a", "sample_rate": 16000, "language": language})
    backend._decode(backend.recognizer, [[0.1] * 100], 16000, language)
    assert options == ([] if expected is None else [("language", expected)] * 2)


def test_unsupported_language_does_not_create_a_stream():
    recognizer = SimpleNamespace(create_stream=lambda: pytest.fail("must validate first"))
    with pytest.raises(BackendError) as caught:
        so.SherpaOnnxBackend._online_stream(recognizer, "xx-XX")
    assert caught.value.code == "language_unsupported"


def test_offline_managed_model_never_downloads(tmp_path):
    from asr_worker.runtime_adapter import fetch_model
    with patch.dict("os.environ", {"ASR_OFFLINE_ONLY": "1", "ASR_MANAGED_MODEL_HOME": str(tmp_path)}), patch("httpx.Client", side_effect=AssertionError("network")):
        with pytest.raises(BackendError) as caught:
            fetch_model("sherpa-onnx", so.NEMOTRON_STREAMING, so.SherpaOnnxBackend())
    assert caught.value.code == "model_not_cached"


def test_replay_resampling_and_worker_protocol_cover_identical_audio():
    import base64
    import struct
    from asr_worker.streaming_replay import PcmResampler, ReplayAdapter

    for rate in (16000, 24000, 44100, 48000):
        samples = [math.sin(i * 0.05) for i in range(10003)]
        expected = PcmResampler(rate).push(samples, final=True)
        chunked = PcmResampler(rate)
        actual = []
        for offset in range(0, len(samples), 731):
            actual.extend(chunked.push(samples[offset:offset + 731]))
        actual.extend(chunked.push([], final=True))
        assert actual == expected
        assert len(chunked.pending) <= 1

    backend = OnlineBackend()
    backend.runtime_label, backend.backend_name = "fake CPU", "sherpa-onnx"
    backend.device, backend.compute_type = "cpu", "float32"
    adapter = ReplayAdapter(backend)
    ready = adapter.handle({"command": "start", "stream_id": "one", "sample_rate": 24000,
                            "channels": 1, "sample_width": 2, "language": "ja-JP"})
    assert ready["event"] == "ready"
    for offset in (0, 2400):
        event = adapter.handle({"command": "audio", "stream_id": "one", "offset_frames": offset,
                                "frames": 2400, "pcm_s16le": base64.b64encode(struct.pack("<2400h", *([123] * 2400))).decode()})
        assert event["event"] == "partial"
    final = adapter.handle({"command": "flush", "stream_id": "one"})
    assert final == {"stream_id": "one", "event": "final", "text": "確定"}
    assert adapter.offset == 3200
    assert adapter.session.stream is None


def test_replay_rejects_source_gaps_and_incorrect_pcm_size():
    from asr_worker.streaming_replay import ReplayAdapter
    backend = OnlineBackend()
    backend.runtime_label, backend.backend_name = "fake CPU", "sherpa-onnx"
    backend.device, backend.compute_type = "cpu", "float32"
    adapter = ReplayAdapter(backend)
    adapter.handle({"command": "start", "stream_id": "one", "sample_rate": 16000, "channels": 1, "sample_width": 2})
    for offset, frames, payload in [(1, 1, "AAA="), (0, 2, "AAA="), (0, True, "AAA=")]:
        with pytest.raises(BackendError):
            adapter.handle({"command": "audio", "stream_id": "one", "offset_frames": offset,
                            "frames": frames, "pcm_s16le": payload})
