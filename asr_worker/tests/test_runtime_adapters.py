"""Runtime adapter lifecycle (issue #87) with fake runtimes: no network, no GPU."""
from __future__ import annotations

import hashlib
import json
import os
import sys
import tempfile
import types
import wave
from pathlib import Path
from typing import Any
from unittest.mock import patch

import numpy as np

from asr_worker import runtime_adapter as ra
from asr_worker import sherpa_onnx_runtime as so
from asr_worker import transformers_runtime as tr
from asr_worker.backends import BackendError, create_backend
from asr_worker.tests.support import IsolatedWorkerTestCase
from asr_worker.worker import Worker


def pinned(name: str, content: bytes) -> ra.PinnedFile:
    return ra.PinnedFile(name, hashlib.sha256(content).hexdigest(), len(content))


CONTENT = {"weights.bin": b"weights-v1", "tokens.txt": b"a 0\nb 1\n"}
FAKE_MODEL = ra.ManagedModel(
    id="fake-ja", display_name="Fake JA", repo="example/fake", revision="0" * 40,
    files=tuple(pinned(name, data) for name, data in CONTENT.items()),
    license="Apache-2.0", japanese_only=True, kind="fake",
)


class FakeAdapter(ra.RuntimeAdapter):
    backend_name = "fake-runtime"
    display_label = "Fake runtime"
    install_extra = "fake-extra"
    models = {FAKE_MODEL.id: FAKE_MODEL}
    default_model = FAKE_MODEL.id

    def __init__(self) -> None:
        super().__init__()
        self.opened: dict[str, Path] | None = None
        self.missing_module = False
        self.chunks_seen = 0

    def _import_runtime(self) -> None:
        if self.missing_module:
            raise ModuleNotFoundError("No module named 'fake_runtime'", name="fake_runtime")

    def _open(self, model: ra.ManagedModel, paths: dict[str, Path]) -> None:
        self.opened = paths
        self.device, self.compute_type, self.runtime_label = "cpu", "float32", "fake 1.0 / CPU"

    def _recognize(self, samples: Any, rate: int, language: str | None, prompt: str | None):
        for _ in ra.split_on_quiet(samples, rate, 1.0):
            self.raise_if_cancelled()
            self.chunks_seen += 1
        return "テスト", [{"start": 0.0, "end": len(samples) / rate, "speaker": None, "text": "テスト"}]

    def _close(self) -> None:
        self.opened = None


def write_wav(path: Path, samples: np.ndarray, rate: int = 16_000, channels: int = 1) -> Path:
    data = (np.clip(samples, -1, 1) * 32767).astype("<i2").tobytes()
    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(channels)
        handle.setsampwidth(2)
        handle.setframerate(rate)
        handle.writeframes(data)
    return path


class RuntimeAdapterTestCase(IsolatedWorkerTestCase):
    def setUp(self) -> None:
        super().setUp()
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.root = Path(folder.name)
        os.environ["ASR_MANAGED_MODEL_HOME"] = str(self.root / "models")

    def publish(self, model: ra.ManagedModel = FAKE_MODEL, runtime: str = "fake-runtime",
                content: dict[str, bytes] = CONTENT) -> Path:
        folder = ra.model_directory(runtime, model)
        folder.mkdir(parents=True, exist_ok=True)
        for name, data in content.items():
            (folder / name).write_bytes(data)
        return folder

    def no_network(self):
        return patch("httpx.Client", side_effect=AssertionError("network must not be used"))


