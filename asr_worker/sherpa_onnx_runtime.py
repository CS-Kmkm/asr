"""sherpa-onnx (ONNX Runtime) adapter: Windows-native CPU and CUDA execution.

The CUDA execution provider silently falls back to CPU inside sherpa-onnx when
its DLLs or the GPU cannot be used, so this adapter reports CUDA only after
three observations: no fallback line in the native log, the CUDA provider
library loaded in this process, and this process holding a CUDA context
(``nvidia-smi`` compute-apps). Otherwise it reports CPU (``auto``) or fails
(``cuda``).

This stage implements offline (batch) recognition. Online recognizers for
stateful streaming (#82) are a new ``ManagedModel.kind`` with its own builder
and session object; the download/verify/device lifecycle stays as is.
"""
from __future__ import annotations

import contextlib
import os
import shutil
import subprocess
from pathlib import Path
from typing import Any

from .backends import BackendError, join_segment_texts
from .diagnostics import log_exception
from .runtime_adapter import ManagedModel, PinnedFile, RuntimeAdapter, captured_native_stderr, split_on_quiet

SHERPA_ONNX_VERSION = "2.0.1"
# Load order matters: each library's own dependencies are loaded first.
CUDA_LIBRARIES = (
    ("cuda_runtime", "cudart64_12.dll"),
    ("cublas", "cublasLt64_12.dll"),
    ("cublas", "cublas64_12.dll"),
    ("cufft", "cufft64_11.dll"),
    ("curand", "curand64_10.dll"),
    ("cuda_nvrtc", "nvrtc64_120_0.dll"),
    ("nvjitlink", "nvJitLink_120_0.dll"),
    ("cudnn", "cudnn64_9.dll"),
)
CUDA_PROVIDER_LIBRARY = "onnxruntime_providers_cuda.dll"
# ReazonSpeech k2 accepts clips of about 30 seconds; longer audio is split.
MAX_CHUNK_SECONDS = 25.0
WARMUP_SECONDS = 1.0

REAZONSPEECH_K2_V2 = ManagedModel(
    id="reazonspeech-k2-v2",
    display_name="ReazonSpeech k2 v2",
    repo="reazon-research/reazonspeech-k2-v2",
    revision="291488c8151be24d7da4bf7af26e533fad96e407",
    files=(
        PinnedFile("encoder-epoch-99-avg-1.onnx", "ecdb0b771e16104aaf8e579cb3c1e32fbd589eb641c5946d82b615bd366c5f96", 592347848),
        PinnedFile("decoder-epoch-99-avg-1.onnx", "58b18211ae06265466bfa17172dab574df94f76c8bcb61a3640c28ba860e4124", 11767836),
        PinnedFile("joiner-epoch-99-avg-1.onnx", "d38a81d1191c9ed6de6a1719503692e07e3e973e2364adde0abae5eaaded1174", 10720115),
        PinnedFile("tokens.txt", "2c3ac659818a48a0c04010e0593bbc4d7c8a24a054340b01131499c05fd52def", 45754),
    ),
    license="Apache-2.0",
    japanese_only=True,
    kind="offline_transducer",
)
MODELS = {model.id: model for model in (REAZONSPEECH_K2_V2,)}


def cuda_library_folders() -> list[Path]:
    """Folders of the NVIDIA runtime wheels installed by the sherpa-onnx extra."""
    try:
        import nvidia  # namespace package of the nvidia-*-cu12 wheels
    except ImportError:
        return []
    roots = [Path(entry) for entry in getattr(nvidia, "__path__", [])]
    return [root / package / "bin" for root in roots for package in sorted({p for p, _ in CUDA_LIBRARIES})
            if (root / package / "bin").is_dir()]


