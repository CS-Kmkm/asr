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
    APP_MAX_RECORDING_SECONDS,
    VIBEVOICE_MAX_NEW_TOKENS,
    BackendError,
    FasterWhisperBackend,
    compute_max_new_tokens,
    create_backend,
    faster_whisper_compute_type,
    wav_duration_seconds,
)
from asr_worker.download import FASTER_WHISPER_ALLOW_PATTERNS
from asr_worker.tests.support import IsolatedWorkerTestCase


class CreateBackendTests(IsolatedWorkerTestCase):
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


class LoadUnavailableTests(IsolatedWorkerTestCase):
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


class ComputeTypeTests(IsolatedWorkerTestCase):
    def test_4bit_and_8bit(self) -> None:
        self.assertEqual(faster_whisper_compute_type("4bit", cuda=True), "int8_float16")
        self.assertEqual(faster_whisper_compute_type("4bit", cuda=False), "int8")
        self.assertEqual(faster_whisper_compute_type("8bit", cuda=True), "int8_float16")
        self.assertEqual(faster_whisper_compute_type("8bit", cuda=False), "int8")

    def test_bf16(self) -> None:
        self.assertEqual(faster_whisper_compute_type("bf16", cuda=True), "float16")
        self.assertEqual(faster_whisper_compute_type("bf16", cuda=False), "int8")


class MaxNewTokensTests(IsolatedWorkerTestCase):
    def test_short_audio_clamped_to_floor(self) -> None:
        self.assertEqual(compute_max_new_tokens(1.0, None), 256)

    def test_long_audio_estimate(self) -> None:
        # 60s -> 60*40 + 128 = 2528, within bounds
        self.assertEqual(compute_max_new_tokens(60.0, None), 2528)

    def test_long_audio_clamped_to_ceiling(self) -> None:
        self.assertEqual(compute_max_new_tokens(10000.0, None), VIBEVOICE_MAX_NEW_TOKENS)

    def test_ceiling_covers_the_app_recording_limit_with_a_minute_of_margin(self) -> None:
        self.assertEqual(APP_MAX_RECORDING_SECONDS, 15 * 60)
        # (900 + 60) s * 40 tok/s + 128
        self.assertEqual(VIBEVOICE_MAX_NEW_TOKENS, 38528)

    def test_full_length_recording_is_not_capped(self) -> None:
        # 15 min -> 900*40 + 128 = 36128, below the ceiling, so the
        # length-based estimate applies instead of a truncating cap.
        full = compute_max_new_tokens(float(APP_MAX_RECORDING_SECONDS), None)
        self.assertEqual(full, 36128)
        self.assertLess(full, VIBEVOICE_MAX_NEW_TOKENS)
        # The previous 4096 cap only fit about 99 s; a 5-minute take now
        # gets its whole estimate.
        self.assertEqual(compute_max_new_tokens(300.0, None), 12128)

    def test_override_still_caps_long_recordings(self) -> None:
        self.assertEqual(
            compute_max_new_tokens(float(APP_MAX_RECORDING_SECONDS), "4096"), 4096
        )

    def test_env_override_takes_priority(self) -> None:
        self.assertEqual(compute_max_new_tokens(60.0, "512"), 512)

    def test_env_override_ignored_when_not_positive(self) -> None:
        self.assertEqual(compute_max_new_tokens(1.0, "0"), 256)
        self.assertEqual(compute_max_new_tokens(1.0, "not-a-number"), 256)

    def test_none_audio_returns_ceiling(self) -> None:
        self.assertEqual(compute_max_new_tokens(None, None), VIBEVOICE_MAX_NEW_TOKENS)