class LifecycleTests(RuntimeAdapterTestCase):
    def test_cached_pinned_files_load_without_network_and_unload_clears_runtime(self):
        folder = self.publish()
        backend = FakeAdapter()
        stages: list[str] = []
        backend.progress = lambda update: stages.append(update["stage"])
        with self.no_network():
            backend.load("4bit")
        self.assertEqual(backend.opened, {name: folder / name for name in CONTENT})
        self.assertEqual(stages, ["load"])
        self.assertEqual((backend.device, backend.runtime_label), ("cpu", "fake 1.0 / CPU"))
        backend.unload()
        self.assertIsNone(backend.opened)
        self.assertEqual((backend.device, backend.compute_type, backend.runtime_label, backend.model), (None, None, None, None))
        with self.assertRaises(BackendError) as caught:
            backend.transcribe(self.root / "absent.wav", None, "ja")
        self.assertEqual(caught.exception.code, "model_not_loaded")

    def test_unknown_model_id_is_rejected_before_any_download(self):
        os.environ["ASR_MODEL_ID"] = "Qwen/unpinned-model"
        backend = FakeAdapter()
        with self.no_network(), self.assertRaises(BackendError) as caught:
            backend.load("4bit")
        self.assertEqual(caught.exception.code, "invalid_model")
        self.assertFalse((self.root / "models").exists())

    def test_invalid_device_policy_is_a_startup_error(self):
        os.environ["ASR_DEVICE"] = "vulkan"
        with self.assertRaises(BackendError) as caught:
            FakeAdapter()
        self.assertEqual(caught.exception.code, "invalid_device")

    def test_missing_runtime_package_names_the_extra_to_install(self):
        backend = FakeAdapter()
        backend.missing_module = True
        with self.no_network(), self.assertRaises(BackendError) as caught:
            backend.load("4bit")
        self.assertEqual(caught.exception.code, "backend_unavailable")
        self.assertIn("uv sync --extra fake-extra", caught.exception.message)
        self.assertIsNone(backend.model)

    def test_real_adapters_report_missing_packages_as_backend_unavailable(self):
        for module, factory, extra in [
            ("sherpa_onnx", so.SherpaOnnxBackend, "sherpa-onnx"),
            ("transformers", tr.TransformersAsrBackend, "transformers-asr"),
        ]:
            with self.subTest(module=module), patch.dict(sys.modules, {module: None}), self.no_network():
                backend = factory()
                with self.assertRaises(BackendError) as caught:
                    backend.load("4bit")
                self.assertEqual(caught.exception.code, "backend_unavailable")
                self.assertIn(extra, caught.exception.message)

    def test_mismatching_published_file_is_preserved_and_never_redownloaded(self):
        folder = self.publish(content={**CONTENT, "weights.bin": b"tampered!!"})
        backend = FakeAdapter()
        with self.no_network(), self.assertRaises(BackendError) as caught:
            backend.load("4bit")
        self.assertEqual(caught.exception.code, "model_artifact_invalid")
        self.assertEqual((folder / "weights.bin").read_bytes(), b"tampered!!")

    def test_download_verifies_before_publication_and_reports_bytes(self):
        import httpx

        served = dict(CONTENT)
        requests: list[str] = []

        def handle(request: httpx.Request) -> httpx.Response:
            requests.append(str(request.url))
            return httpx.Response(200, content=served[request.url.path.rsplit("/", 1)[1]])

        real_client = httpx.Client

        def client(**kwargs: Any) -> httpx.Client:
            kwargs.pop("verify", None)
            return real_client(transport=httpx.MockTransport(handle), **kwargs)

        backend = FakeAdapter()
        progress: list[dict[str, Any]] = []
        backend.progress = progress.append
        with patch("httpx.Client", side_effect=client):
            backend.load("4bit")
        self.assertEqual(len(requests), 2)
        self.assertTrue(all(f"/example/fake/resolve/{'0' * 40}/" in url for url in requests))
        downloads = [item for item in progress if item["stage"] == "download"]
        self.assertEqual(downloads[-1]["completed_bytes"], sum(len(data) for data in CONTENT.values()))
        self.assertEqual(downloads[-1]["total_bytes"], downloads[-1]["completed_bytes"])
        backend.unload()

        # A corrupted download is never published under the final name.
        folder = ra.model_directory("fake-runtime", FAKE_MODEL)
        (folder / "weights.bin").unlink()
        served["weights.bin"] = b"corrupted!"
        with patch("httpx.Client", side_effect=client), self.assertRaises(BackendError) as caught:
            FakeAdapter().load("4bit")
        self.assertEqual(caught.exception.code, "model_artifact_invalid")
        self.assertFalse((folder / "weights.bin").exists())
        self.assertEqual([path.name for path in folder.glob(".download-*")], [])

    def test_network_failure_maps_to_download_error(self):
        import httpx

        backend = FakeAdapter()
        with patch("httpx.Client", side_effect=httpx.ConnectError("offline")), self.assertRaises(BackendError) as caught:
            backend.load("4bit")
        self.assertEqual(caught.exception.code, "hf_download_failed")

    def test_cancel_stops_download_and_transcription_between_chunks(self):
        import httpx

        real_client = httpx.Client

        def client(**kwargs: Any) -> httpx.Client:
            kwargs.pop("verify", None)
            return real_client(transport=httpx.MockTransport(lambda request: httpx.Response(200, content=b"x" * 64)), **kwargs)

        backend = FakeAdapter()
        backend.cancel_requested = lambda: True
        with patch("httpx.Client", side_effect=client), self.assertRaises(BackendError) as caught:
            backend.load("4bit")
        self.assertEqual(caught.exception.code, "cancelled")
        folder = ra.model_directory("fake-runtime", FAKE_MODEL)
        self.assertEqual(list(folder.glob("*")), [])

        self.publish()
        backend = FakeAdapter()
        backend.load("4bit")
        audio = write_wav(self.root / "long.wav", np.zeros(16_000 * 3, dtype=np.float32))
        calls = iter([False, False, True])
        backend.cancel_requested = lambda: next(calls, True)
        with self.assertRaises(BackendError) as caught:
            backend.transcribe(audio, None, "ja-JP")
        self.assertEqual(caught.exception.code, "cancelled")
        self.assertLess(backend.chunks_seen, 3)

    def test_japanese_only_model_rejects_other_languages_before_reading_audio(self):
        self.publish()
        backend = FakeAdapter()
        backend.load("4bit")
        with self.assertRaises(BackendError) as caught:
            backend.transcribe(self.root / "never-read.wav", None, "en-US")
        self.assertEqual(caught.exception.code, "language_unsupported")
        audio = write_wav(self.root / "ok.wav", np.zeros(8_000, dtype=np.float32))
        self.assertEqual(backend.transcribe(audio, None, "ja_JP")[0], "テスト")
        self.assertEqual(backend.transcribe(audio, None, None)[0], "テスト")

    def test_worker_health_reports_actual_runtime_only_while_loaded(self):
        self.publish()
        worker = Worker(FakeAdapter())
        response, _ = worker.handle({"id": 1, "command": "load"})
        self.assertTrue(response["ok"], response)
        health = worker.handle({"id": 2, "command": "health"})[0]["health"]
        self.assertEqual((health["device"], health["runtime"]), ("cpu", "fake 1.0 / CPU"))
        worker.handle({"id": 3, "command": "shutdown"})
        health = worker.handle({"id": 4, "command": "health"})[0]["health"]
        self.assertNotIn("runtime", health)
        self.assertIsNone(health["device"])

    def test_create_backend_builds_both_new_runtimes_with_their_defaults(self):
        sherpa = create_backend("sherpa-onnx")
        qwen = create_backend("transformers-asr")
        self.assertEqual(sherpa.model_name, "sherpa-onnx:reazonspeech-k2-v2")
        self.assertEqual(qwen.model_name, "transformers-asr:qwen3-asr-0.6b")
        self.assertTrue(callable(sherpa.preload_runtime) and callable(qwen.preload_runtime))


