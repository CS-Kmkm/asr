"""Pinned Windows/Vulkan Japanese models, served only on an owned loopback port.

The model load action acquires missing immutable artifacts. No shell, external
endpoint, user-supplied executable or implicit cloud/CPU fallback is used.
"""
from __future__ import annotations

import contextlib
import hashlib
import os
import platform
import re
import subprocess
import tempfile
import threading
import time
import zipfile
from pathlib import Path
from typing import Any

from .backends import BackendError, OpenAICompatibleBackend

VERSION = "0.10.1"
IS_WINDOWS = os.name == "nt"
ARCHIVE = "speech-0.10.1-windows-x64-vulkan.zip"
ARCHIVE_SHA256 = "a8fe0e4b4f463c460e7ace9d38c363bec08b254cac5a7f764b7967af35d4fa1a"
RUNTIME_FILES = {
    "speech.exe": "46f264e26fcaa60f433455c6c6092137c123af67fad956d67f250daf5ebaa893",
    "speech.dll": "923fb0babff5596202ae92dc5b6423a23579ab461bc7cce8b35258107ee05662",
}
# Revision and digest are the converted artifact's identity, not the upstream
# NeMo checkpoint's identity. Q8_0 uses a GPU encoder and CPU greedy decoder.
MODELS = {
    "haas-ja-v1.0": {
        "repo": "sakasegawa/Haas-ja-v1.0-GGUF",
        "revision": "6b965d5007ba780585de51549d57a8c1c0d67c7e",
        "file": "Haas-616M-ja-v1.0-Q8_0.gguf",
        "sha256": "cd5f60b2f1c411404b83fca4a65e3ce30a26d9e951b6a346529e65f22758afeb",
        "license": "Apache-2.0", "size": 656381472,
    },
    "parakeet-tdt_ctc-0.6b-ja": {
        "repo": "sakasegawa/parakeet-tdt_ctc-0.6b-ja-GGUF",
        "revision": "f7cb5387e1e76385456b7a792054565ecf177417",
        "file": "parakeet-tdt_ctc-0.6B-ja-Q8_0.gguf",
        "sha256": "6ff90c5240f0778dfb44816916eb3c8e634645e120170d6ae43c0f50a11d79d0",
        "license": "CC-BY-4.0", "size": 661012416,
    },
}


def server_environment() -> dict[str, str]:
    # A local GGUF server has no reason to receive Hugging Face tokens,
    # cloud credentials, proxy secrets or any of the worker's ASR settings.
    allowed = {"SYSTEMROOT", "WINDIR", "PATH", "PATHEXT", "TEMP", "TMP",
               "USERPROFILE", "LOCALAPPDATA", "APPDATA", "HOMEDRIVE", "HOMEPATH", "PROGRAMDATA"}
    return {name: value for name, value in os.environ.items() if name.upper() in allowed}


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest() if hasattr(hashlib, "file_digest") else _digest(stream)


def _digest(stream: Any) -> str:
    result = hashlib.sha256()
    for chunk in iter(lambda: stream.read(1024 * 1024), b""):
        result.update(chunk)
    return result.hexdigest()


def verify(path: Path, expected: str) -> None:
    if digest(path) != expected:
        raise BackendError("speech_cpp_artifact_invalid", "speech.cpp artifact SHA256 mismatch; existing files were preserved")


def _remove_staging(path: Path) -> None:
    # Antivirus can briefly lock a completed download. Failure to remove only
    # its staging name must not turn successful publication into a load error.
    with contextlib.suppress(OSError):
        path.unlink(missing_ok=True)


def publish_bytes(content: bytes, target: Path, expected: str) -> None:
    """Atomic no-clobber extraction, including interruption/disk-full cases."""
    if target.exists():
        verify(target, expected)
        return
    staging: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(dir=target.parent, prefix=f".download-{os.getpid()}-", delete=False) as stream:
            staging = Path(stream.name)
            stream.write(content)
        verify(staging, expected)
        try:
            os.link(staging, target)
        except FileExistsError:
            verify(target, expected)
    finally:
        if staging is not None:
            _remove_staging(staging)


