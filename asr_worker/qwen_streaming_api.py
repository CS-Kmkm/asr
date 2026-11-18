"""Explicit loopback connector for Qwen's official vLLM streaming demo API.

The demo provides cumulative text but no verified model/coverage metadata.
Its partials are preview-only; the desktop always performs a full-recording
request at stop. This is buffered streaming, not encoder-cache streaming.
"""
from __future__ import annotations

import contextlib
import ipaddress
import os
import struct
import wave
from urllib.parse import urlsplit

from .backends import BackendError, ProgressReporting


class QwenStreamingApiBackend(ProgressReporting):
    backend_name = "openai-compatible"
    supports_streaming = True
    stream_final_reusable = False

    def __init__(self):
        self.base_url = os.environ.get("ASR_API_BASE_URL", "http://127.0.0.1:8000").rstrip("/")
        self.model_name = "qwen-demo:unverified"
        self.client = None

    def load(self, quantization):
        import httpx
        parsed = urlsplit(self.base_url)
        try:
            valid_host = ipaddress.ip_address(parsed.hostname or "").is_loopback
        except ValueError:
            valid_host = False
        if parsed.scheme != "http" or not valid_host or parsed.username or parsed.password or parsed.query or parsed.fragment or parsed.path not in ("", "/"):
            raise BackendError("invalid_api_config", "Qwen demo Streaming requires a numeric loopback HTTP origin without a path")
        if self.client is not None:
            self.client.close()
        self.client = httpx.Client(base_url=self.base_url + "/", timeout=30, follow_redirects=False, trust_env=False)

    def _request(self, path, **kwargs):
        if self.client is None:
            raise BackendError("model_not_loaded", "Load the Qwen streaming connector first")
        self.raise_if_cancelled()
        try:
            response = self.client.post(path, **kwargs)
            if response.status_code >= 300:
                raise BackendError("api_request_failed", f"Qwen streaming service returned HTTP {response.status_code}")
            payload = response.json()
            if not isinstance(payload, dict):
                raise ValueError("not an object")
            return payload
        except BackendError:
            raise
        except Exception as exc:
            raise BackendError("api_request_failed", "Qwen streaming service did not return a valid response") from exc

    def create_online_stream(self, language=None):
        if language not in (None, "", "auto"):
            raise BackendError("language_unsupported", "The official Qwen demo uses automatic language detection; select automatic speech language")
        payload = self._request("api/start")
        sid = payload.get("session_id")
        if not isinstance(sid, str) or not sid or len(sid) > 128:
            raise BackendError("invalid_api_response", "Qwen streaming service did not return a session ID")
        return {"id": sid, "closed": False}

    def decode_online(self, stream, samples, final):
        if stream["closed"]:
            raise BackendError("stream_missing", "Qwen streaming session is closed")
        if samples:
            payload = self._request("api/chunk", params={"session_id": stream["id"]}, content=struct.pack(f"<{len(samples)}f", *samples), headers={"Content-Type": "application/octet-stream"})
        else:
            payload = {"text": ""}
        if final:
            payload = self._request("api/finish", params={"session_id": stream["id"]})
            stream["closed"] = True
        if not isinstance(payload.get("text"), str):
            raise BackendError("invalid_api_response", "Qwen streaming service did not return cumulative text")
        return payload["text"]

    def close_online_stream(self, stream):
        if not stream["closed"] and self.client is not None:
            # The official API has finish but no cancel endpoint. Discard its
            # result; cleanup is bounded and never delivered to the target.
            stream["closed"] = True
            with contextlib.suppress(Exception):
                self.client.post("api/finish", params={"session_id": stream["id"]}, timeout=1)

    def transcribe(self, audio_path, prompt, language=None, decoding=None):
        from .streaming_replay import PcmResampler
        stream = self.create_online_stream(language)
        try:
            with wave.open(str(audio_path), "rb") as handle:
                if handle.getnchannels() != 1 or handle.getsampwidth() != 2 or handle.getcomptype() != "NONE":
                    raise BackendError("audio_unsupported", "Qwen streaming connector requires mono PCM16 WAV")
                rate = handle.getframerate()
                if not 8000 <= rate <= 192000:
                    raise BackendError("audio_unsupported", "Qwen streaming connector requires an 8..192 kHz sample rate")
                resampler = PcmResampler(rate)
                while data := handle.readframes(rate):
                    self.raise_if_cancelled()
                    samples = [value[0] / 32768 for value in struct.iter_unpack("<h", data)]
                    chunk = resampler.push(samples)
                    if chunk:
                        self.decode_online(stream, chunk, False)
                tail = resampler.push([], final=True)
                text = self.decode_online(stream, tail, True)
            return text, [{"start": 0.0, "end": 0.0, "speaker": None, "text": text}]
        finally:
            self.close_online_stream(stream)

    def unload(self):
        if self.client is not None:
            self.client.close()
            self.client = None
