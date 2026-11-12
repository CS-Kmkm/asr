"""Shared lifecycle for app-managed local ASR runtimes (issue #87).

A runtime adapter owns one inference runtime (ONNX Runtime through sherpa-onnx,
torch through transformers, ...) and serves only the models it pins here:
repository, immutable revision, file names, sizes and SHA256 digests. A new
runtime subclasses :class:`RuntimeAdapter` and implements ``_import_runtime``,
``_open``, ``_recognize`` and ``_close``; the worker protocol, the recording
pipeline and text insertion stay unchanged because the adapter is an ordinary
``Backend``.

Contract shared with the speech.cpp backend:

* Model files are fetched only by an explicit ``load`` (the app's Load action
  or the first recording that prepares the selected model), from the pinned
  revision, verified before publication and again on every load. There is no
  fallback to another model, a cloud API or an unpinned download.
* ``device``/``compute_type``/``runtime_label`` describe what actually ran, as
  observed after the warm-up, never what was requested.
* Errors are ``BackendError`` codes; dependency, GPU and memory failures map
  to the same codes the other local backends use.
"""
from __future__ import annotations

import contextlib
import gc
import hashlib
import os
import re
import tempfile
import time
import wave
from dataclasses import dataclass
from pathlib import Path
from typing import Any, ClassVar, Iterator, Mapping

from .backends import BackendError, ProgressReporting, map_backend_exception

DEVICE_POLICIES = ("auto", "cpu", "cuda")


@dataclass(frozen=True)
class PinnedFile:
    name: str
    sha256: str
    size: int


@dataclass(frozen=True)
class ManagedModel:
    """One catalog model served by a runtime adapter.

    ``kind`` selects how the adapter builds the recognizer (for example
    ``offline_transducer``). A streaming model added by #82 gets its own kind
    and recognizer builder without changing the download/verify lifecycle.
    """

    id: str
    display_name: str
    repo: str
    revision: str
    files: tuple[PinnedFile, ...]
    license: str
    japanese_only: bool
    kind: str
    sample_rate: int = 16_000

    def url(self, file: PinnedFile) -> str:
        return f"https://huggingface.co/{self.repo}/resolve/{self.revision}/{file.name}"

    @property
    def size(self) -> int:
        return sum(file.size for file in self.files)


def managed_home() -> Path:
    """Root of app-managed model files, mirroring the speech.cpp cache root."""
    override = os.environ.get("ASR_MANAGED_MODEL_HOME")
    if override:
        return Path(override)
    return Path(os.environ.get("LOCALAPPDATA", Path.home() / ".cache")) / "LocalVoice" / "models"


def model_directory(runtime: str, model: ManagedModel) -> Path:
    return managed_home() / runtime / model.id / model.revision