def process_alive(pid: int) -> bool:
    # Called only on Windows. Access denied is not proof of a dead owner.
    import ctypes
    from ctypes import wintypes
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    handle = kernel.OpenProcess(0x1000, False, pid)
    if not handle:
        return ctypes.get_last_error() != 87  # ERROR_INVALID_PARAMETER: PID is gone
    try:
        code = wintypes.DWORD()
        return not kernel.GetExitCodeProcess(handle, ctypes.byref(code)) or code.value == 259
    finally:
        kernel.CloseHandle(handle)


def clean_staging(home: Path) -> None:
    """Remove only old, owned partials of exited workers; never model files."""
    folders = [home, home / f"runtime-{VERSION}", *(home / model["revision"] for model in MODELS.values())]
    for folder in folders:
        for path in folder.glob(".download-*"):
            match = re.fullmatch(r"\.download-(\d+)-[a-z0-9_]{8}", path.name)
            if match is None or path.is_symlink() or not path.is_file():
                continue
            with contextlib.suppress(OSError):
                if time.time() - path.stat().st_mtime > 3600 and not process_alive(int(match[1])):
                    _remove_staging(path)


def cache_home() -> Path:
    override = os.environ.get("ASR_SPEECH_CPP_HOME")
    return Path(override) if override else Path(os.environ.get("LOCALAPPDATA", Path.home() / ".cache")) / "LocalVoice" / "speech.cpp"


def acquire(url: str, target: Path, expected: str, backend: Any) -> None:
    """Download to a unique staging file and publish without replacing anything."""
    if target.exists():
        verify(target, expected)
        return
    import httpx
    target.parent.mkdir(parents=True, exist_ok=True)
    staging: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(dir=target.parent, prefix=f".download-{os.getpid()}-", delete=False) as stream:
            staging = Path(stream.name)
            with httpx.stream("GET", url, follow_redirects=True, timeout=60) as response:
                response.raise_for_status()
                total = response.headers.get("content-length")
                completed = 0
                for chunk in response.iter_bytes(1024 * 1024):
                    backend.raise_if_cancelled()
                    stream.write(chunk)
                    completed += len(chunk)
                    backend.report_progress("download", completed_bytes=completed,
                                            total_bytes=int(total) if total and total.isdigit() else None)
        verify(staging, expected)
        backend.raise_if_cancelled()
        try:
            # Atomic, no-clobber publication on Windows and Unix, same volume.
            os.link(staging, target)
        except FileExistsError:
            verify(target, expected)
    finally:
        if staging is not None:
            _remove_staging(staging)


def prepare(backend: Any) -> tuple[Path, Path]:
    if not IS_WINDOWS or platform.machine().lower() not in {"amd64", "x86_64"}:
        raise BackendError("speech_cpp_unsupported_platform", "Japanese Vulkan models require Windows x64. Select faster-whisper on this system.")
    model = MODELS.get(backend.catalog_id)
    if model is None:
        raise BackendError("speech_cpp_unavailable", "Select haas-ja-v1.0 or parakeet-tdt_ctc-0.6b-ja")
    home = cache_home()
    clean_staging(home)
    runtime = home / f"runtime-{VERSION}"
    if not all((runtime / name).is_file() for name in RUNTIME_FILES):
        archive = home / ARCHIVE
        acquire(f"https://github.com/nyosegawa/speech.cpp/releases/download/v{VERSION}/{ARCHIVE}", archive, ARCHIVE_SHA256, backend)
        runtime.mkdir(parents=True, exist_ok=True)
        # Extract only the two known, independently hashed executables. Never
        # use extractall, and never replace a preexisting runtime file.
        with zipfile.ZipFile(archive) as bundle:
            for name, expected in RUNTIME_FILES.items():
                path = runtime / name
                publish_bytes(bundle.read(name), path, expected)
    for name, expected in RUNTIME_FILES.items():
        verify(runtime / name, expected)
    audio_model = home / model["revision"] / model["file"]
    acquire(f"https://huggingface.co/{model['repo']}/resolve/{model['revision']}/{model['file']}", audio_model, model["sha256"], backend)
    return runtime / "speech.exe", audio_model


