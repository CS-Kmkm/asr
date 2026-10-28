from __future__ import annotations

import argparse
import importlib.metadata
import json
import os
import platform
import queue
import sys
import threading
import time
from pathlib import Path
from typing import Any, Callable, Iterable, Iterator, TextIO

from .backends import Backend, BackendError, CancelCheck, QUANTIZATIONS, cancelled_error, create_backend
from .diagnostics import log_exception
from .decision import DecisionModel

PROTOCOL_VERSION = 1
WORKER_VERSION = "0.1.0"
SUPPORTED_OPERATIONS = "hello, health, load, transcribe, load_decision, unload_decision, classify_decision, cancel, shutdown"


def _configure_protocol_stdio() -> None:
    """Force the JSONL protocol streams to UTF-8 on every platform.

    Redirected Python stdio inherits the Windows ANSI code page unless UTF-8
    mode is enabled by the parent process. Keep this safeguard for direct
    worker launches as well as launches from the desktop app.
    """
    for stream in (sys.stdin, sys.stdout):
        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure is not None:
            reconfigure(encoding="utf-8", errors="strict")


def _isolate_protocol_stdout() -> TextIO:
    """Reserve the original stdout pipe for JSONL; redirect library fd 1 too."""
    sys.stdout.flush()
    protocol = os.fdopen(os.dup(sys.stdout.fileno()), "w", encoding="utf-8", errors="strict")
    os.dup2(sys.stderr.fileno(), sys.stdout.fileno())
    sys.stdout = sys.stderr
    return protocol


