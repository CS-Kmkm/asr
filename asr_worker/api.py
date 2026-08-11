from __future__ import annotations

import os
import secrets
import tempfile
import threading
from pathlib import Path
from typing import Any

from .backends import Backend, BackendError


RESPONSE_FORMATS = {"json", "text", "verbose_json", "srt", "vtt"}
SUPPORTED_AUDIO_SUFFIXES = {".flac", ".m4a", ".mp3", ".mp4", ".mpeg", ".mpga", ".ogg", ".wav", ".webm"}
DEFAULT_MAX_UPLOAD_BYTES = 25 * 1024 * 1024


def _timestamp(value: float, separator: str = ",") -> str:
    milliseconds = max(0, round(value * 1000))
    hours, remainder = divmod(milliseconds, 3_600_000)
    minutes, remainder = divmod(remainder, 60_000)
    seconds, millis = divmod(remainder, 1000)
    return f"{hours:02}:{minutes:02}:{seconds:02}{separator}{millis:03}"


def _subtitle(segments: list[dict[str, Any]], text: str, *, vtt: bool) -> str:
    usable = segments or [{"start": 0.0, "end": 0.0, "text": text}]
    blocks: list[str] = []
    for index, segment in enumerate(usable, start=1):
        separator = "." if vtt else ","
        start = _timestamp(float(segment.get("start", 0.0)), separator)
        end = _timestamp(float(segment.get("end", 0.0)), separator)
        body = str(segment.get("text", "")).strip()
        prefix = "" if vtt else f"{index}\n"
        blocks.append(f"{prefix}{start} --> {end}\n{body}")
    rendered = "\n\n".join(blocks) + "\n"
    return f"WEBVTT\n\n{rendered}" if vtt else rendered


def _verbose_response(
    text: str,
    segments: list[dict[str, Any]],
    language: str | None,
) -> dict[str, Any]:
    normalized = []
    for index, segment in enumerate(segments):
        normalized.append(
            {
                "id": index,
                "seek": 0,
                "start": float(segment.get("start", 0.0)),
                "end": float(segment.get("end", 0.0)),
                "text": str(segment.get("text", "")),
                "tokens": [],
                "temperature": 0.0,
                "avg_logprob": 0.0,
                "compression_ratio": 0.0,
                "no_speech_prob": 0.0,
            }
        )
    duration = max((segment["end"] for segment in normalized), default=0.0)
    return {
        "task": "transcribe",
        "language": language or "unknown",
        "duration": duration,
        "text": text,
        "segments": normalized,
    }


