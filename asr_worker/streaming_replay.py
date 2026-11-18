"""Bridge the frozen benchmark replay protocol to the real streaming worker.

Run with ``uv run --extra sherpa-onnx -m asr_worker.streaming_replay``.
Cached weights only unless --allow-download is explicitly supplied. This is an
ASR engine adapter; it does not measure Windows target insertion or IME.
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import sys

from .backends import BackendError
from .sherpa_onnx_runtime import NEMOTRON_STREAMING, SherpaOnnxBackend
from .streaming import StreamingSession
from .worker import _configure_protocol_stdio, _isolate_protocol_stdout


class PcmResampler:
    """Continuous rational linear interpolation, independent of replay chunks."""
    def __init__(self, rate: int) -> None:
        self.rate, self.base, self.total, self.next = rate, 0, 0, 0
        self.pending: list[float] = []

    def push(self, samples: list[float], final: bool = False) -> list[float]:
        self.pending.extend(samples)
        self.total += len(samples)
        output = []
        while True:
            left, remainder = divmod(self.next * self.rate, 16000)
            if left >= self.total or (not final and left + 1 >= self.total):
                break
            right = min(left + 1, self.total - 1)
            a, b = self.pending[left - self.base], self.pending[right - self.base]
            output.append(a + (b - a) * remainder / 16000)
            self.next += 1
        keep = min(self.next * self.rate // 16000, self.total)
        del self.pending[:keep - self.base]
        self.base = keep
        return output


class ReplayAdapter:
    def __init__(self, backend: SherpaOnnxBackend) -> None:
        self.backend = backend
        self.session = StreamingSession(backend)
        self.rate = self.source_offset = self.offset = 0
        self.resampler: PcmResampler | None = None

    def submit(self, samples: list[float]) -> str:
        text = ""
        for index in range(0, len(samples), 32000):
            chunk = samples[index:index + 32000]
            result = self.session.handle({"command": "stream_push", "session_id": self.session.session_id,
                                          "offset": self.offset, "samples": chunk})
            self.offset = result["offset"]
            text = result["text"]
        return text

    def handle(self, request: dict) -> dict:
        import numpy as np

        stream_id = request.get("stream_id")
        command = request.get("command")
        if command == "start":
            rate = request.get("sample_rate")
            if type(rate) is not int or not 8000 <= rate <= 192000 or request.get("channels") != 1 or request.get("sample_width") != 2:
                raise BackendError("invalid_request", "Replay requires mono PCM16 at 8..192 kHz")
            self.session.handle({"command": "stream_start", "session_id": stream_id,
                                 "sample_rate": 16000, "language": request.get("language")})
            self.rate, self.source_offset, self.offset = rate, 0, 0
            self.resampler = PcmResampler(rate)
            return {"stream_id": stream_id, "event": "ready", "backend": self.backend.backend_name,
                    "runtime": self.backend.runtime_label, "model": self.backend.model_name,
                    "model_revision": NEMOTRON_STREAMING.revision, "quantization": "INT8",
                    "device": self.backend.device, "compute_type": self.backend.compute_type}
        if self.resampler is None or stream_id != self.session.session_id:
            raise BackendError("stream_missing", "Replay stream is not active")
        if command == "audio":
            frames, offset, encoded = request.get("frames"), request.get("offset_frames"), request.get("pcm_s16le")
            if type(frames) is not int or not 0 < frames <= self.rate * 2 or type(offset) is not int or offset != self.source_offset:
                raise BackendError("stream_sequence", "Replay PCM is not contiguous or exceeds two seconds")
            if not isinstance(encoded, str) or len(encoded) > frames * 4 + 4:
                raise BackendError("invalid_request", "Replay PCM payload is invalid")
            try:
                data = base64.b64decode(encoded, validate=True)
            except ValueError as exc:
                raise BackendError("invalid_request", "Replay PCM payload is invalid") from exc
            if len(data) != frames * 2:
                raise BackendError("invalid_request", "Replay PCM frame count does not match payload")
            samples = (np.frombuffer(data, dtype="<i2").astype(np.float32) / 32768).tolist()
            self.source_offset += frames
            text = self.submit(self.resampler.push(samples))
            return {"stream_id": stream_id, "event": "partial", "text": text}
        if command == "flush":
            self.submit(self.resampler.push([], final=True))
            result = self.session.handle({"command": "stream_flush", "session_id": stream_id, "offset": self.offset})
            self.resampler = None
            return {"stream_id": stream_id, "event": "final", "text": result["text"]}
        raise BackendError("unsupported_operation", "Unknown replay command")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allow-download", action="store_true", help="Explicitly allow missing pinned model files to download")
    parser.add_argument("--device", choices=("auto", "cpu", "cuda"))
    args = parser.parse_args()
    _configure_protocol_stdio()
    output = _isolate_protocol_stdout()
    os.environ["ASR_MODEL_ID"] = NEMOTRON_STREAMING.id
    if not args.allow_download:
        os.environ["ASR_OFFLINE_ONLY"] = "1"
    if args.device:
        os.environ["ASR_DEVICE"] = args.device
    backend = SherpaOnnxBackend()
    adapter = ReplayAdapter(backend)
    stream_id = None
    try:
        backend.load("bf16")  # The model's weight format is pinned INT8.
        for line in sys.stdin:
            request = json.loads(line)
            if not isinstance(request, dict):
                raise BackendError("invalid_request", "Replay request must be an object")
            stream_id = request.get("stream_id")
            output.write(json.dumps(adapter.handle(request), ensure_ascii=False) + "\n")
            output.flush()
        return 0
    except (BackendError, ValueError, OSError):
        # Neither transcripts nor arbitrary exception text belong in diagnostics.
        output.write(json.dumps({"stream_id": stream_id, "event": "error", "message": "Streaming replay failed; verify the cached model, runtime, device and protocol."}) + "\n")
        output.flush()
        return 1
    finally:
        adapter.session.close()
        backend.unload()


if __name__ == "__main__":
    raise SystemExit(main())
