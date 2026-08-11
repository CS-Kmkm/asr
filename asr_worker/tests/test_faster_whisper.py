from __future__ import annotations

import os
import sys
import types
import unittest
import wave
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

from asr_worker.backends import (
    BackendError,
    FasterWhisperBackend,
    compute_max_new_tokens,
    create_backend,
    faster_whisper_compute_type,
    wav_duration_seconds,
)


class CreateBackendTests(unittest.TestCase):
    def test_create_faster_whisper_backend(self) -> None:
        backend = create_backend("faster-whisper")
        self.assertIsInstance(backend, FasterWhisperBackend)

    def test_default_model_name(self) -> None:
        with patch.dict(os.environ, {}, clear=False):
            os.environ.pop("ASR_FASTER_WHISPER_MODEL", None)
            backend = FasterWhisperBackend()
        self.assertEqual(backend.model_name, "faster-whisper:large-v3-turbo")

    def test_model_name_respects_env(self) -> None:
        with patch.dict(os.environ, {"ASR_FASTER_WHISPER_MODEL": "tiny"}):
            backend = FasterWhisperBackend()
        self.assertEqual(backend.model_name, "faster-whisper:tiny")

    def test_generic_model_id_env_takes_precedence(self) -> None:
        with patch.dict(
            os.environ,
            {"ASR_MODEL_ID": "custom/whisper", "ASR_FASTER_WHISPER_MODEL": "tiny"},
        ):
            backend = FasterWhisperBackend()
        self.assertEqual(backend.model_name, "faster-whisper:custom/whisper")


class LoadUnavailableTests(unittest.TestCase):
    def test_load_without_faster_whisper_reports_backend_unavailable(self) -> None:
        backend = FasterWhisperBackend()
        # Ensure import fails: faster_whisper is not installed in this env.
        with patch.dict(sys.modules, {"faster_whisper": None}):
            with self.assertRaises(BackendError) as ctx:
                backend.load("4bit")
        self.assertEqual(ctx.exception.code, "backend_unavailable")
        self.assertIn("pip install faster-whisper", ctx.exception.message)

    def test_load_rejects_unsupported_quantization(self) -> None:
        backend = FasterWhisperBackend()
        with self.assertRaises(BackendError) as ctx:
            backend.load("fp32")
        self.assertEqual(ctx.exception.code, "unsupported_quantization")


class ComputeTypeTests(unittest.TestCase):
    def test_4bit_and_8bit(self) -> None:
        self.assertEqual(faster_whisper_compute_type("4bit", cuda=True), "int8_float16")
        self.assertEqual(faster_whisper_compute_type("4bit", cuda=False), "int8")
        self.assertEqual(faster_whisper_compute_type("8bit", cuda=True), "int8_float16")
        self.assertEqual(faster_whisper_compute_type("8bit", cuda=False), "int8")

    def test_bf16(self) -> None:
        self.assertEqual(faster_whisper_compute_type("bf16", cuda=True), "float16")
        self.assertEqual(faster_whisper_compute_type("bf16", cuda=False), "int8")

    def test_unknown_raises(self) -> None:
        with self.assertRaises(BackendError) as ctx:
            faster_whisper_compute_type("fp32", cuda=True)
        self.assertEqual(ctx.exception.code, "unsupported_quantization")


class MaxNewTokensTests(unittest.TestCase):
    def test_short_audio_clamped_to_floor(self) -> None:
        self.assertEqual(compute_max_new_tokens(1.0, None), 256)

    def test_long_audio_estimate(self) -> None:
        # 60s -> 60*40 + 128 = 2528, within bounds
        self.assertEqual(compute_max_new_tokens(60.0, None), 2528)

    def test_long_audio_clamped_to_ceiling(self) -> None:
        self.assertEqual(compute_max_new_tokens(10000.0, None), 4096)

    def test_env_override_takes_priority(self) -> None:
        self.assertEqual(compute_max_new_tokens(60.0, "512"), 512)

    def test_env_override_ignored_when_not_positive(self) -> None:
        self.assertEqual(compute_max_new_tokens(1.0, "0"), 256)
        self.assertEqual(compute_max_new_tokens(1.0, "not-a-number"), 256)

    def test_none_audio_returns_ceiling(self) -> None:
        self.assertEqual(compute_max_new_tokens(None, None), 4096)