class AudioTests(RuntimeAdapterTestCase):
    def test_read_wav_downmixes_and_rejects_non_pcm(self):
        stereo = np.array([[0.5, -0.5], [0.25, 0.25]], dtype=np.float32).reshape(-1)
        path = write_wav(self.root / "stereo.wav", stereo, rate=24_000, channels=2)
        samples, rate = ra.read_wav(path)
        self.assertEqual(rate, 24_000)
        np.testing.assert_allclose(samples, [0.0, 0.25], atol=1e-3)
        bogus = self.root / "bogus.wav"
        bogus.write_bytes(b"ID3 not a wav")
        with self.assertRaises(BackendError) as caught:
            ra.read_wav(bogus)
        self.assertEqual(caught.exception.code, "audio_unsupported")

    def test_long_audio_splits_at_the_quietest_frame_within_the_limit(self):
        rate = 1_000
        samples = np.ones(rate * 25, dtype=np.float32)
        samples[8_000:8_100] = 0.0  # a pause inside the search window of the first cut
        chunks = ra.split_on_quiet(samples, rate, max_seconds=10.0, search_seconds=4.0)
        self.assertEqual(sum(len(chunk) for chunk in chunks), len(samples))
        self.assertTrue(all(len(chunk) <= 10 * rate for chunk in chunks))
        self.assertEqual(len(chunks[0]), 8_050)
        self.assertEqual(ra.split_on_quiet(samples[:5_000], rate, 10.0)[0].shape, (5_000,))

    def test_native_stderr_is_captured_and_still_logged(self):
        with ra.captured_native_stderr() as log:
            os.write(2, b"Fallback to cpu\n")
        self.assertIn("Fallback to cpu", "".join(log))


