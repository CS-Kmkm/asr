from __future__ import annotations

import contextlib
import os
import wave
from pathlib import Path
from typing import Any, Protocol

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

    def load(self, quantization: str) -> None: ...

    def transcribe(self, audio_path: Path, prompt: str | None) -> tuple[str, list[dict[str, Any]]]: ...


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
    message = str(exc).lower()
    if "out of memory" in message or "cuda oom" in message:
        return BackendError("gpu_oom", "GPU out of memory while operating the ASR model")
    if operation == "load":
        return BackendError("model_load_failed", f"Unable to load {backend_label} model: {type(exc).__name__}")
    return BackendError("transcription_failed", f"{backend_label} transcription failed: {type(exc).__name__}")


class MockBackend:
    model_name = f"{MODEL_ID}:mock"

    def __init__(self) -> None:
        self.loaded = False

    def load(self, quantization: str) -> None:
        if quantization not in QUANTIZATIONS:
            raise BackendError("unsupported_quantization", f"Unsupported quantization: {quantization}")
        if os.environ.get("ASR_WORKER_MOCK_LOAD_ERROR") == "oom":
            raise BackendError("gpu_oom", "GPU out of memory while operating the ASR model")
        self.loaded = True

    def transcribe(self, audio_path: Path, prompt: str | None) -> tuple[str, list[dict[str, Any]]]:
        text = os.environ.get("ASR_WORKER_MOCK_TEXT", "mock transcription")
        return text, [{"start": 0.0, "end": 0.0, "speaker": 0, "text": text}]


class VibeVoiceBackend:
    model_name = MODEL_ID

    def __init__(self) -> None:
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

            self.processor = AutoProcessor.from_pretrained(MODEL_ID)
            self.model = VibeVoiceAsrForConditionalGeneration.from_pretrained(MODEL_ID, **kwargs)
            device = str(next(self.model.parameters()).device)
            if not device.startswith("cuda"):
                self.processor = None
                self.model = None
                raise BackendError(
                    "gpu_unsupported",
                    "VibeVoice was not placed on CUDA; CPU fallback is disabled",
                )
        except BackendError:
            raise
        except BaseException as exc:
            raise map_backend_exception(exc, "load") from exc

    def transcribe(self, audio_path: Path, prompt: str | None) -> tuple[str, list[dict[str, Any]]]:
        try:
            inputs = self.processor.apply_transcription_request(
                audio=str(audio_path),
                prompt=prompt,
            ).to(self.model.device, self.model.dtype)
            max_new_tokens = compute_max_new_tokens(
                wav_duration_seconds(audio_path),
                os.environ.get("ASR_MAX_NEW_TOKENS"),
            )
            output_ids = self.model.generate(**inputs, max_new_tokens=max_new_tokens)
            generated_ids = output_ids[:, inputs["input_ids"].shape[1] :]
            parsed = self.processor.decode(generated_ids, return_format="parsed")[0]
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


class FasterWhisperBackend:
    def __init__(self) -> None:
        self.model_id = os.environ.get("ASR_FASTER_WHISPER_MODEL", FASTER_WHISPER_DEFAULT_MODEL)
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
            self.model = WhisperModel(self.model_id, device=device, compute_type=compute_type)
        except BackendError:
            raise
        except BaseException as exc:
            raise map_backend_exception(exc, "load", "faster-whisper") from exc

    def transcribe(self, audio_path: Path, prompt: str | None) -> tuple[str, list[dict[str, Any]]]:
        try:
            hotwords = prompt if prompt else None
            raw_segments, _info = self.model.transcribe(
                str(audio_path),
                language=None,
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


def create_backend(name: str) -> Backend:
    if name == "mock":
        return MockBackend()
    if name == "vibevoice":
        return VibeVoiceBackend()
    if name == "faster-whisper":
        return FasterWhisperBackend()
    raise BackendError("unsupported_backend", f"Unsupported backend: {name}")