def attach_job(proc: subprocess.Popen) -> Any:
    """Kill the server even when Rust forcibly kills this Python worker.

    Fail closed if the Windows job cannot be attached. The handle is not
    inherited, so only this worker keeps the server alive.
    """
    import ctypes
    from ctypes import wintypes

    class BasicLimits(ctypes.Structure):
        _fields_ = [("process_time", ctypes.c_int64), ("job_time", ctypes.c_int64),
                    ("flags", wintypes.DWORD), ("minimum", ctypes.c_size_t),
                    ("maximum", ctypes.c_size_t), ("active", wintypes.DWORD),
                    ("affinity", ctypes.c_size_t), ("priority", wintypes.DWORD),
                    ("scheduling", wintypes.DWORD)]

    class Limits(ctypes.Structure):
        _fields_ = [("basic", BasicLimits), ("io", ctypes.c_uint64 * 6),
                    ("process_memory", ctypes.c_size_t), ("job_memory", ctypes.c_size_t),
                    ("peak_process", ctypes.c_size_t), ("peak_job", ctypes.c_size_t)]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    kernel.CreateJobObjectW.restype = wintypes.HANDLE
    kernel.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    handle = kernel.CreateJobObjectW(None, None)
    if not handle:
        raise OSError("CreateJobObject failed")
    limits = Limits()
    limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    if not kernel.SetInformationJobObject(handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)) or not kernel.AssignProcessToJobObject(handle, wintypes.HANDLE(int(proc._handle))):
        kernel.CloseHandle(handle)
        raise OSError("Unable to own speech.cpp process")
    return kernel, handle