def hide_torch(case: IsolatedWorkerTestCase) -> None:
    """Make this process look torch-free for sherpa CUDA paths; restored after."""
    saved = sys.modules.pop("torch", None)
    if saved is not None:
        case.addCleanup(sys.modules.__setitem__, "torch", saved)


class FakeRecognizer:
    """Returns the next scripted text for each decoded stream."""

    def __init__(self, texts: list[str] | None = None) -> None:
        self.texts = texts or [""]
        self.decoded = 0

    def create_stream(self) -> Any:
        return types.SimpleNamespace(accept_waveform=lambda rate, samples: None,
                                     result=types.SimpleNamespace(text=""))

    def decode_stream(self, stream: Any) -> None:
        stream.result.text = self.texts[self.decoded % len(self.texts)]
        self.decoded += 1


class SherpaOnnxTests(RuntimeAdapterTestCase):
    def setUp(self) -> None:
        super().setUp()
        hide_torch(self)
        self.sherpa = types.SimpleNamespace(__version__="2.0.1+test", onnxruntime_version="1.24.4")

    def backend(self, policy: str) -> so.SherpaOnnxBackend:
        os.environ["ASR_DEVICE"] = policy
        backend = so.SherpaOnnxBackend()
        backend.sherpa = self.sherpa
        return backend

    def open(self, backend: so.SherpaOnnxBackend, *, log: str = "", preload: bool = True,
             loaded: bool = True, context: tuple[bool | None, str | None] = (True, "NVIDIA GeForce RTX 3060")):
        built: list[str] = []

        def build(model: Any, paths: Any, provider: str) -> tuple[Any, str]:
            built.append(provider)
            return FakeRecognizer(), log if provider == "cuda" else ""

        stages: list[str] = []
        backend.progress = lambda update: stages.append(update["stage"])
        with patch.object(backend, "_build", side_effect=build), \
             patch.object(so, "preload_cuda_libraries", return_value=preload), \
             patch.object(so, "module_loaded", return_value=loaded), \
             patch.object(so, "cuda_context_gpu", return_value=context):
            backend._open(so.REAZONSPEECH_K2_V2, {})
        return built, stages

    def test_cuda_is_reported_only_after_the_provider_verifiably_ran(self):
        backend = self.backend("cuda")
        built, _ = self.open(backend)
        self.assertEqual(built, ["cuda"])
        self.assertEqual((backend.device, backend.device_name), ("cuda", "NVIDIA GeForce RTX 3060"))
        self.assertIn("CUDAExecutionProvider", backend.runtime_label)

    def test_explicit_cuda_fails_instead_of_running_silently_on_cpu(self):
        for kwargs in [{"log": "Failed to enable CUDA ... Fallback to cpu"}, {"preload": False},
                       {"loaded": False}, {"context": (False, None)}]:
            with self.subTest(**kwargs):
                backend = self.backend("cuda")
                with self.assertRaises(BackendError) as caught:
                    self.open(backend, **kwargs)
                self.assertEqual(caught.exception.code, "cuda_unavailable")
                self.assertIsNone(backend.device)

    def test_auto_falls_back_to_cpu_and_reports_cpu(self):
        backend = self.backend("auto")
        built, stages = self.open(backend, log="Fallback to cpu")
        self.assertEqual(built, ["cuda", "cpu"])
        self.assertIn("cpu_fallback", stages)
        self.assertEqual(backend.device, "cpu")
        self.assertIsNone(backend.device_name)
        self.assertIn("CPUExecutionProvider", backend.runtime_label)

    def test_unknown_gpu_context_check_does_not_block_other_evidence(self):
        backend = self.backend("cuda")
        self.open(backend, context=(None, None))
        self.assertEqual(backend.device, "cuda")

    def test_cpu_policy_never_touches_cuda(self):
        backend = self.backend("cpu")
        with patch.object(so, "preload_cuda_libraries", side_effect=AssertionError("CUDA probed")):
            built, _ = self.open(backend)
        self.assertEqual(built, ["cpu"])
        self.assertEqual(backend.device, "cpu")

    def test_cuda_out_of_memory_maps_to_gpu_oom_when_cuda_is_required(self):
        backend = self.backend("cuda")
        with patch.object(backend, "_build", side_effect=RuntimeError("CUDA failure 2: out of memory")), \
             patch.object(so, "preload_cuda_libraries", return_value=True), \
             self.assertRaises(BackendError) as caught:
            backend._open(so.REAZONSPEECH_K2_V2, {})
        self.assertEqual(caught.exception.code, "gpu_oom")

    def test_long_recordings_are_decoded_in_chunks_and_joined_without_spaces(self):
        backend = self.backend("cpu")
        backend.recognizer = FakeRecognizer(["今日は", "晴れです"])
        samples = np.ones(16_000 * 40, dtype=np.float32) * 0.1
        text, segments = backend._recognize(samples, 16_000, "ja", None)
        self.assertEqual(backend.recognizer.decoded, 2)
        self.assertEqual(text, "今日は晴れです")
        self.assertEqual(len(segments), 2)
        self.assertLessEqual(segments[0]["end"], so.MAX_CHUNK_SECONDS)