def file_digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(4 * 1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def verify_file(path: Path, file: PinnedFile) -> None:
    """Reject a published file whose size or SHA256 differs from its pin.

    The file is never deleted or replaced here: a mismatch can be a user's
    own file or a concurrent writer, and removal is an explicit user action.
    """
    if path.is_symlink() or not path.is_file():
        raise BackendError("model_artifact_invalid", f"Model file {file.name} is not a regular file; it was left unchanged")
    if path.stat().st_size != file.size or file_digest(path) != file.sha256:
        raise BackendError("model_artifact_invalid", f"Model file {file.name} failed SHA256 verification; existing files were preserved")


def _remove_quietly(path: Path) -> None:
    with contextlib.suppress(OSError):
        path.unlink(missing_ok=True)


def tls_verification() -> Any:
    """Verify certificates against the OS trust store when truststore is installed.

    Security software that inspects TLS installs its root in the OS store
    only; certifi's bundle would reject those connections. Verification is
    never disabled (same approach as the decision model download).
    """
    try:
        import ssl

        import truststore
    except ImportError:
        return True
    return truststore.SSLContext(ssl.PROTOCOL_TLS_CLIENT)


def download_file(url: str, target: Path, file: PinnedFile, backend: ProgressReporting,
                  progress: dict[str, int]) -> None:
    """Download one pinned file to a unique staging name, verify, then publish.

    Publication is an atomic no-clobber hard link, so an interrupted or
    concurrent load never exposes a partial file under the final name.
    """
    import httpx

    target.parent.mkdir(parents=True, exist_ok=True)
    staging: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(dir=target.parent, prefix=f".download-{os.getpid()}-", delete=False) as stream:
            staging = Path(stream.name)
            # No token is sent: the pinned repositories are public. The
            # environment still supplies proxies and corporate CA bundles
            # (SSL_CERT_FILE), as for the speech.cpp downloads.
            with httpx.Client(follow_redirects=True, timeout=60, verify=tls_verification()) as client:
                with client.stream("GET", url) as response:
                    response.raise_for_status()
                    for chunk in response.iter_bytes(1024 * 1024):
                        backend.raise_if_cancelled()
                        stream.write(chunk)
                        progress["completed"] += len(chunk)
                        backend.report_progress("download", completed_bytes=progress["completed"],
                                                total_bytes=progress["total"])
        backend.raise_if_cancelled()
        verify_file(staging, file)
        try:
            os.link(staging, target)
        except FileExistsError:
            pass
        verify_file(target, file)
    finally:
        if staging is not None:
            _remove_quietly(staging)


def clean_stale_staging(folder: Path, max_age_seconds: float = 3600) -> None:
    """Remove this runtime's own old partial downloads, never model files."""
    if not folder.is_dir():
        return
    for path in folder.glob(".download-*"):
        if not re.fullmatch(r"\.download-\d+-[a-z0-9_]{8}", path.name) or path.is_symlink() or not path.is_file():
            continue
        with contextlib.suppress(OSError):
            if time.time() - path.stat().st_mtime > max_age_seconds:
                path.unlink()


def fetch_model(runtime: str, model: ManagedModel, backend: ProgressReporting) -> dict[str, Path]:
    """Return verified local paths of every pinned file, downloading missing ones."""
    folder = model_directory(runtime, model)
    clean_stale_staging(folder)
    paths = {file.name: folder / file.name for file in model.files}
    missing = [file for file in model.files if not paths[file.name].exists()]
    if missing:
        progress = {"completed": 0, "total": sum(file.size for file in missing)}
        backend.report_progress("download", model=f"{model.repo}@{model.revision[:12]}",
                                completed_bytes=0, total_bytes=progress["total"])
        for file in missing:
            backend.raise_if_cancelled()
            try:
                download_file(model.url(file), paths[file.name], file, backend, progress)
            except BackendError:
                raise
            except Exception as exc:
                raise BackendError("hf_download_failed", f"Unable to download {file.name} for {model.display_name}: {type(exc).__name__}") from exc
    for file in model.files:
        verify_file(paths[file.name], file)
    return paths


def read_wav(audio_path: Path) -> tuple[Any, int]:
    """Decode a PCM WAV file to mono float32 samples and its sample rate.

    The app records 16-bit PCM WAV. Other PCM widths are accepted for
    benchmark files; compressed formats are rejected rather than guessed.
    """
    import numpy as np

    try:
        with contextlib.closing(wave.open(str(audio_path), "rb")) as handle:
            channels = handle.getnchannels()
            width = handle.getsampwidth()
            rate = handle.getframerate()
            if handle.getcomptype() != "NONE" or rate <= 0 or width not in (1, 2, 4):
                raise BackendError("audio_unsupported", "This runtime reads PCM WAV audio only")
            data = handle.readframes(handle.getnframes())
    except (wave.Error, EOFError) as exc:
        raise BackendError("audio_unsupported", "This runtime reads PCM WAV audio only") from exc
    if width == 1:
        samples = (np.frombuffer(data, dtype=np.uint8).astype(np.float32) - 128.0) / 128.0
    elif width == 2:
        samples = np.frombuffer(data, dtype="<i2").astype(np.float32) / 32768.0
    else:
        samples = np.frombuffer(data, dtype="<i4").astype(np.float32) / 2147483648.0
    if channels > 1:
        samples = samples[: len(samples) // channels * channels].reshape(-1, channels).mean(axis=1)
    return np.ascontiguousarray(samples, dtype=np.float32), rate


def split_on_quiet(samples: Any, rate: int, max_seconds: float, search_seconds: float = 8.0) -> list[Any]:
    """Split audio longer than ``max_seconds`` at the quietest 100 ms frame.

    Each cut is searched in the last ``search_seconds`` of the window so a
    chunk boundary falls in a pause where possible. A model with a short
    context (ReazonSpeech k2: about 30 s) then never sees an over-long clip.
    """
    import numpy as np

    limit = int(max_seconds * rate)
    if len(samples) <= limit:
        return [samples]
    frame = max(1, rate // 10)
    chunks: list[Any] = []
    start = 0
    while len(samples) - start > limit:
        window_start = start + max(frame, limit - int(search_seconds * rate))
        window_end = start + limit
        region = samples[window_start:window_end]
        frames = len(region) // frame
        if frames == 0:
            cut = window_end
        else:
            energy = np.square(region[: frames * frame].reshape(frames, frame)).mean(axis=1)
            cut = window_start + int(np.argmin(energy)) * frame + frame // 2
        chunks.append(samples[start:cut])
        start = cut
    chunks.append(samples[start:])
    return chunks


@contextlib.contextmanager
def captured_native_stderr() -> Iterator[list[str]]:
    """Capture what native libraries write to file descriptor 2.

    Native runtimes report a silent CPU fallback only as a log line. The
    captured text is written back to the real stderr (the worker's diagnostic
    log) afterwards, so nothing is hidden from diagnostics.
    """
    import sys

    captured: list[str] = []
    with contextlib.suppress(Exception):
        sys.stderr.flush()
    try:
        saved = os.dup(2)
    except OSError:
        yield captured
        return
    with tempfile.TemporaryFile() as sink:
        os.dup2(sink.fileno(), 2)
        try:
            yield captured
        finally:
            with contextlib.suppress(Exception):
                sys.stderr.flush()
            os.dup2(saved, 2)
            os.close(saved)
            sink.seek(0)
            text = sink.read().decode("utf-8", errors="replace")
            captured.append(text)
            if text:
                with contextlib.suppress(Exception):
                    sys.stderr.write(text)
                    sys.stderr.flush()


class RuntimeAdapter(ProgressReporting):
    """Base class of an app-managed runtime serving pinned models.

    Subclasses set the class attributes and implement:

    * ``_import_runtime()``: import the runtime package (``ImportError``
      becomes ``backend_unavailable`` naming ``install_extra``).
    * ``_open(model, paths)``: build the recognizer on the device chosen by
      ``device_policy`` and run a short warm-up; set ``device``,
      ``compute_type`` and ``runtime_label`` from what actually ran.
    * ``_recognize(samples, rate, language, prompt)``: return text and
      segments; poll ``raise_if_cancelled`` wherever the runtime allows.
    * ``_close()``: release the recognizer and any device memory.
    """

    backend_name: ClassVar[str] = "unknown"
    display_label: ClassVar[str] = "runtime"
    install_extra: ClassVar[str] = ""
    models: ClassVar[Mapping[str, ManagedModel]] = {}
    default_model: ClassVar[str] = ""
    # Device policies this runtime accepts; ASR_DEVICE outside it is rejected.
    supported_policies: ClassVar[tuple[str, ...]] = DEVICE_POLICIES

    def __init__(self) -> None:
        self.catalog_id = os.environ.get("ASR_MODEL_ID", "").strip() or self.default_model
        self.model_name = f"{self.backend_name}:{self.catalog_id}"
        policy = os.environ.get("ASR_DEVICE", "auto").strip().lower() or "auto"
        if policy not in DEVICE_POLICIES:
            raise BackendError("invalid_device", "ASR_DEVICE must be auto, cpu, or cuda")
        self.device_policy = policy
        self.device: str | None = None
        self.compute_type: str | None = None
        self.device_name: str | None = None
        self.runtime_label: str | None = None
        self.model: ManagedModel | None = None

    # -- lifecycle -----------------------------------------------------
    def load(self, quantization: str) -> None:
        # The managed models ship one pinned weight format; the app's
        # quantization setting belongs to other backends and is ignored.
        self.unload()
        model = self.models.get(self.catalog_id)
        if model is None:
            choices = ", ".join(sorted(self.models))
            raise BackendError("invalid_model", f"{self.display_label} serves only: {choices}")
        if self.device_policy not in self.supported_policies:
            raise BackendError("invalid_device", f"{self.display_label} supports ASR_DEVICE {', '.join(self.supported_policies)}")
        try:
            try:
                self._import_runtime()
            except ImportError as exc:
                raise self.dependency_error(exc) from exc
            paths = fetch_model(self.backend_name, model, self)
            self.raise_if_cancelled()
            self.report_progress("load", model=self.model_name)
            self._open(model, paths)
            self.model = model
            self.raise_if_cancelled()
        except BackendError:
            self._safe_unload()
            raise
        except BaseException as exc:
            device = self.device
            self._safe_unload()
            raise map_backend_exception(exc, "load", self.display_label, device=device) from exc

    def preload_runtime(self) -> None:
        """Import the native runtime before the worker's input thread blocks.

        On Windows a first native import (NumPy, torch) can hang while another
        thread is blocked reading the protocol pipe; the worker calls this
        from that thread before it reads again. A missing package raises
        ImportError, which the worker reports as ``backend_unavailable``.
        """
        import numpy  # noqa: F401

        self._import_runtime()

    def dependency_error(self, exc: ImportError) -> BackendError:
        # An ImportError of a present package means an incompatible version.
        missing = exc.name or "incompatible version"
        return BackendError(
            "backend_unavailable",
            f"{self.display_label} support is not installed (missing Python module: {missing}). "
            f"Run 'uv sync --extra {self.install_extra}' and restart Local Voice.",
        )

    def _safe_unload(self) -> None:
        with contextlib.suppress(Exception):
            self.unload()

    def unload(self) -> None:
        try:
            self._close()
        finally:
            self.model = None
            self.device = self.compute_type = self.device_name = self.runtime_label = None
            gc.collect()

    def transcribe(self, audio_path: Path, prompt: str | None, language: str | None = None,
                   decoding: str | None = None) -> tuple[str, list[dict[str, Any]]]:
        model = self.model
        if model is None:
            raise BackendError("model_not_loaded", "Load the model before transcription")
        base = language.replace("_", "-").split("-", 1)[0].lower() if language else None
        if model.japanese_only and base not in (None, "ja"):
            raise BackendError("language_unsupported", f"{model.display_name} recognizes Japanese only; select another model for other languages")
        self.raise_if_cancelled()
        try:
            samples, rate = read_wav(audio_path)
            self.raise_if_cancelled()
            text, segments = self._recognize(samples, rate, base, prompt)
        except BackendError:
            raise
        except BaseException as exc:
            raise map_backend_exception(exc, "transcribe", self.display_label, device=self.device) from exc
        self.raise_if_cancelled()
        return text, segments

    # -- runtime hooks -------------------------------------------------
    def _import_runtime(self) -> None:  # pragma: no cover - abstract
        raise NotImplementedError

    def _open(self, model: ManagedModel, paths: dict[str, Path]) -> None:  # pragma: no cover - abstract
        raise NotImplementedError

    def _recognize(self, samples: Any, rate: int, language: str | None,
                   prompt: str | None) -> tuple[str, list[dict[str, Any]]]:  # pragma: no cover - abstract
        raise NotImplementedError

    def _close(self) -> None:  # pragma: no cover - abstract
        raise NotImplementedError