def create_app(
    backend: Backend,
    *,
    quantization: str = "4bit",
    api_key: str | None = None,
    served_model: str | None = None,
    max_upload_bytes: int = DEFAULT_MAX_UPLOAD_BYTES,
):
    """Create an OpenAI Audio Transcriptions compatible ASGI application."""

    try:
        from fastapi import FastAPI, File, Form, Header, HTTPException, UploadFile
        from fastapi.concurrency import run_in_threadpool
        from fastapi.exceptions import RequestValidationError
        from fastapi.responses import JSONResponse, PlainTextResponse
    except ModuleNotFoundError as exc:
        raise BackendError(
            "serve_dependencies_missing",
            "Local API serving requires the 'serve' extra: uv sync --extra serve",
        ) from exc

    # With postponed annotations, FastAPI resolves endpoint annotations through
    # module globals even though optional serve dependencies are imported here.
    globals()["UploadFile"] = UploadFile

    backend.load(quantization)
    inference_lock = threading.Lock()
    model_name = served_model or os.environ.get("ASR_SERVED_MODEL_NAME") or backend.model_name
    app = FastAPI(title="Local ASR OpenAI-compatible API", version="1.0.0")

    def authorize(authorization: str | None) -> None:
        if api_key is None:
            return
        expected = f"Bearer {api_key}"
        if authorization is None or not secrets.compare_digest(authorization, expected):
            raise HTTPException(status_code=401, detail="Invalid API key")

    @app.exception_handler(HTTPException)
    async def http_error_handler(_request, exc: HTTPException):  # noqa: ANN001
        return JSONResponse(
            status_code=exc.status_code,
            content={
                "error": {
                    "message": str(exc.detail),
                    "type": "invalid_request_error",
                    "param": None,
                    "code": "invalid_api_key" if exc.status_code == 401 else "invalid_request",
                }
            },
        )

    @app.exception_handler(BackendError)
    async def backend_error_handler(_request, exc: BackendError):  # noqa: ANN001
        return JSONResponse(
            status_code=500,
            content={
                "error": {
                    "message": exc.message,
                    "type": "server_error",
                    "param": None,
                    "code": exc.code,
                }
            },
        )

    @app.exception_handler(RequestValidationError)
    async def validation_error_handler(_request, exc: RequestValidationError):  # noqa: ANN001
        return JSONResponse(
            status_code=400,
            content={
                "error": {
                    "message": "Invalid transcription request",
                    "type": "invalid_request_error",
                    "param": None,
                    "code": "invalid_request",
                }
            },
        )

    @app.get("/health")
    async def health() -> dict[str, str]:
        return {"status": "ok", "model": model_name}

    @app.get("/v1/models")
    async def models(authorization: str | None = Header(default=None)) -> dict[str, Any]:
        authorize(authorization)
        return {"object": "list", "data": [{"id": model_name, "object": "model", "owned_by": "local"}]}

    @app.post("/v1/audio/transcriptions")
    async def transcription(
        file: UploadFile = File(...),
        model: str = Form(...),
        language: str | None = Form(default=None),
        prompt: str | None = Form(default=None),
        response_format: str = Form(default="json"),
        temperature: float = Form(default=0.0),
        stream: bool = Form(default=False),
        authorization: str | None = Header(default=None),
    ):
        authorize(authorization)
        if not model.strip():
            raise HTTPException(status_code=400, detail="model must not be empty")
        if not 0 <= temperature <= 1:
            raise HTTPException(status_code=400, detail="temperature must be between 0 and 1")
        if language is not None and len(language) > 32:
            raise HTTPException(status_code=400, detail="language is too long")
        if stream:
            raise HTTPException(status_code=400, detail="Streaming transcription is not supported")
        if response_format not in RESPONSE_FORMATS:
            raise HTTPException(
                status_code=400,
                detail=f"response_format must be one of: {', '.join(sorted(RESPONSE_FORMATS))}",
            )
        suffix = Path(file.filename or "audio.wav").suffix.lower()
        if suffix not in SUPPORTED_AUDIO_SUFFIXES:
            suffix = ".audio"
        temporary_path: Path | None = None
        uploaded = 0
        try:
            with tempfile.NamedTemporaryFile(prefix="asr-api-", suffix=suffix, delete=False) as temporary:
                temporary_path = Path(temporary.name)
                while chunk := await file.read(1024 * 1024):
                    uploaded += len(chunk)
                    if uploaded > max_upload_bytes:
                        raise HTTPException(status_code=413, detail="Audio file is too large")
                    temporary.write(chunk)

            def infer() -> tuple[str, list[dict[str, Any]]]:
                with inference_lock:
                    return backend.transcribe(temporary_path, prompt, language)

            text, segments = await run_in_threadpool(infer)
        finally:
            await file.close()
            if temporary_path is not None:
                temporary_path.unlink(missing_ok=True)

        if response_format == "text":
            return PlainTextResponse(text)
        if response_format == "srt":
            return PlainTextResponse(_subtitle(segments, text, vtt=False))
        if response_format == "vtt":
            return PlainTextResponse(_subtitle(segments, text, vtt=True))
        if response_format == "verbose_json":
            return _verbose_response(text, segments, language)
        return {"text": text}

    return app


def run_api_server(
    backend: Backend,
    *,
    host: str,
    port: int,
    quantization: str,
    api_key: str | None,
    served_model: str | None,
) -> None:
    try:
        import uvicorn
    except ModuleNotFoundError as exc:
        raise BackendError(
            "serve_dependencies_missing",
            "Local API serving requires the 'serve' extra: uv sync --extra serve",
        ) from exc
    app = create_app(
        backend,
        quantization=quantization,
        api_key=api_key,
        served_model=served_model,
    )
    uvicorn.run(app, host=host, port=port)
