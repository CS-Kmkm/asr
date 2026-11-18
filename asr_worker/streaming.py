"""One bounded, ordered stateful session per worker; hypotheses replace text."""
from __future__ import annotations

import math
from typing import Any

from .backends import BackendError, cancelled_error


class StreamingSession:
    def __init__(self, backend: Any) -> None:
        self.backend = backend
        self.session_id: str | None = None
        self.stream: Any = None
        self.offset = 0

    def close(self) -> None:
        if self.stream is not None:
            close = getattr(self.backend, "close_online_stream", None)
            if close is not None:
                close(self.stream)
        self.stream = None
        self.session_id = None
        self.offset = 0

    def handle(self, request: dict, cancel=None) -> dict:
        operation = request.get("command", request.get("op"))
        session_id = request.get("session_id")
        if not isinstance(session_id, str) or not session_id or len(session_id) > 128:
            raise BackendError("invalid_request", "session_id must be a short non-empty string")
        if operation == "stream_start":
            if cancel is not None and cancel():
                raise cancelled_error()
            if request.get("sample_rate") != 16000:
                raise BackendError("invalid_request", "Streaming requires mono 16 kHz float PCM")
            if self.stream is not None:
                raise BackendError("stream_busy", "Close the previous streaming session first")
            if not getattr(self.backend, "supports_streaming", False):
                raise BackendError("stream_unsupported", "This model supports batch recognition only")
            language = request.get("language")
            if language is not None and (not isinstance(language, str) or len(language) > 32):
                raise BackendError("invalid_request", "language must be a short string or null")
            self.stream = self.backend.create_online_stream(language)
            self.session_id, self.offset = session_id, 0
            if cancel is not None and cancel():
                self.close()
                raise cancelled_error()
            return self.result("")
        if session_id != self.session_id or self.stream is None:
            raise BackendError("stream_missing", "Streaming session is missing or belongs to an older worker")
        if operation in {"stream_cancel", "stream_close"}:
            self.close()
            return {"session_id": session_id, "closed": True}
        try:
            if cancel is not None and cancel():
                raise cancelled_error()
            if operation == "stream_push":
                samples = request.get("samples")
                offset = request.get("offset")
                if type(offset) is not int or offset != self.offset:
                    raise BackendError("stream_sequence", "PCM must be contiguous and submitted once")
                if not isinstance(samples, list) or not 0 < len(samples) <= 32000 or any(
                    type(v) not in (int, float) or not math.isfinite(v) or abs(v) > 1 for v in samples
                ):
                    raise BackendError("invalid_request", "Expected 1..32000 finite PCM samples in [-1, 1]")
                text = self.backend.decode_online(self.stream, samples, final=False)
                self.offset += len(samples)
                result = self.result(text)
            elif operation == "stream_flush":
                if type(request.get("offset")) is not int or request["offset"] != self.offset:
                    raise BackendError("stream_sequence", "Final PCM coverage does not match")
                text = self.backend.decode_online(self.stream, [], final=True)
                result = self.result(text, final=True)
                self.close()
            else:
                raise BackendError("unsupported_operation", "Unknown streaming operation")
            if cancel is not None and cancel():
                raise cancelled_error()
            return result
        except BaseException:
            # A failed mutation cannot be replayed against partially advanced state.
            self.close()
            raise

    def result(self, text: str, final: bool = False) -> dict:
        return {"session_id": self.session_id, "offset": self.offset, "text": str(text),
                "model": self.backend.model_name, "final": final,
                **({"reusable": False} if getattr(self.backend, "stream_final_reusable", True) is False else {})}