class WavDurationTests(unittest.TestCase):
    def test_valid_wav(self) -> None:
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "tone.wav"
            with wave.open(str(path), "wb") as handle:
                handle.setnchannels(1)
                handle.setsampwidth(2)
                handle.setframerate(16000)
                handle.writeframes(b"\x00\x00" * 16000)  # 1 second
            self.assertAlmostEqual(wav_duration_seconds(path), 1.0, places=3)

    def test_non_wav_returns_none(self) -> None:
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "not.wav"
            path.write_bytes(b"not a wav file")
            self.assertIsNone(wav_duration_seconds(path))

    def test_missing_file_returns_none(self) -> None:
        self.assertIsNone(wav_duration_seconds(Path("/nonexistent/missing.wav")))


class _FakeSegment:
    def __init__(self, start: float, end: float, text: str) -> None:
        self.start = start
        self.end = end
        self.text = text


class _FakeWhisperModel:
    last_init_kwargs: dict = {}
    last_transcribe_kwargs: dict = {}

    def __init__(self, model_id: str, device: str, compute_type: str) -> None:
        _FakeWhisperModel.last_init_kwargs = {
            "model_id": model_id,
            "device": device,
            "compute_type": compute_type,
        }

    def transcribe(self, audio_path, language=None, hotwords=None):  # noqa: ANN001
        _FakeWhisperModel.last_transcribe_kwargs = {
            "audio_path": audio_path,
            "language": language,
            "hotwords": hotwords,
        }
        segments = [
            _FakeSegment(0.0, 1.5, " Hello"),
            _FakeSegment(1.5, 3.0, " world"),
        ]
        return iter(segments), types.SimpleNamespace(language="en")


def _install_fake_faster_whisper() -> dict[str, types.ModuleType]:
    fake_fw = types.ModuleType("faster_whisper")
    fake_fw.WhisperModel = _FakeWhisperModel  # type: ignore[attr-defined]
    fake_ct2 = types.ModuleType("ctranslate2")
    fake_ct2.get_cuda_device_count = lambda: 0  # type: ignore[attr-defined]
    return {"faster_whisper": fake_fw, "ctranslate2": fake_ct2}


class TranscribeWithFakeModuleTests(unittest.TestCase):
    def test_transcribe_formats_segments_and_passes_hotwords(self) -> None:
        backend = FasterWhisperBackend()
        with patch.dict(sys.modules, _install_fake_faster_whisper()):
            backend.load("8bit")
            text, segments = backend.transcribe(Path("audio.wav"), "VibeVoice\nTauri")

        # CPU path selected since fake reports 0 cuda devices.
        self.assertEqual(_FakeWhisperModel.last_init_kwargs["device"], "cpu")
        self.assertEqual(_FakeWhisperModel.last_init_kwargs["compute_type"], "int8")

        # hotwords passed through, language auto (None).
        self.assertEqual(_FakeWhisperModel.last_transcribe_kwargs["hotwords"], "VibeVoice\nTauri")
        self.assertIsNone(_FakeWhisperModel.last_transcribe_kwargs["language"])

        self.assertEqual(text, "Hello world")
        self.assertEqual(len(segments), 2)
        self.assertEqual(segments[0], {"start": 0.0, "end": 1.5, "speaker": None, "text": " Hello"})
        self.assertEqual(segments[1]["speaker"], None)

    def test_empty_prompt_becomes_no_hotwords(self) -> None:
        backend = FasterWhisperBackend()
        with patch.dict(sys.modules, _install_fake_faster_whisper()):
            backend.load("4bit")
            backend.transcribe(Path("audio.wav"), None)
        self.assertIsNone(_FakeWhisperModel.last_transcribe_kwargs["hotwords"])

    def test_unload_releases_model_reference(self) -> None:
        backend = FasterWhisperBackend()
        with patch.dict(sys.modules, _install_fake_faster_whisper()):
            backend.load("4bit")
        self.assertIsNotNone(backend.model)

        backend.unload()

        self.assertIsNone(backend.model)


if __name__ == "__main__":
    unittest.main()