class TransformersAsrTests(RuntimeAdapterTestCase):
    def fake_modules(self, cuda: bool) -> dict[str, Any]:
        torch = types.ModuleType("torch")
        torch.__version__ = "2.11.0+test"
        torch.float32, torch.bfloat16 = "float32", "bfloat16"
        torch.cuda = types.SimpleNamespace(is_available=lambda: cuda, get_device_name=lambda device: "Fake GPU")
        transformers = types.ModuleType("transformers")
        transformers.__version__ = "5.15.1"
        transformers.AutoProcessor = object
        transformers.Qwen3ASRForConditionalGeneration = object
        return {"torch": torch, "transformers": transformers}

    def test_cuda_is_required_unless_cpu_is_selected(self):
        for policy in ["auto", "cuda"]:
            with self.subTest(policy=policy), patch.dict(sys.modules, self.fake_modules(cuda=False)):
                os.environ["ASR_DEVICE"] = policy
                backend = tr.TransformersAsrBackend()
                with self.assertRaises(BackendError) as caught:
                    backend._open(tr.QWEN3_ASR_0_6B, {"config.json": self.root / "config.json"})
                self.assertEqual(caught.exception.code, "gpu_unsupported")
                self.assertIsNone(backend.network)

    def test_load_failure_unloads_and_maps_gpu_oom(self):
        self.publish(tr.QWEN3_ASR_0_6B, "transformers-asr", {})  # folder only
        backend = tr.TransformersAsrBackend()
        with patch.object(ra, "fetch_model", return_value={"config.json": self.root / "config.json"}), \
             patch.object(backend, "_import_runtime"), \
             patch.object(backend, "_open", side_effect=RuntimeError("CUDA out of memory. Tried to allocate")), \
             self.assertRaises(BackendError) as caught:
            backend.load("4bit")
        self.assertEqual(caught.exception.code, "gpu_oom")
        self.assertIsNone(backend.model)

    def test_dictionary_prompt_and_language_reach_the_processor(self):
        backend = tr.TransformersAsrBackend()
        backend.model = tr.QWEN3_ASR_0_6B
        calls: list[tuple] = []
        backend._generate = lambda samples, language, prompt, max_new_tokens: calls.append((len(samples), language, prompt, max_new_tokens)) or "こんにちは"
        audio = write_wav(self.root / "a.wav", np.zeros(16_000, dtype=np.float32))
        text, segments = backend.transcribe(audio, "固有名詞", "ja-JP")
        self.assertEqual(text, "こんにちは")
        self.assertEqual(calls[0][:3], (16_000, "ja", "固有名詞"))
        self.assertEqual(segments[0]["end"], 1.0)
        # Qwen3-ASR is multilingual: other languages are passed through.
        backend.transcribe(audio, None, "en-US")
        self.assertEqual(calls[1][1], "en")


