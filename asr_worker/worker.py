from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path
from typing import Any, TextIO

from .backends import Backend, BackendError, create_backend


def error_response(request_id: Any, code: str, message: str) -> dict[str, Any]:
    return {"id": request_id, "ok": False, "error": {"code": code, "message": message}}


class Worker:
    def __init__(self, backend: Backend) -> None:
        self.backend = backend
        self.loaded = False

    def handle(self, request: Any) -> tuple[dict[str, Any], bool]:
        if not isinstance(request, dict):
            return error_response(None, "invalid_request", "Request must be a JSON object"), False
        request_id = request.get("id")
        if "id" not in request:
            return error_response(None, "invalid_request", "Request id is required"), False
        operation = request.get("command", request.get("op"))
        try:
            if operation == "load":
                quantization = request.get("quantization", "4bit")
                if not isinstance(quantization, str):
                    return error_response(request_id, "invalid_request", "quantization must be a string"), False
                self.backend.load(quantization)
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
                started = time.monotonic()
                text, segments = self.backend.transcribe(audio_path, prompt)
                return {
                    "id": request_id,
                    "ok": True,
                    "text": text,
                    "segments": segments,
                    "model": self.backend.model_name,
                    "duration_ms": round((time.monotonic() - started) * 1000),
                }, False

            if operation == "shutdown":
                return {"id": request_id, "ok": True}, True
            return error_response(request_id, "unsupported_operation", "Supported operations: load, transcribe, shutdown"), False
        except BackendError as exc:
            return error_response(request_id, exc.code, exc.message), False
        except BaseException as exc:
            print(f"worker operation failed: {type(exc).__name__}", file=sys.stderr, flush=True)
            return error_response(request_id, "internal_error", "Unexpected worker error"), False


def serve(worker: Worker, input_stream: TextIO = sys.stdin, output_stream: TextIO = sys.stdout) -> None:
    for line in input_stream:
        try:
            request = json.loads(line)
        except json.JSONDecodeError:
            response, should_stop = error_response(None, "invalid_json", "Request is not valid JSON"), False
        else:
            response, should_stop = worker.handle(request)
        output_stream.write(json.dumps(response, ensure_ascii=False, separators=(",", ":")) + "\n")
        output_stream.flush()
        if should_stop:
            return


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="VibeVoice JSON Lines worker")
    parser.add_argument(
        "--backend",
        choices=("vibevoice", "faster-whisper", "mock"),
        default=os.environ.get("ASR_WORKER_BACKEND", "faster-whisper"),
    )
    args = parser.parse_args(argv)
    try:
        backend = create_backend(args.backend)
    except BackendError as exc:
        print(f"worker startup failed: {exc.code}", file=sys.stderr)
        return 2
    serve(Worker(backend))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
