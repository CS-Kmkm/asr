from __future__ import annotations

import contextlib
import gc
import mimetypes
import os
import wave
from pathlib import Path
from typing import Any, Protocol
from urllib.parse import urlparse

from .download import (
    FASTER_WHISPER_ALLOW_PATTERNS,
    ProgressCallback,
    cached_snapshot_path,
    download_snapshot,
)

MODEL_ID = "microsoft/VibeVoice-ASR-HF"
QUANTIZATIONS = {"4bit", "8bit", "bf16"}
FASTER_WHISPER_DEFAULT_MODEL = "large-v3-turbo"


class BackendError(Exception):
    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


class Backend(Protocol):
    model_name: str
    progress: ProgressCallback | None

    def load(self, quantization: str) -> None: ...

    def unload(self) -> None: ...

    def transcribe(
        self,
        audio_path: Path,
        prompt: str | None,
        language: str | None = None,
    ) -> tuple[str, list[dict[str, Any]]]: ...


class ProgressReporting:
    """Load-progress sink that the worker assigns for the duration of a load.

    Loading a cached model and downloading it on first use look identical from
    the outside, so each backend reports which stage it has reached.
    """

    progress: ProgressCallback | None = None

    def report_progress(self, stage: str, **fields: Any) -> None:
        callback = self.progress
        if callback is not None:
            callback({"stage": stage, **fields})


def faster_whisper_repo_id(model_id: str) -> str | None:
    """Resolve a faster-whisper model name to its Hugging Face repository.

    Returns None when the name is not one of the mappings faster-whisper ships;
    the caller then leaves model resolution to faster-whisper itself and only
    gives up download progress reporting.
    """
    if "/" in model_id:
        return model_id
    try:
        from faster_whisper.utils import _MODELS
    except ImportError:
        return None
    return _MODELS.get(model_id)


def faster_whisper_compute_type(quantization: str, *, cuda: bool) -> str:
    if quantization in {"4bit", "8bit"}:
        return "int8_float16" if cuda else "int8"
    if quantization == "bf16":
        return "float16" if cuda else "int8"
    raise BackendError("unsupported_quantization", f"Unsupported quantization: {quantization}")


def compute_max_new_tokens(
    audio_seconds: float | None,
    env_override: str | None,
    floor: int = 256,
    ceiling: int = 4096,
) -> int:
    if env_override is not None:
        try:
            override = int(env_override)
        except (TypeError, ValueError):
            override = 0
        if override > 0:
            return override
    if audio_seconds is None:
        return ceiling
    estimate = int(audio_seconds * 40) + 128
    return max(floor, min(ceiling, estimate))


def wav_duration_seconds(audio_path: Path) -> float | None:
    try:
        with contextlib.closing(wave.open(str(audio_path), "rb")) as handle:
            frames = handle.getnframes()
            rate = handle.getframerate()
            if rate <= 0:
                return None
            return frames / float(rate)
    except (wave.Error, OSError, EOFError):
        return None


def map_backend_exception(exc: BaseException, operation: str, backend_label: str = "VibeVoice") -> BackendError:
    details: list[str] = []
    current: BaseException | None = exc
    seen: set[int] = set()
    while current is not None and id(current) not in seen:
        seen.add(id(current))
        details.extend((type(current).__name__.lower(), str(current).lower()))
        current = current.__cause__ or current.__context__
    diagnostic = "\n".join(details)

    if "out of memory" in diagnostic or "cuda oom" in diagnostic:
        return BackendError("gpu_oom", "GPU out of memory while operating the ASR model")
    if operation == "load":
        if any(marker in diagnostic for marker in ("rate limit", "ratelimit", "too many requests", "status code: 429")):
            return BackendError(
                "hf_rate_limited",
                f"Hugging Face download rate limit reached for the {backend_label} model. "
                "Set HF_TOKEN for higher limits, wait, then try again.",
            )
        if "gatedrepoerror" in diagnostic or any(
            marker in diagnostic
            for marker in (
                "gated repo",
                "gated model",
                "access to this model is restricted",
                "access to model is restricted",
            )
        ):
            return BackendError(
                "hf_auth_required",
                f"Hugging Face authorization is required to download the {backend_label} model. "
                "Set HF_TOKEN to a read token, accept any gated-model access terms, and restart Local Voice.",
            )
        if "repositorynotfounderror" in diagnostic:
            return BackendError(
                "hf_repository_unavailable",
                f"The Hugging Face repository for the {backend_label} model was not found or is private. "
                "Check the model ID; for a private model, set HF_TOKEN with read access and restart Local Voice.",
            )
        if any(
            marker in diagnostic
            for marker in (
                "401 client error",
                "403 client error",
                "status code: 401",
                "status code: 403",
                "unauthorized",
                "forbidden",
                "invalid token",
                "authentication required",
            )
        ):
            return BackendError(
                "hf_auth_required",
                f"Hugging Face authentication failed while downloading the {backend_label} model. "
                "Set a valid read token in HF_TOKEN and restart Local Voice.",
            )
        if any(
            marker in diagnostic
            for marker in (
                "connectionerror",
                "connecttimeout",
                "readtimeout",
                "name resolution",
                "network is unreachable",
                "offlinemodeisenabled",
            )
        ):
            return BackendError(
                "hf_download_failed",
                f"Could not reach Hugging Face while downloading the {backend_label} model. "
                "Check the network connection and try again.",
            )
        return BackendError("model_load_failed", f"Unable to load {backend_label} model: {type(exc).__name__}")
    return BackendError("transcription_failed", f"{backend_label} transcription failed: {type(exc).__name__}")