class SpeechCppBackend(OpenAICompatibleBackend):
    backend_name = "speech-cpp"

    def __init__(self) -> None:
        super().__init__()
        self.catalog_id = os.environ.get("ASR_MODEL_ID", "haas-ja-v1.0")
        self.model_name = f"speech.cpp:{self.catalog_id}:Q8_0"
        # Never send inherited credentials to the unauthenticated local server.
        self.api_key = None
        self.process: subprocess.Popen | None = None
        self.job: Any = None
        self.reader: threading.Thread | None = None
        self.device_name: str | None = None

    def load(self, quantization: str) -> None:
        self.unload()
        try:
            executable, model = prepare(self)
            self.raise_if_cancelled()
            self.report_progress("load", model=self.model_name)
            import httpx
            devices = subprocess.run([str(executable.resolve()), "devices"], capture_output=True, timeout=10,
                                     env=server_environment(), creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
                                     check=True).stdout.decode("utf-8", errors="replace")
            device_line = re.search(r"^Vulkan0\s+gpu\s+([^\r\n]+)", devices, re.MULTILINE)
            if device_line is None:
                raise BackendError("speech_cpp_unavailable", "speech.cpp found no Vulkan0 GPU; select faster-whisper to recover")
            device_name = device_line[1].split(",", 1)[0].strip()
            if not device_name or len(device_name) > 128 or any(ord(c) < 32 for c in device_name):
                raise BackendError("speech_cpp_unavailable", "speech.cpp returned invalid GPU information")
            # Port 0 is bound by this child. Read its actual bound port, rather
            # than probing a free port and trusting some other process on it.
            ready = threading.Event()
            state: dict[str, Any] = {}
            self.process = subprocess.Popen(
                [str(executable.resolve()), "serve", str(model.resolve()), "--host", "127.0.0.1",
                 "--port", "0", "--device", "Vulkan0"],
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
                env=server_environment(),
            )
            self.job = attach_job(self.process)
            output = self.process.stdout

            def drain() -> None:
                assert output is not None
                try:
                    for raw in output:
                        line = raw.decode("utf-8", errors="replace")
                        match = re.search(r"Ready at http://127\.0\.0\.1:(\d+)/", line)
                        if match:
                            state["port"] = int(match[1])
                            ready.set()
                finally:
                    output.close()
                    ready.set()

            self.reader = threading.Thread(target=drain, daemon=True)
            self.reader.start()
            deadline = time.monotonic() + self.timeout
            while not ready.wait(0.1):
                self.raise_if_cancelled()
                if self.process.poll() is not None or time.monotonic() >= deadline:
                    raise BackendError("speech_cpp_unavailable", "speech.cpp failed to start on Vulkan; select faster-whisper to recover")
            self.raise_if_cancelled()
            if self.process.poll() is not None or "port" not in state:
                raise BackendError("speech_cpp_unavailable", "speech.cpp exited during startup; select faster-whisper to recover")
            self.base_url = f"http://127.0.0.1:{state['port']}/v1"
            self.client = httpx.Client(base_url=self.base_url + "/", timeout=self.timeout, trust_env=False)
            models = self.client.get("models")
            models.raise_for_status()
            entries = models.json()["data"]
            recognition = [item for item in entries if item.get("speech", {}).get("task") == "recognition"]
            if len(recognition) != 1 or recognition[0]["speech"].get("device") != "Vulkan0" or recognition[0]["speech"].get("weight_type") != "Q8_0":
                raise BackendError("speech_cpp_unavailable", "speech.cpp did not report the loaded recognition model")
            self.model_id = recognition[0]["id"]
            self.device = recognition[0]["speech"]["device"].removesuffix("0").lower()
            self.compute_type = recognition[0]["speech"]["weight_type"]
            self.device_name = device_name
        except BackendError:
            with contextlib.suppress(BackendError):
                self.unload()
            raise
        except Exception as exc:
            with contextlib.suppress(BackendError):
                self.unload()
            raise BackendError("speech_cpp_unavailable", "Unable to prepare speech.cpp; check disk, network and Vulkan support, or select faster-whisper") from exc

    def transcribe(self, audio_path: Path, prompt: str | None, language: str | None = None,
                   decoding: str | None = None) -> tuple[str, list[dict[str, Any]]]:
        if language and language.replace("_", "-").split("-", 1)[0].lower() != "ja":
            raise BackendError("speech_cpp_language", "This model recognizes Japanese only; select faster-whisper for other languages")
        if self.process is None or self.process.poll() is not None:
            raise BackendError("speech_cpp_unavailable", "speech.cpp is not running; reload the model or select faster-whisper")
        # FastConformer does not accept OpenAI's text prompt. The app still
        # applies its deterministic dictionary after recognition. Native Haas
        # keyword boosting needs structured entries, not a lossy prompt parse.
        return super().transcribe(audio_path, None, language, decoding)

    def unload(self) -> None:
        process, job, reader = self.process, self.job, self.reader
        self.process = self.job = self.reader = None
        self.device = self.compute_type = self.device_name = None
        stuck = False
        try:
            with contextlib.suppress(Exception):
                self._close_client()
            self.client = self._client_key = None
            if process is not None:
                with contextlib.suppress(OSError, subprocess.SubprocessError):
                    if process.poll() is None:
                        process.terminate()
                    process.wait(timeout=5)
                with contextlib.suppress(OSError, subprocess.SubprocessError):
                    if process.poll() is None:
                        process.kill()
                    process.wait(timeout=5)
                stuck = process.poll() is None
        finally:
            if job is not None:
                kernel, handle = job
                kernel.CloseHandle(handle)
            if reader is not None:
                with contextlib.suppress(RuntimeError):
                    reader.join(timeout=2)
            # Closing a pipe while another thread is blocked reading it can
            # block too. The drain thread owns that close in the stuck case.
            if process is not None and process.stdout is not None and (reader is None or not reader.is_alive()):
                process.stdout.close()
        if stuck:
            raise BackendError("speech_cpp_unavailable", "The GPU process did not stop; restart the worker or select faster-whisper")
