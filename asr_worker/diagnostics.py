"""Allowlisted exception diagnostics: no values, source text, paths or locals."""
from __future__ import annotations

import json
import sys
import traceback

PREFIX = "ASR_DIAGNOSTIC "
ERROR_TYPES = {"RuntimeError", "ValueError", "TypeError", "OSError", "FileNotFoundError", "PermissionError", "ModuleNotFoundError", "ImportError", "MemoryError", "BackendError", "HTTPError", "ReadTimeout", "ConnectTimeout", "Exception"}
HINTS = {
    "cublas64_12.dll": "missing_cublas12",
    "cudnn64_9.dll": "missing_cudnn9",
    "out of memory": "out_of_memory",
    "tokenizer.json": "tokenizer_configuration",
    "model.bin": "model_file",
}
CONFIG_HINTS = {"invalid_device", "invalid_api_config", "unsupported_backend", "serve_auth_required", "serve_dependencies_missing"}


def log_exception(exc: BaseException, operation: str) -> None:
    """Keep a useful stack and fixed hints without trying to scrub arbitrary
    exception messages (which may contain entire transcripts or credentials).
    Chained exception messages are intentionally never rendered verbatim.
    """
    operation = operation if operation in {"load", "transcribe", "startup", "internal"} else "internal"
    chain = []
    seen: set[int] = set()
    current: BaseException | None = exc
    while current is not None and id(current) not in seen and len(chain) < 8:
        seen.add(id(current))
        frames = []
        for frame, line in traceback.walk_tb(current.__traceback__):
            module = frame.f_globals.get("__name__", "")
            local = module in {"asr_worker.backends", "asr_worker.worker", "asr_worker.api", "asr_worker.download"}
            frames.append({"module": module if local else "dependency", "function": frame.f_code.co_name if local else "dependency_call", "line": line})
        try:
            details = str(current).lower()
        except BaseException:
            details = ""
        code = getattr(current, "code", None)
        hint = code if isinstance(code, str) and code in CONFIG_HINTS else next((hint for marker, hint in HINTS.items() if marker in details), "details_redacted")
        chain.append({"type": type(current).__name__ if type(current).__name__ in ERROR_TYPES else "Exception", "hint": hint, "frames": frames[-24:]})
        current = current.__cause__ or current.__context__
    print(PREFIX + json.dumps({"event": "exception", "operation": operation, "chain": chain}, separators=(",", ":")), file=sys.stderr, flush=True)