def vibevoice_dependency_error(exc: ModuleNotFoundError) -> BackendError:
    missing = exc.name or "unknown"
    return BackendError(
        "backend_unavailable",
        f"VibeVoice support is not installed (missing Python module: {missing}). "
        "Run 'uv sync --extra vibevoice' and restart Local Voice.",
    )


class MockBackend(ProgressReporting):
    model_name = f"{MODEL_ID}:mock"

    def __init__(self) -> None:
        self.loaded = False

    def load(self, quantization: str) -> None:
        if quantization not in QUANTIZATIONS:
            raise BackendError("unsupported_quantization", f"Unsupported quantization: {quantization}")
        if os.environ.get("ASR_WORKER_MOCK_LOAD_ERROR") == "oom":
            raise BackendError("gpu_oom", "GPU out of memory while operating the ASR model")
        self.loaded = True

    def transcribe(
        self,
        audio_path: Path,
        prompt: str | None,
        language: str | None = None,
    ) -> tuple[str, list[dict[str, Any]]]:
        text = os.environ.get("ASR_WORKER_MOCK_TEXT", "mock transcription")
        return text, [{"start": 0.0, "end": 0.0, "speaker": 0, "text": text}]

    def unload(self) -> None:
        self.loaded = False


class VibeVoiceBackend(ProgressReporting):
    def __init__(self) -> None:
        self.model_id = os.environ.get("ASR_MODEL_ID", MODEL_ID)
        self.model_name = self.model_id
        self.processor: Any = None
        self.model: Any = None

    def load(self, quantization: str) -> None:
        if quantization not in QUANTIZATIONS:
            raise BackendError("unsupported_quantization", f"Unsupported quantization: {quantization}")

        try:
            import torch
            from transformers import AutoProcessor, VibeVoiceAsrForConditionalGeneration

            if not torch.cuda.is_available():
                raise BackendError(
                    "gpu_unsupported",
                    "VibeVoice requires a CUDA GPU; CPU fallback is disabled",
                )

            kwargs: dict[str, Any] = {"device_map": "cuda"}
            if quantization == "4bit":
                from transformers import BitsAndBytesConfig
                kwargs["quantization_config"] = BitsAndBytesConfig(load_in_4bit=True)
            elif quantization == "8bit":
                from transformers import BitsAndBytesConfig
                kwargs["quantization_config"] = BitsAndBytesConfig(load_in_8bit=True)
            else:
                kwargs["torch_dtype"] = torch.bfloat16

            self._report_pending_download()
            self.processor = AutoProcessor.from_pretrained(self.model_id)
            self.model = VibeVoiceAsrForConditionalGeneration.from_pretrained(self.model_id, **kwargs)
            device = str(next(self.model.parameters()).device)
            if not device.startswith("cuda"):
                self.processor = None
                self.model = None
                raise BackendError(
                    "gpu_unsupported",
                    "VibeVoice was not placed on CUDA; CPU fallback is disabled",
                )
        except ModuleNotFoundError as exc:
            raise vibevoice_dependency_error(exc) from exc
        except BackendError:
            raise
        except BaseException as exc:
            raise map_backend_exception(exc, "load") from exc

    def _report_pending_download(self) -> None:
        """Report that model files are still missing before transformers loads.

        transformers downloads inside ``from_pretrained`` without a progress
        hook, so only the stage is reported for this backend.
        """
        if Path(self.model_id).is_dir():
            return
        if cached_snapshot_path(self.model_id) is None:
            self.report_progress("download", model=self.model_id)

    def unload(self) -> None:
        self.model = None
        self.processor = None
        gc.collect()
        try:
            import torch

            if torch.cuda.is_available():
                torch.cuda.empty_cache()
                # Releases CUDA inter-process cache allocations when supported.
                if hasattr(torch.cuda, "ipc_collect"):
                    torch.cuda.ipc_collect()
        except (ImportError, RuntimeError):
            # Cleanup is best-effort and must not prevent worker shutdown.
            pass

    def transcribe(
        self,
        audio_path: Path,
        prompt: str | None,
        language: str | None = None,
    ) -> tuple[str, list[dict[str, Any]]]:
        try:
            import torch

            # Inference mode removes autograd bookkeeping and view tracking for
            # the entire preprocessing/generation path. Releasing the (often
            # large) audio inputs before decoding also lowers peak live VRAM.
            with torch.inference_mode():
                inputs = self.processor.apply_transcription_request(
                    audio=str(audio_path),
                    prompt=prompt,
                ).to(self.model.device, self.model.dtype)
                input_length = inputs["input_ids"].shape[1]
                max_new_tokens = compute_max_new_tokens(
                    wav_duration_seconds(audio_path),
                    os.environ.get("ASR_MAX_NEW_TOKENS"),
                )
                output_ids = self.model.generate(**inputs, max_new_tokens=max_new_tokens)
                generated_ids = output_ids[:, input_length:]
                del inputs
                parsed = self.processor.decode(generated_ids, return_format="parsed")[0]
                del generated_ids, output_ids
            segments = [
                {
                    "start": float(item.get("Start", 0.0)),
                    "end": float(item.get("End", 0.0)),
                    "speaker": item.get("Speaker"),
                    "text": str(item.get("Content", "")),
                }
                for item in parsed
            ]
            text = " ".join(segment["text"] for segment in segments).strip()
            return text, segments
        except BaseException as exc:
            raise map_backend_exception(exc, "transcribe") from exc