def _sanitize_unicode(value: Any) -> Any:
    """Replace lone UTF-16 surrogates before serializing a protocol response.

    Model output can occasionally contain a lone surrogate. Python strings can
    represent those code points, but Unicode and JSON cannot; serde_json rejects
    an escaped lone surrogate as an invalid Unicode code point.
    """
    if isinstance(value, str):
        sanitized: list[str] = []
        index = 0
        while index < len(value):
            code_point = ord(value[index])
            if 0xD800 <= code_point <= 0xDBFF and index + 1 < len(value):
                low_surrogate = ord(value[index + 1])
                if 0xDC00 <= low_surrogate <= 0xDFFF:
                    sanitized.append(
                        chr(
                            0x10000
                            + ((code_point - 0xD800) << 10)
                            + (low_surrogate - 0xDC00)
                        )
                    )
                    index += 2
                    continue
            sanitized.append("\ufffd" if 0xD800 <= code_point <= 0xDFFF else value[index])
            index += 1
        return "".join(sanitized)
    if isinstance(value, dict):
        return {
            _sanitize_unicode(key): _sanitize_unicode(item)
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [_sanitize_unicode(item) for item in value]
    if isinstance(value, tuple):
        return tuple(_sanitize_unicode(item) for item in value)
    return value


def error_response(request_id: Any, code: str, message: str) -> dict[str, Any]:
    return {"id": request_id, "ok": False, "error": {"code": code, "message": message}}


class Worker:
    def __init__(
        self,
        backend: Backend,
        notify: Callable[[dict[str, Any]], None] | None = None,
    ) -> None:
        self.backend = backend
        self.loaded = False
        self.decision = DecisionModel()
        # Assigned by serve() so a load can report progress before its response.
        self.notify = notify

    def _emit_progress(self, request_id: Any, update: dict[str, Any]) -> None:
        if self.notify is None:
            return
        self.notify({"id": request_id, "event": "progress", **update})

    def handle(
        self,
        request: Any,
        cancel_requested: CancelCheck | None = None,
    ) -> tuple[dict[str, Any], bool]:
        """Run one request. `cancel_requested` reports whether the client has
        cancelled this request since it arrived (see `serve`)."""
        if not isinstance(request, dict):
            return error_response(None, "invalid_request", "Request must be a JSON object"), False
        request_id = request.get("id")
        if "id" not in request:
            return error_response(None, "invalid_request", "Request id is required"), False
        operation = request.get("command", request.get("op"))
        try:
            if operation == "hello":
                return {
                    "id": request_id,
                    "ok": True,
                    "protocol_version": PROTOCOL_VERSION,
                    "worker_version": WORKER_VERSION,
                }, False
            if operation == "health":
                def version(package: str) -> str | None:
                    try:
                        return importlib.metadata.version(package)
                    except importlib.metadata.PackageNotFoundError:
                        return None

                return {
                    "id": request_id,
                    "ok": True,
                    "health": {
                        "pythonPath": sys.executable,
                        "pythonVersion": platform.python_version(),
                        "fasterWhisperVersion": version("faster-whisper"),
                        "ctranslate2Version": version("ctranslate2"),
                        "backend": getattr(self.backend, "backend_name", "unknown"),
                        "loaded": self.loaded,
                        "device": getattr(self.backend, "device", None) if self.loaded else None,
                        "computeType": getattr(self.backend, "compute_type", None) if self.loaded else None,
                    },
                }, False
            if operation in {"load_decision", "classify_decision"}:
                if cancel_requested is not None and cancel_requested():
                    raise cancelled_error()
                if operation == "load_decision":
                    model = request.get("model")
                    quantization = request.get("quantization", "4bit")
                    if not isinstance(model, str) or not isinstance(quantization, str):
                        return error_response(request_id, "invalid_request", "Decision model and quantization must be strings"), False
                    self.decision.load(model, quantization, lambda update: self._emit_progress(request_id, update))
                    result = {"model": model}
                else:
                    result = self.decision.classify(request.get("input"))
                if cancel_requested is not None and cancel_requested():
                    if operation == "load_decision":
                        self.decision.unload()
                    raise cancelled_error()
                return {**result, "id": request_id, "ok": True}, False

            if operation == "unload_decision":
                self.decision.unload()
                return {"id": request_id, "ok": True}, False

            if operation == "load":
                quantization = request.get("quantization", "4bit")
                if not isinstance(quantization, str):
                    return error_response(request_id, "invalid_request", "quantization must be a string"), False
                if self.loaded:
                    self.backend.unload()
                    self.loaded = False
                self.backend.progress = lambda update: self._emit_progress(request_id, update)
                try:
                    self.backend.load(quantization)
                finally:
                    self.backend.progress = None
                self.loaded = True
                return {
                    "id": request_id,
                    "ok": True,
                    "model": self.backend.model_name,
                    "quantization": quantization,
                }, False

            if operation == "transcribe":
                if not self.loaded:
                    return error_response(request_id, "model_not_loaded", "Load the model before transcription"), False
                raw_path = request.get("audio_path")
                if not isinstance(raw_path, str) or not raw_path:
                    return error_response(request_id, "invalid_audio_path", "audio_path must be a non-empty string"), False
                audio_path = Path(raw_path)
                if not audio_path.is_file():
                    return error_response(request_id, "audio_not_found", "Audio path does not exist or is not a file"), False
                prompt = request.get("prompt")
                if isinstance(prompt, list):
                    if not all(isinstance(term, str) for term in prompt):
                        return error_response(
                            request_id,
                            "invalid_request",
                            "prompt dictionary terms must all be strings",
                        ), False
                    prompt = "\n".join(term.strip() for term in prompt if term.strip())
                if prompt is not None and not isinstance(prompt, str):
                    return error_response(
                        request_id,
                        "invalid_request",
                        "prompt must be a string, list of dictionary terms, or null",
                    ), False
                language = request.get("language")
                if language is not None and (not isinstance(language, str) or len(language) > 32):
                    return error_response(
                        request_id,
                        "invalid_request",
                        "language must be a short string or null",
                    ), False
                if cancel_requested is not None and cancel_requested():
                    raise cancelled_error()
                started = time.perf_counter()
                self.backend.cancel_requested = cancel_requested
                try:
                    text, segments = self.backend.transcribe(audio_path, prompt, language)
                finally:
                    self.backend.cancel_requested = None
                # A backend that cannot stop early finishes; its result is
                # discarded because the client no longer waits for it.
                if cancel_requested is not None and cancel_requested():
                    raise cancelled_error()
                return {
                    "id": request_id,
                    "ok": True,
                    "text": text,
                    "segments": segments,
                    "model": self.backend.model_name,
                    "duration_ms": round((time.perf_counter() - started) * 1000),
                }, False

            if operation == "cancel":
                # Handled by `serve` while another request runs; reaching this
                # point means nothing is in flight to cancel.
                if "target_id" not in request:
                    return error_response(request_id, "invalid_request", "target_id is required"), False
                return {"id": request_id, "ok": True, "cancelled": False}, False

            if operation == "shutdown":
                self.decision.unload()
                self.backend.unload()
                self.loaded = False
                return {"id": request_id, "ok": True}, True
            return error_response(
                request_id, "unsupported_operation", f"Supported operations: {SUPPORTED_OPERATIONS}"
            ), False
        except BackendError as exc:
            return error_response(request_id, exc.code, exc.message), operation == "shutdown"
        except BaseException as exc:
            log_exception(exc, "internal")
            return error_response(request_id, "internal_error", "Unexpected worker error"), operation == "shutdown"


def _write_message(output_stream: TextIO, message: dict[str, Any]) -> None:
    safe_message = _sanitize_unicode(message)
    output_stream.write(json.dumps(safe_message, ensure_ascii=False, separators=(",", ":")) + "\n")
    output_stream.flush()


def _request_key(request_id: Any) -> str:
    # Request ids are arbitrary JSON values; compare them by their encoding.
    return json.dumps(request_id, sort_keys=True)


def _is_cancel(request: Any) -> bool:
    return isinstance(request, dict) and request.get("command", request.get("op")) == "cancel"


class _CancelRegistry:
    """Requests read but not yet answered, and which of them were cancelled."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._pending: set[str] = set()
        self._cancelled: set[str] = set()

    def submit(self, key: str) -> None:
        with self._lock:
            self._pending.add(key)

    def finish(self, key: str) -> None:
        with self._lock:
            self._pending.discard(key)
            self._cancelled.discard(key)

    def is_cancelled(self, key: str) -> bool:
        with self._lock:
            return key in self._cancelled

    def cancel(self, request: dict[str, Any]) -> dict[str, Any]:
        request_id = request.get("id")
        if "id" not in request:
            return error_response(None, "invalid_request", "Request id is required")
        if "target_id" not in request:
            return error_response(request_id, "invalid_request", "target_id is required")
        key = _request_key(request["target_id"])
        with self._lock:
            pending = key in self._pending
            if pending:
                self._cancelled.add(key)
        return {"id": request_id, "ok": True, "cancelled": pending}


_END_OF_INPUT = object()


def serve(
    worker: Worker,
    input_stream: Iterable[str] = sys.stdin,
    output_stream: TextIO = sys.stdout,
    *,
    notify_progress: bool = True,
    initialize_load_runtime: Callable[[], Any] | None = None,
) -> None:
    """Answer requests until EOF or `shutdown`.

    A reader thread keeps consuming input while a request runs, so a `cancel`
    is acknowledged at once and the running transcription can stop early.
    Every other request is answered in arrival order by the calling thread.
    """
    write_lock = threading.Lock()

    def write(message: dict[str, Any]) -> None:
        with write_lock:
            _write_message(output_stream, message)

    if notify_progress:
        worker.notify = write
    registry = _CancelRegistry()
    requests: queue.Queue[Any] = queue.Queue()

    def read_requests() -> None:
        nonlocal initialize_load_runtime
        try:
            for line in input_stream:
                try:
                    request = json.loads(line)
                except json.JSONDecodeError:
                    requests.put((None, error_response(None, "invalid_json", "Request is not valid JSON")))
                    continue
                if _is_cancel(request):
                    write(registry.cancel(request))
                    continue
                if (
                    initialize_load_runtime is not None
                    and isinstance(request, dict)
                    and "id" in request
                    and request.get("command", request.get("op")) == "load"
                    and isinstance(request.get("quantization", "4bit"), str)
                    and request.get("quantization", "4bit") in QUANTIZATIONS
                ):
                    # Do native initialization before this thread returns to
                    # its blocking stdin read. Other requests (hello/health)
                    # remain independent of numerical/model dependencies.
                    try:
                        initialize_load_runtime()
                    except Exception as exc:
                        log_exception(exc, "load")
                        code = "backend_unavailable" if isinstance(exc, ImportError) else "model_load_failed"
                        requests.put((None, error_response(request["id"], code, "Unable to initialize the numerical runtime. Run worker setup and try again.")))
                        continue
                    initialize_load_runtime = None
                if isinstance(request, dict) and "id" in request:
                    registry.submit(_request_key(request["id"]))
                requests.put((request, None))
        finally:
            requests.put(_END_OF_INPUT)

    threading.Thread(target=read_requests, name="asr-worker-input", daemon=True).start()
    while True:
        item = requests.get()
        if item is _END_OF_INPUT:
            return
        request, rejection = item
        if rejection is not None:
            write(rejection)
            continue
        key = _request_key(request["id"]) if isinstance(request, dict) and "id" in request else None
        try:
            response, should_stop = worker.handle(
                request,
                None if key is None else (lambda: registry.is_cancelled(key)),
            )
        finally:
            if key is not None:
                registry.finish(key)
        write(response)
        if should_stop:
            return


def _stdin_lines() -> Iterator[str]:
    """Protocol lines read straight from file descriptor 0.

    The reader thread is still blocked here when the worker exits after
    `shutdown`. Reading through `sys.stdin` would leave its buffer lock held
    by a daemon thread, which aborts interpreter shutdown.
    """
    fd = sys.stdin.fileno()
    buffered = b""
    while True:
        chunk = os.read(fd, 65536)
        if not chunk:
            if buffered:
                yield buffered.decode("utf-8", errors="replace")
            return
        buffered += chunk
        while (newline := buffered.find(b"\n")) >= 0:
            line, buffered = buffered[: newline + 1], buffered[newline + 1 :]
            yield line.decode("utf-8", errors="replace")


def main(argv: list[str] | None = None) -> int:
    _configure_protocol_stdio()
    parser = argparse.ArgumentParser(description="VibeVoice JSON Lines worker")
    parser.add_argument(
        "--backend",
        choices=("vibevoice", "faster-whisper", "openai-compatible", "mock"),
        default=os.environ.get("ASR_WORKER_BACKEND", "faster-whisper"),
    )
    parser.add_argument("--serve", action="store_true", help="Serve an OpenAI-compatible HTTP API")
    parser.add_argument("--host", default=os.environ.get("ASR_SERVE_HOST", "127.0.0.1"))
    parser.add_argument("--port", type=int, default=int(os.environ.get("ASR_SERVE_PORT", "8000")))
    parser.add_argument("--quantization", default=os.environ.get("ASR_QUANTIZATION", "4bit"))
    parser.add_argument("--served-model", default=os.environ.get("ASR_SERVED_MODEL_NAME"))
    args = parser.parse_args(argv)
    try:
        backend = create_backend(args.backend)
    except BackendError as exc:
        log_exception(exc, "startup")
        guidance = {
            "invalid_api_config": "Check ASR_API_BASE_URL and ASR_API_TIMEOUT_SECONDS.",
            "invalid_device": "Set ASR_DEVICE to auto, cpu, or cuda.",
        }.get(exc.code, "Check the selected ASR backend and its configuration.")
        print(f"worker startup failed: {exc.code}: {guidance}", file=sys.stderr)
        return 2
    if args.serve:
        try:
            from .api import run_api_server

            run_api_server(
                backend,
                host=args.host,
                port=args.port,
                quantization=args.quantization,
                api_key=os.environ.get("ASR_SERVE_API_KEY"),
                served_model=args.served_model,
            )
        except BackendError as exc:
            print(f"API server startup failed: {exc.code}: {exc.message}", file=sys.stderr)
            return 2
    else:
        # Progress notifications break a client that predates them, and the
        # worker runs from this repository while the desktop app is a
        # separately built binary. Only a client that asks for them gets them.
        with _isolate_protocol_stdout() as output:
            serve(
                Worker(backend),
                _stdin_lines(),
                output_stream=output,
                notify_progress=os.environ.get("ASR_WORKER_PROGRESS") == "1",
                # A first native NumPy import can hang on Windows while the
                # input thread is already blocked waiting for another command.
                initialize_load_runtime=(
                    (lambda: importlib.import_module("numpy"))
                    if os.name == "nt" and args.backend in {"faster-whisper", "vibevoice"}
                    else None
                ),
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