def preload_cuda_libraries() -> bool:
    """Make the pip-installed CUDA 12 / cuDNN 9 DLLs resolvable for ONNX Runtime.

    Returns False when any required library is missing; ONNX Runtime would
    then fall back to CPU, which the caller reports instead of hiding.
    """
    if os.name != "nt":
        return True  # Linux CUDA wheels resolve their libraries themselves.
    import ctypes

    folders = cuda_library_folders()
    for folder in folders:
        with contextlib.suppress(OSError):
            os.add_dll_directory(str(folder))
    # cuDNN loads its sub-libraries by name; only PATH reaches that search.
    path = os.environ.get("PATH", "")
    missing = [str(folder) for folder in folders if str(folder) not in path.split(os.pathsep)]
    if missing:
        os.environ["PATH"] = os.pathsep.join([*missing, path])
    try:
        for package, name in CUDA_LIBRARIES:
            candidates = [folder / name for folder in folders if folder.parent.name == package]
            if not candidates or not candidates[0].is_file():
                return False
            ctypes.WinDLL(str(candidates[0]))
    except OSError:
        return False
    return True


def module_loaded(name: str) -> bool:
    if os.name != "nt":
        return True
    import ctypes

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.GetModuleHandleW.restype = ctypes.c_void_p
    kernel.GetModuleHandleW.argtypes = [ctypes.c_wchar_p]
    return bool(kernel.GetModuleHandleW(name))


def cuda_context_gpu(pid: int | None = None) -> tuple[bool | None, str | None]:
    """Whether ``pid`` holds a CUDA context, and that GPU's name.

    Returns ``(None, None)`` when nvidia-smi is unavailable or fails; the
    caller then relies on its other observations.
    """
    executable = shutil.which("nvidia-smi")
    if executable is None:
        return None, None
    pid = os.getpid() if pid is None else pid
    flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    try:
        apps = subprocess.run([executable, "--query-compute-apps=pid,gpu_uuid", "--format=csv,noheader"],
                              capture_output=True, text=True, timeout=10, creationflags=flags, check=True).stdout
        gpus = subprocess.run([executable, "--query-gpu=uuid,name", "--format=csv,noheader"],
                              capture_output=True, text=True, timeout=10, creationflags=flags, check=True).stdout
    except (OSError, subprocess.SubprocessError):
        return None, None
    names = dict(tuple(part.strip() for part in line.split(",", 1)) for line in gpus.splitlines() if "," in line)
    for line in apps.splitlines():
        parts = [part.strip() for part in line.split(",")]
        if len(parts) >= 2 and parts[0] == str(pid):
            name = names.get(parts[1])
            valid = name and len(name) <= 128 and not any(ord(c) < 32 for c in name)
            return True, name if valid else None
    return False, None