class FasterWhisperBackend(ProgressReporting):
    def __init__(self) -> None:
        self.model_id = os.environ.get(
            "ASR_MODEL_ID",
            os.environ.get("ASR_FASTER_WHISPER_MODEL", FASTER_WHISPER_DEFAULT_MODEL),
        )
        self.model_name = f"faster-whisper:{self.model_id}"
        self.model: Any = None

    def load(self, quantization: str) -> None:
        if quantization not in QUANTIZATIONS:
            raise BackendError("unsupported_quantization", f"Unsupported quantization: {quantization}")
        try:
            from faster_whisper import WhisperModel
        except ImportError as exc:
            raise BackendError(
                "backend_unavailable",
                "faster-whisper is not installed; install with: pip install faster-whisper",
            ) from exc
        try:
            import ctranslate2

            cuda = ctranslate2.get_cuda_device_count() > 0
            device = "cuda" if cuda else "cpu"
            compute_type = faster_whisper_compute_type(quantization, cuda=cuda)
            model_source = self._resolve_model_files()
            self.report_progress("load")
            self.model = WhisperModel(model_source, device=device, compute_type=compute_type)
        except BackendError:
            raise
        except BaseException as exc:
            raise map_backend_exception(exc, "load", "faster-whisper") from exc

    def _resolve_model_files(self) -> str:
        """Return a local model source, downloading it only when it is missing.

        WhisperModel downloads silently, which makes a first-use download
        indistinguishable from loading a cached model. Resolving the files here
        keeps the two apart and lets the download report byte progress.
        """
        if Path(self.model_id).is_dir():
            return self.model_id
        repo_id = faster_whisper_repo_id(self.model_id)
        if repo_id is None:
            return self.model_id
        cached = cached_snapshot_path(repo_id, FASTER_WHISPER_ALLOW_PATTERNS)
        if cached is not None:
            return cached
        self.report_progress("download", model=repo_id)
        return download_snapshot(
            repo_id,
            FASTER_WHISPER_ALLOW_PATTERNS,
            lambda update: self.report_progress("download", model=repo_id, **update),
        )

    def unload(self) -> None:
        self.model = None
        gc.collect()

    def transcribe(
        self,
        audio_path: Path,
        prompt: str | None,
        language: str | None = None,
    ) -> tuple[str, list[dict[str, Any]]]:
        try:
            hotwords = prompt if prompt else None
            raw_segments, _info = self.model.transcribe(
                str(audio_path),
                language=language,
                hotwords=hotwords,
            )
            segments = [
                {
                    "start": float(segment.start),
                    "end": float(segment.end),
                    "speaker": None,
                    "text": str(segment.text),
                }
                for segment in raw_segments
            ]
            text = "".join(segment["text"] for segment in segments).strip()
            return text, segments
        except BaseException as exc:
            raise map_backend_exception(exc, "transcribe", "faster-whisper") from exc