class SharedCatalogRuntimeTests(IsolatedWorkerTestCase):
    def test_catalog_matches_runtime_adapter_pins(self):
        catalog_path = Path(__file__).resolve().parents[2] / "src" / "asr-model-catalog.json"
        catalog = json.loads(catalog_path.read_text(encoding="utf-8"))
        self.assertEqual(catalog["runtimes"]["sherpa-onnx"]["version"], so.SHERPA_ONNX_VERSION)
        for backend, models in [("sherpa-onnx", so.MODELS), ("transformers-asr", tr.MODELS)]:
            entries = {entry["modelId"]: entry for entry in catalog["models"]
                       if entry["backend"] == backend and entry["availability"] == "available"}
            self.assertEqual(set(entries), set(models))
            for model_id, model in models.items():
                source = entries[model_id]["source"]
                self.assertEqual(entries[model_id]["license"], model.license)
                self.assertEqual(entries[model_id]["languageScope"], "japanese_only" if model.japanese_only else "multilingual")
                self.assertEqual((source["repo"], source["revision"], source["sizeBytes"]), (model.repo, model.revision, model.size))
                self.assertEqual(source["files"], [{"file": f.name, "sha256": f.sha256, "sizeBytes": f.size} for f in model.files])


class FailedLoadReleaseTests(RuntimeAdapterTestCase):
    """A failed warm-up must not keep tensors alive through its traceback."""

    def probe_adapter(self, error: BaseException):
        import weakref

        class Tensor:
            pass

        probes: list[Any] = []
        alive_at_close: list[bool] = []

        class Failing(FakeAdapter):
            def _open(self, model: ra.ManagedModel, paths: dict[str, Path]) -> None:
                self.device = "cuda"
                activations = Tensor()  # held only by this frame's locals
                probes.append(weakref.ref(activations))
                raise error

            def _close(self) -> None:
                alive_at_close.extend(probe() is not None for probe in probes)

        return Failing(), probes, alive_at_close

    def test_oom_and_cancel_release_frame_references_before_unload(self):
        self.publish()
        for error, code in [(RuntimeError("CUDA out of memory. Tried to allocate"), "gpu_oom"),
                            (BackendError("cancelled", "Transcription was cancelled"), "cancelled")]:
            with self.subTest(code=code):
                backend, probes, alive_at_close = self.probe_adapter(error)
                with self.assertRaises(BackendError) as caught:
                    backend.load("4bit")
                self.assertEqual(caught.exception.code, code)
                self.assertEqual(alive_at_close, [False])
                self.assertIsNone(probes[0]())
                self.assertIsNone(backend.device)