class SherpaOnnxBackend(RuntimeAdapter):
    backend_name = "sherpa-onnx"
    display_label = "sherpa-onnx"
    install_extra = "sherpa-onnx"
    models = MODELS
    default_model = REAZONSPEECH_K2_V2.id

    def __init__(self) -> None:
        super().__init__()
        self.recognizer: Any = None
        self.sherpa: Any = None

    def _import_runtime(self) -> None:
        import sherpa_onnx

        self.sherpa = sherpa_onnx

    def _threads(self) -> int:
        return max(1, min(4, (os.cpu_count() or 2) // 2))

    def _build(self, model: ManagedModel, paths: dict[str, Path], provider: str) -> tuple[Any, str]:
        if model.kind != "offline_transducer":
            raise BackendError("invalid_model", f"Unsupported sherpa-onnx model kind: {model.kind}")
        files = [file.name for file in model.files]
        with captured_native_stderr() as log:
            recognizer = self.sherpa.OfflineRecognizer.from_transducer(
                encoder=str(paths[files[0]]), decoder=str(paths[files[1]]), joiner=str(paths[files[2]]),
                tokens=str(paths[files[3]]), num_threads=self._threads(), sample_rate=model.sample_rate,
                feature_dim=80, decoding_method="greedy_search", provider=provider,
            )
            self._decode(recognizer, [self._silence(model)], model.sample_rate)
        return recognizer, "".join(log)

    @staticmethod
    def _silence(model: ManagedModel) -> Any:
        import numpy as np

        return np.zeros(int(model.sample_rate * WARMUP_SECONDS), dtype=np.float32)

    def _decode(self, recognizer: Any, chunks: list[Any], rate: int) -> list[str]:
        texts = []
        for chunk in chunks:
            self.raise_if_cancelled()
            stream = recognizer.create_stream()
            stream.accept_waveform(rate, chunk)
            recognizer.decode_stream(stream)
            texts.append(str(stream.result.text))
        return texts

    def _open(self, model: ManagedModel, paths: dict[str, Path]) -> None:
        version = getattr(self.sherpa, "__version__", "unknown")
        ort = getattr(self.sherpa, "onnxruntime_version", "unknown")
        self.compute_type = "float32"
        if self.device_policy in ("auto", "cuda"):
            failure = self._open_cuda(model, paths)
            if failure is None:
                self.device = "cuda"
                self.runtime_label = f"sherpa-onnx {version} / ONNX Runtime {ort} CUDAExecutionProvider"
                return
            if self.device_policy == "cuda":
                raise BackendError("cuda_unavailable", f"sherpa-onnx could not run on CUDA ({failure}); CPU fallback is disabled by ASR_DEVICE=cuda")
            self.report_progress("cpu_fallback")
        self.report_progress("warmup")
        self.recognizer, _ = self._build(model, paths, "cpu")
        self.device = "cpu"
        self.runtime_label = f"sherpa-onnx {version} / ONNX Runtime {ort} CPUExecutionProvider"

    def _open_cuda(self, model: ManagedModel, paths: dict[str, Path]) -> str | None:
        """Build on CUDA; return None when CUDA verifiably ran, else the reason."""
        # Even a partial preload leaves CUDA 12 / cuDNN 9 DLLs in the process.
        self.excludes_torch = True
        if not preload_cuda_libraries():
            return "CUDA 12 / cuDNN 9 libraries are missing; run worker setup"
        self.report_progress("warmup")
        try:
            recognizer, log = self._build(model, paths, "cuda")
        except BackendError:
            raise
        except Exception as exc:
            log_exception(exc, "load")
            text = f"{type(exc).__name__}: {exc}".lower()
            if "out of memory" in text and self.device_policy == "cuda":
                raise BackendError("gpu_oom", "GPU out of memory while operating the ASR model") from exc
            return f"CUDA initialization failed ({type(exc).__name__})"
        lowered = log.lower()
        if "fallback to cpu" in lowered or "failed to enable cuda" in lowered:
            del recognizer
            return "ONNX Runtime fell back to CPU"
        if not module_loaded(CUDA_PROVIDER_LIBRARY):
            del recognizer
            return "the CUDA execution provider was not loaded"
        has_context, gpu = cuda_context_gpu()
        if has_context is False:
            del recognizer
            return "no CUDA context was created"
        self.recognizer = recognizer
        self.device_name = gpu
        return None

    def _recognize(self, samples: Any, rate: int, language: str | None,
                   prompt: str | None) -> tuple[str, list[dict[str, Any]]]:
        # The dictionary is applied by the app after recognition; this
        # transducer is decoded greedily without hotwords.
        chunks = split_on_quiet(samples, rate, MAX_CHUNK_SECONDS)
        texts = self._decode(self.recognizer, chunks, rate)
        segments: list[dict[str, Any]] = []
        start = 0.0
        for chunk, text in zip(chunks, texts):
            end = start + len(chunk) / rate
            if text.strip():
                segments.append({"start": round(start, 3), "end": round(end, 3), "speaker": None, "text": text.strip()})
            start = end
        return join_segment_texts(texts), segments

    def _close(self) -> None:
        self.recognizer = None