class OpenAICompatibleBackend(ProgressReporting):
    """Client for OpenAI's ``POST /v1/audio/transcriptions`` contract.

    The endpoint can be OpenAI itself or a locally served compatible model. API
    credentials are read from the environment so they are not part of worker
    protocol messages or diagnostic output.
    """

    def __init__(self) -> None:
        self.base_url = os.environ.get("ASR_API_BASE_URL", "https://api.openai.com/v1").rstrip("/")
        self.api_key = os.environ.get("ASR_API_KEY") or os.environ.get("OPENAI_API_KEY")
        self.model_id = os.environ.get("ASR_MODEL_ID", "gpt-4o-mini-transcribe")
        self.model_name = f"openai-compatible:{self.model_id}"
        self.timeout = self._timeout_from_env()
        self.client: Any = None

    @staticmethod
    def _timeout_from_env() -> float:
        raw = os.environ.get("ASR_API_TIMEOUT_SECONDS", "300")
        try:
            timeout = float(raw)
        except ValueError as exc:
            raise BackendError("invalid_api_config", "ASR_API_TIMEOUT_SECONDS must be a number") from exc
        if not 1 <= timeout <= 3600:
            raise BackendError("invalid_api_config", "ASR_API_TIMEOUT_SECONDS must be between 1 and 3600")
        return timeout

    def load(self, quantization: str) -> None:
        parsed = urlparse(self.base_url)
        if parsed.scheme not in {"http", "https"} or not parsed.netloc:
            raise BackendError("invalid_api_config", "ASR API base URL must be an absolute HTTP(S) URL")
        if parsed.hostname == "api.openai.com" and not self.api_key:
            raise BackendError(
                "missing_api_key",
                "OPENAI_API_KEY or ASR_API_KEY is required for api.openai.com",
            )
        try:
            import httpx

            if self.client is not None:
                self.client.close()
            headers = {"Authorization": f"Bearer {self.api_key}"} if self.api_key else {}
            self.client = httpx.Client(base_url=f"{self.base_url}/", headers=headers, timeout=self.timeout)
        except ModuleNotFoundError as exc:
            raise BackendError(
                "backend_unavailable",
                "OpenAI-compatible API support is not installed (missing Python module: httpx)",
            ) from exc

    def transcribe(
        self,
        audio_path: Path,
        prompt: str | None,
        language: str | None = None,
    ) -> tuple[str, list[dict[str, Any]]]:
        if self.client is None:
            raise BackendError("model_not_loaded", "Load the API backend before transcription")
        # JSON is supported by whisper-1, GPT-4o transcription models, and
        # compatible local servers. verbose_json is not accepted by every
        # OpenAI transcription model.
        data = {"model": self.model_id, "response_format": "json"}
        if prompt:
            data["prompt"] = prompt
        if language:
            data["language"] = language
        media_type = mimetypes.guess_type(audio_path.name)[0] or "application/octet-stream"
        try:
            with audio_path.open("rb") as audio:
                response = self.client.post(
                    "audio/transcriptions",
                    data=data,
                    files={"file": (audio_path.name, audio, media_type)},
                )
            if response.status_code >= 400:
                raise BackendError(
                    "api_request_failed",
                    f"Transcription API returned HTTP {response.status_code}",
                )
            payload = response.json()
            text = payload.get("text")
            if not isinstance(text, str):
                raise BackendError("invalid_api_response", "Transcription API response has no text field")
            raw_segments = payload.get("segments", [])
            segments: list[dict[str, Any]] = []
            if isinstance(raw_segments, list):
                for segment in raw_segments:
                    if not isinstance(segment, dict):
                        continue
                    try:
                        start = float(segment.get("start", 0.0))
                        end = float(segment.get("end", 0.0))
                    except (TypeError, ValueError):
                        continue
                    segments.append(
                        {
                            "start": start,
                            "end": end,
                            "speaker": segment.get("speaker"),
                            "text": str(segment.get("text", "")),
                        }
                    )
            if not segments:
                segments = [{"start": 0.0, "end": 0.0, "speaker": None, "text": text}]
            return text, segments
        except BackendError:
            raise
        except (OSError, ValueError) as exc:
            raise BackendError(
                "api_request_failed",
                f"Unable to use transcription API: {type(exc).__name__}",
            ) from exc
        except BaseException as exc:
            # httpx is an optional runtime import; avoid coupling exception
            # matching to it while still keeping credentials and response bodies
            # out of diagnostics.
            raise BackendError(
                "api_request_failed",
                f"Transcription API request failed: {type(exc).__name__}",
            ) from exc

    def unload(self) -> None:
        if self.client is not None:
            self.client.close()
            self.client = None
        gc.collect()


def create_backend(name: str) -> Backend:
    if name == "mock":
        return MockBackend()
    if name == "vibevoice":
        return VibeVoiceBackend()
    if name == "faster-whisper":
        return FasterWhisperBackend()
    if name == "openai-compatible":
        return OpenAICompatibleBackend()
    raise BackendError("unsupported_backend", f"Unsupported backend: {name}")