class DecisionConflictTests(RuntimeAdapterTestCase):
    def setUp(self) -> None:
        super().setUp()
        hide_torch(self)

    def test_worker_refuses_decision_model_after_sherpa_cuda_libraries_loaded(self):
        backend = so.SherpaOnnxBackend()
        worker = Worker(backend)
        with patch.object(worker.decision, "load", side_effect=AssertionError("must not load")):
            backend.excludes_torch = True
            response, _ = worker.handle({"id": 1, "command": "load_decision", "model": "Cloudflare/clef-flash"})
        self.assertEqual(response["error"]["code"], "decision_runtime_conflict")

    def test_sherpa_cuda_load_is_refused_after_torch_was_loaded_in_this_worker(self):
        fake_torch = types.ModuleType("torch")
        sherpa = types.SimpleNamespace(__version__="t", onnxruntime_version="t")
        for policy in ["auto", "cuda"]:
            with self.subTest(policy=policy), patch.dict(sys.modules, {"torch": fake_torch}):
                os.environ["ASR_DEVICE"] = policy
                backend = so.SherpaOnnxBackend()
                backend.sherpa = sherpa
                with patch.object(so, "preload_cuda_libraries", side_effect=AssertionError("CUDA DLLs loaded")),                      self.assertRaises(BackendError) as caught:
                    backend._open(so.REAZONSPEECH_K2_V2, {})
                self.assertEqual(caught.exception.code, "decision_runtime_conflict")
                self.assertFalse(backend.excludes_torch)
        # An explicit CPU load stays possible next to torch.
        os.environ["ASR_DEVICE"] = "cpu"
        backend = so.SherpaOnnxBackend()
        backend.sherpa = sherpa
        with patch.dict(sys.modules, {"torch": fake_torch}),              patch.object(backend, "_build", return_value=(FakeRecognizer(), "")):
            backend._open(so.REAZONSPEECH_K2_V2, {})
        self.assertEqual(backend.device, "cpu")

    def test_worker_refuses_cuda_load_after_load_decision_in_the_same_worker(self):
        self.publish(so.REAZONSPEECH_K2_V2, "sherpa-onnx", {})
        os.environ["ASR_DEVICE"] = "auto"
        worker = Worker(so.SherpaOnnxBackend())
        fake_torch = types.ModuleType("torch")

        def load_decision(model, quantization, progress):
            sys.modules["torch"] = fake_torch  # what the real decision load imports

        with patch.dict(sys.modules, {}), patch.object(worker.decision, "load", side_effect=load_decision),              patch.object(so.SherpaOnnxBackend, "_import_runtime", lambda self: setattr(self, "sherpa", types.SimpleNamespace(__version__="t", onnxruntime_version="t"))),              patch.object(ra, "fetch_model", return_value={}),              patch.object(so, "preload_cuda_libraries", side_effect=AssertionError("CUDA DLLs loaded")):
            decision, _ = worker.handle({"id": 1, "command": "load_decision", "model": "Cloudflare/clef-flash"})
            self.assertTrue(decision["ok"], decision)
            response, _ = worker.handle({"id": 2, "command": "load"})
        self.assertEqual(response["error"]["code"], "decision_runtime_conflict")
        self.assertFalse(worker.loaded)

    def test_cuda_attempt_marks_the_process_torch_incompatible_but_cpu_does_not(self):
        os.environ["ASR_DEVICE"] = "cpu"
        cpu = so.SherpaOnnxBackend()
        cpu.sherpa = types.SimpleNamespace(__version__="t", onnxruntime_version="t")
        with patch.object(cpu, "_build", return_value=(FakeRecognizer(), "")):
            cpu._open(so.REAZONSPEECH_K2_V2, {})
        self.assertFalse(cpu.excludes_torch)
        os.environ["ASR_DEVICE"] = "auto"
        auto = so.SherpaOnnxBackend()
        auto.sherpa = cpu.sherpa
        with patch.object(auto, "_build", return_value=(FakeRecognizer(), "")), \
             patch.object(so, "preload_cuda_libraries", return_value=False):
            auto._open(so.REAZONSPEECH_K2_V2, {})
        self.assertTrue(auto.excludes_torch)
        self.assertEqual(auto.device, "cpu")