class WavDurationTests(IsolatedWorkerTestCase):
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

    def transcribe(self, audio_path, language=None, hotwords=None, **kwargs):  # noqa: ANN001
        _FakeWhisperModel.last_transcribe_kwargs = {
            "audio_path": audio_path,
            "language": language,
            "hotwords": hotwords,
            **kwargs,
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


BUILTIN_REPO = "Systran/faster-whisper-tiny"


class ResolveModelFilesTests(IsolatedWorkerTestCase):
    def test_cached_model_is_loaded_without_downloading(self) -> None:
        backend = FasterWhisperBackend()
        events: list[dict] = []
        backend.progress = events.append
        with patch("asr_worker.backends.faster_whisper_repo_id", return_value="org/repo"), patch(
            "asr_worker.backends.cached_snapshot_path", return_value="/cache/repo"
        ), patch("asr_worker.backends.download_snapshot") as download:
            source = backend._resolve_model_files()

        self.assertEqual(source, "/cache/repo")
        download.assert_not_called()
        self.assertEqual(events, [])

    def test_missing_model_is_downloaded_with_progress(self) -> None:
        backend = FasterWhisperBackend()
        events: list[dict] = []
        backend.progress = events.append

        def fake_download(repo_id, allow_patterns, progress):  # noqa: ANN001
            progress({"completed_bytes": 5, "total_bytes": 10})
            return "/cache/repo"

        with patch("asr_worker.backends.faster_whisper_repo_id", return_value="org/repo"), patch(
            "asr_worker.backends.cached_snapshot_path", return_value=None
        ), patch("asr_worker.backends.download_snapshot", fake_download):
            source = backend._resolve_model_files()

        self.assertEqual(source, "/cache/repo")
        self.assertEqual(
            events,
            [
                {"stage": "download", "model": "org/repo"},
                {
                    "stage": "download",
                    "model": "org/repo",
                    "completed_bytes": 5,
                    "total_bytes": 10,
                },
            ],
        )

    def resolve_with_cached_files(
        self, files: list[str], repo_id: str = "org/repo"
    ) -> tuple[str, list[dict], list[dict]]:
        """Resolve against a fake hub cache whose snapshot folder holds ``files``.

        ``BUILTIN_REPO`` is the only repository faster-whisper maps a name to.
        """
        backend = FasterWhisperBackend()
        events: list[dict] = []
        backend.progress = events.append
        downloads: list[dict] = []

        def fake_download(repo_id, allow_patterns, progress):  # noqa: ANN001
            downloads.append({"repo_id": repo_id, "allow_patterns": allow_patterns})
            return "/downloaded/repo"

        with TemporaryDirectory() as tmp:
            snapshot = Path(tmp)
            for name in files:
                (snapshot / name).write_bytes(b"x")
            hub = types.ModuleType("huggingface_hub")
            hub.snapshot_download = lambda repo_id, **kwargs: str(snapshot)  # type: ignore[attr-defined]
            fw_utils = types.ModuleType("faster_whisper.utils")
            fw_utils._MODELS = {"tiny": BUILTIN_REPO}  # type: ignore[attr-defined]
            fake_modules = {
                "huggingface_hub": hub,
                "faster_whisper": types.ModuleType("faster_whisper"),
                "faster_whisper.utils": fw_utils,
            }
            with patch.dict(sys.modules, fake_modules), patch(
                "asr_worker.backends.faster_whisper_repo_id", return_value=repo_id
            ), patch("asr_worker.backends.download_snapshot", fake_download):
                source = backend._resolve_model_files()
                if source == str(snapshot):
                    source = "<cached snapshot>"
        return source, events, downloads

    def test_interrupted_first_download_is_downloaded_again(self) -> None:
        source, events, downloads = self.resolve_with_cached_files(
            ["config.json", "tokenizer.json", "vocabulary.json"]
        )

        self.assertEqual(source, "/downloaded/repo")
        self.assertEqual(downloads, [{"repo_id": "org/repo", "allow_patterns": FASTER_WHISPER_ALLOW_PATTERNS}])
        self.assertEqual(events, [{"stage": "download", "model": "org/repo"}])

    def test_complete_cached_snapshot_is_loaded_without_downloading(self) -> None:
        source, events, downloads = self.resolve_with_cached_files(
            ["config.json", "model.bin", "tokenizer.json", "vocabulary.json", "preprocessor_config.json"]
        )

        self.assertEqual(source, "<cached snapshot>")
        self.assertEqual(downloads, [])
        self.assertEqual(events, [])

    def test_builtin_model_without_tokenizer_is_downloaded_again(self) -> None:
        source, events, downloads = self.resolve_with_cached_files(
            ["config.json", "model.bin", "vocabulary.json"], BUILTIN_REPO
        )

        self.assertEqual(source, "/downloaded/repo")
        self.assertEqual(len(downloads), 1)
        self.assertEqual(events, [{"stage": "download", "model": BUILTIN_REPO}])

    def test_complete_builtin_snapshot_is_loaded_without_downloading(self) -> None:
        source, events, downloads = self.resolve_with_cached_files(
            ["config.json", "model.bin", "tokenizer.json", "vocabulary.txt"], BUILTIN_REPO
        )

        self.assertEqual(source, "<cached snapshot>")
        self.assertEqual(downloads, [])
        self.assertEqual(events, [])

    def test_custom_repository_without_tokenizer_counts_as_cached(self) -> None:
        # faster-whisper falls back to another tokenizer for such repositories,
        # so a missing tokenizer.json must not re-download on every load.
        source, events, downloads = self.resolve_with_cached_files(
            ["config.json", "model.bin", "vocabulary.json"], "org/custom-whisper"
        )

        self.assertEqual(source, "<cached snapshot>")
        self.assertEqual(downloads, [])
        self.assertEqual(events, [])

    def test_unresolvable_model_is_left_to_faster_whisper(self) -> None:
        backend = FasterWhisperBackend()
        events: list[dict] = []
        backend.progress = events.append
        with patch("asr_worker.backends.faster_whisper_repo_id", return_value=None):
            source = backend._resolve_model_files()

        self.assertEqual(source, backend.model_id)
        self.assertEqual(events, [])


class TranscribeWithFakeModuleTests(IsolatedWorkerTestCase):
    def setUp(self) -> None:
        super().setUp()
        # A fake CTranslate2 module must never resolve/download a real model.
        model_files = patch.object(FasterWhisperBackend, "_resolve_model_files", return_value="/fake/model")
        model_files.start()
        self.addCleanup(model_files.stop)

    def test_cpu_policy_never_queries_or_initializes_cuda(self) -> None:
        modules = _install_fake_faster_whisper()
        def unexpected_cuda_query():
            raise AssertionError("CPU mode must not query CUDA")
        modules["ctranslate2"].get_cuda_device_count = unexpected_cuda_query
        with patch.dict(os.environ, {"ASR_DEVICE": "cpu"}), patch.dict(sys.modules, modules):
            backend = FasterWhisperBackend()
            backend.load("bf16")
        self.assertEqual(backend.device, "cpu")
        self.assertEqual(_FakeWhisperModel.last_init_kwargs["compute_type"], "int8")

    def test_invalid_device_policy_is_actionable(self) -> None:
        with patch.dict(os.environ, {"ASR_DEVICE": "invalid"}):
            with self.assertRaises(BackendError) as caught:
                FasterWhisperBackend()
        self.assertEqual(caught.exception.code, "invalid_device")

    def test_auto_uses_cpu_when_cuda_discovery_raises(self) -> None:
        modules = _install_fake_faster_whisper()
        def failed_probe():
            raise RuntimeError("CUDA driver unavailable")
        modules["ctranslate2"].get_cuda_device_count = failed_probe
        events = []
        backend = FasterWhisperBackend()
        backend.progress = events.append
        with patch.dict(sys.modules, modules):
            backend.load("4bit")
        self.assertEqual(backend.device, "cpu")
        self.assertEqual(_FakeWhisperModel.last_init_kwargs["compute_type"], "int8")
        self.assertIn({"stage": "cpu_fallback"}, events)

    def test_auto_retries_cpu_when_lazy_cuda_warmup_fails(self) -> None:
        devices: list[str] = []
        warmups: list[tuple[str, int]] = []
        class FailingCudaModel(_FakeWhisperModel):
            def __init__(self, model_id, device, compute_type):
                super().__init__(model_id, device, compute_type)
                self.device = device
                devices.append(device)

            def transcribe(self, audio, **kwargs):
                def decode():
                    warmups.append((self.device, len(audio)))
                    if self.device == "cuda":
                        raise RuntimeError("missing CUDA DLL")
                    yield _FakeSegment(0, 0.5, "discard this warmup")
                return decode(), None

        modules = _install_fake_faster_whisper()
        modules["faster_whisper"].WhisperModel = FailingCudaModel
        modules["ctranslate2"].get_cuda_device_count = lambda: 1
        backend = FasterWhisperBackend()
        events: list[dict] = []
        backend.progress = events.append
        with patch.dict(sys.modules, modules):
            backend.load("4bit")
        self.assertEqual(devices, ["cuda", "cpu"])
        self.assertEqual(warmups, [("cuda", 8_000), ("cpu", 8_000)])
        self.assertEqual(backend.device, "cpu")
        self.assertEqual(backend.compute_type, "int8")
        self.assertIn({"stage": "cpu_fallback"}, events)

        devices.clear()
        with patch.dict(os.environ, {"ASR_DEVICE": "cuda"}), patch.dict(sys.modules, modules):
            forced = FasterWhisperBackend()
            with self.assertRaises(BackendError):
                forced.load("8bit")
        self.assertEqual(devices, ["cuda"])
        self.assertIsNone(forced.model)

    def test_transcribe_formats_segments_and_passes_hotwords(self) -> None:
        backend = create_backend("faster-whisper")
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

    def test_fast_profile_is_explicit_and_standard_keeps_library_decode_defaults(self):
        for profile in ["standard", "fast"]:
            with self.subTest(profile=profile), patch.dict(os.environ, {"ASR_WHISPER_DECODE_PROFILE": profile}), patch.dict(sys.modules, _install_fake_faster_whisper()):
                backend = FasterWhisperBackend()
                backend.load("4bit")
                backend.transcribe(Path("audio.wav"), "dictionary", "ja-JP")
                options = _FakeWhisperModel.last_transcribe_kwargs
                self.assertEqual(options["language"], "ja")
                self.assertEqual(options["hotwords"], "dictionary")
                if profile == "fast":
                    self.assertEqual(options["beam_size"], 1)
                    self.assertTrue(options["vad_filter"])
                else:
                    self.assertNotIn("beam_size", options)
                    self.assertNotIn("vad_filter", options)

    def test_unknown_decode_profile_is_rejected_before_loading(self):
        with patch.dict(os.environ, {"ASR_WHISPER_DECODE_PROFILE": "unknown"}):
            with self.assertRaises(BackendError) as caught:
                FasterWhisperBackend()
        self.assertEqual(caught.exception.code, "invalid_decode_profile")

    def test_regional_locale_uses_supported_base_language(self) -> None:
        backend = FasterWhisperBackend()
        with patch.dict(sys.modules, _install_fake_faster_whisper()):
            backend.load("4bit")
            backend.transcribe(Path("audio.wav"), None, "en-GB")
        self.assertEqual(_FakeWhisperModel.last_transcribe_kwargs["language"], "en")

    def test_unload_releases_model_reference(self) -> None:
        backend = FasterWhisperBackend()
        with patch.dict(sys.modules, _install_fake_faster_whisper()):
            backend.load("4bit")
        self.assertIsNotNone(backend.model)

        backend.unload()

        self.assertIsNone(backend.model)


class InferenceFallbackTests(IsolatedWorkerTestCase):
    def loaded(self, cuda_decode, cpu_decode=None, policy="auto"):
        calls = []
        initialized = []

        class InferenceModel(_FakeWhisperModel):
            def __init__(self, model_id, device, compute_type):
                super().__init__(model_id, device, compute_type)
                self.device = device
                initialized.append((model_id, device, compute_type))

            def transcribe(self, audio, **kwargs):
                if not isinstance(audio, str):
                    return super().transcribe(audio, **kwargs)
                calls.append((self.device, audio, kwargs))
                decode = cuda_decode if self.device == "cuda" else cpu_decode
                if decode is None:
                    return iter([_FakeSegment(0, 1, " CPU result")]), None
                return decode(), None

        modules = _install_fake_faster_whisper()
        modules["faster_whisper"].WhisperModel = InferenceModel
        modules["ctranslate2"].get_cuda_device_count = lambda: 1
        self.enterContext(patch.dict(os.environ, {"ASR_DEVICE": policy}))
        self.enterContext(patch.dict(sys.modules, modules))
        resolved = self.enterContext(patch.object(FasterWhisperBackend, "_resolve_model_files", return_value="/fake/pinned-snapshot"))
        backend = FasterWhisperBackend()
        backend.load("8bit")
        return backend, calls, initialized, resolved

    @staticmethod
    def cuda_failure():
        raise RuntimeError("CUDA out of memory")

    def test_caught_first_inference_failure_retries_same_input_on_cpu_once(self):
        backend, calls, initialized, resolved = self.loaded(self.cuda_failure)
        text, segments = backend.transcribe(Path("audio.wav"), "dictionary", "ja-JP")
        self.assertEqual(text, "CPU result")
        self.assertEqual(len(segments), 1)
        self.assertEqual([call[0] for call in calls], ["cuda", "cpu"])
        self.assertEqual(calls[0][1:], calls[1][1:])
        self.assertEqual(initialized, [("/fake/pinned-snapshot", "cuda", "int8_float16"), ("/fake/pinned-snapshot", "cpu", "int8")])
        resolved.assert_called_once()
        self.assertEqual((backend.device, backend.compute_type), ("cpu", "int8"))
        backend.transcribe(Path("next.wav"), None)
        self.assertEqual([call[0] for call in calls], ["cuda", "cpu", "cpu"])

    def test_lazy_cuda_failure_before_a_segment_also_retries(self):
        def lazy_failure():
            raise RuntimeError("Could not locate cudnn_ops64 DLL")
            yield  # pragma: no cover
        backend, calls, initialized, _ = self.loaded(lazy_failure)
        self.assertEqual(backend.transcribe(Path("audio.wav"), None)[0], "CPU result")
        self.assertEqual(len(calls), 2)
        self.assertEqual(len(initialized), 2)

    def test_fast_options_survive_the_cpu_inference_retry(self):
        with patch.dict(os.environ, {"ASR_WHISPER_DECODE_PROFILE": "fast"}):
            backend, calls, _, _ = self.loaded(self.cuda_failure)
        backend.transcribe(Path("audio.wav"), "dictionary", "ja-JP")
        self.assertEqual([call[0] for call in calls], ["cuda", "cpu"])
        self.assertTrue(all(call[2]["beam_size"] == 1 and call[2]["vad_filter"] for call in calls))
        self.assertEqual(calls[0][1:], calls[1][1:])

    def test_failure_after_publishing_a_segment_never_mixes_in_cpu_output(self):
        def partial_failure():
            yield _FakeSegment(0, 1, " GPU prefix")
            raise RuntimeError("CUDA kernel failed")
        backend, calls, initialized, _ = self.loaded(partial_failure)
        segments = backend.transcribe_segments(Path("audio.wav"), None)
        self.assertEqual(next(segments)["text"], " GPU prefix")
        with self.assertRaises(BackendError):
            next(segments)
        self.assertEqual(len(calls), 1)
        self.assertEqual(len(initialized), 1)
        self.assertEqual(backend.device, "cuda")

    def test_explicit_cuda_and_unrelated_errors_never_fall_back(self):
        for policy, message in [("cuda", "CUDA kernel failed"), ("auto", "invalid audio format")]:
            with self.subTest(policy=policy, message=message):
                def fail():
                    raise RuntimeError(message)
                backend, calls, initialized, _ = self.loaded(fail, policy=policy)
                with self.assertRaises(BackendError):
                    backend.transcribe(Path("audio.wav"), None)
                self.assertEqual(len(calls), 1)
                self.assertEqual(len(initialized), 1)

    def test_cancelled_cuda_failure_never_initializes_cpu(self):
        cancelled = False
        def fail():
            nonlocal cancelled
            cancelled = True
            raise RuntimeError("CUDA kernel failed")
        backend, calls, initialized, _ = self.loaded(fail)
        backend.cancel_requested = lambda: cancelled
        with self.assertRaises(BackendError) as caught:
            backend.transcribe(Path("audio.wav"), None)
        self.assertEqual(caught.exception.code, "cancelled")
        self.assertEqual(len(initialized), 1)
        self.assertEqual(len(calls), 1)

    def test_cpu_decode_failure_is_not_retried_again(self):
        def cpu_failure():
            raise RuntimeError("CPU decode failed")
        backend, calls, initialized, _ = self.loaded(self.cuda_failure, cpu_failure)
        with self.assertRaises(BackendError):
            backend.transcribe(Path("audio.wav"), None)
        self.assertEqual(len(calls), 2)
        self.assertEqual(len(initialized), 2)
        self.assertEqual(backend.device, "cpu")

    def test_cpu_retry_oom_never_claims_gpu_memory_exhaustion(self):
        def oom():
            raise RuntimeError("out of memory")
        backend, calls, _, _ = self.loaded(self.cuda_failure, oom)
        with self.assertRaises(BackendError) as caught:
            backend.transcribe(Path("audio.wav"), None)
        self.assertEqual(caught.exception.code, "cpu_oom")
        self.assertNotIn("GPU", caught.exception.message)
        self.assertEqual(len(calls), 2)

    def test_cpu_constructor_and_warmup_oom_restore_the_previous_model(self):
        for phase in ["constructor", "warmup"]:
            with self.subTest(phase=phase):
                backend, calls, _, _ = self.loaded(self.cuda_failure)
                previous = backend.model
                patcher = patch("faster_whisper.WhisperModel", side_effect=RuntimeError("out of memory")) if phase == "constructor" else patch.object(backend, "_warm_up", side_effect=RuntimeError("out of memory"))
                with patcher:
                    with self.assertRaises(BackendError) as caught:
                        backend.transcribe(Path("audio.wav"), None)
                self.assertEqual(caught.exception.code, "cpu_oom")
                self.assertIs(backend.model, previous)
                self.assertEqual(backend.device, "cuda")
                self.assertEqual(len(calls), 1)

    def test_cancel_during_cpu_warmup_restores_previous_device_without_decoding(self):
        backend, calls, initialized, _ = self.loaded(self.cuda_failure)
        previous = backend.model
        def cancel():
            backend.cancel_requested = lambda: True
        with patch.object(backend, "_warm_up", side_effect=cancel):
            with self.assertRaises(BackendError) as caught:
                backend.transcribe(Path("audio.wav"), None)
        self.assertEqual(caught.exception.code, "cancelled")
        self.assertIs(backend.model, previous)
        self.assertEqual(backend.device, "cuda")
        self.assertEqual(len(calls), 1)
        self.assertEqual(len(initialized), 2)

    def test_consumer_close_does_not_trigger_cpu_retry(self):
        backend, calls, initialized, _ = self.loaded(lambda: iter([_FakeSegment(0, 1, " result")]))
        segments = backend.transcribe_segments(Path("audio.wav"), None)
        next(segments)
        segments.close()
        self.assertEqual(len(calls), 1)
        self.assertEqual(len(initialized), 1)

    def test_worker_health_reports_the_cpu_used_after_inference_retry(self):
        from asr_worker.worker import Worker

        backend, _, _, _ = self.loaded(self.cuda_failure)
        worker = Worker(backend)
        worker.loaded = True
        with TemporaryDirectory() as tmp:
            audio = Path(tmp) / "fake.wav"
            audio.write_bytes(b"fake input for model stub")
            response, _ = worker.handle({"id": 1, "command": "transcribe", "audio_path": str(audio)})
        self.assertTrue(response["ok"])
        health, _ = worker.handle({"id": 2, "command": "health"})
        self.assertTrue(health["health"]["loaded"])
        self.assertEqual(health["health"]["device"], "cpu")
        self.assertEqual(health["health"]["computeType"], "int8")


if __name__ == "__main__":
    unittest.main()
