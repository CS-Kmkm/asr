#!/usr/bin/env python3
"""Real-time PCM replay for local JSONL streaming adapters (see public_datasets.md)."""
from __future__ import annotations

import argparse
import base64
import json
import math
import queue
import sys
import threading
import time
import wave
from pathlib import Path

from bench_asr import BenchError, WorkerSession, load_manifest, summarize
from bench_data import write_json_new


IDENTITY_FIELDS = ("backend", "runtime", "model", "model_revision", "quantization", "device", "compute_type")


class StreamSession(WorkerSession):
    """Continuously timestamp events, including while the sender is sleeping."""

    def __enter__(self):
        super().__enter__()
        self.events = queue.Queue()
        self.reader = threading.Thread(target=self._receive, daemon=True)
        self.reader.start()
        return self

    def _receive(self):
        try:
            for line in self.proc.stdout:
                received = time.perf_counter()
                event = json.loads(line)
                if not isinstance(event, dict):
                    raise ValueError("adapter event must be an object")
                self.events.put((received, event))
        except (ValueError, OSError) as exc:
            self.events.put((time.perf_counter(), {"event": "error", "message": str(exc)}))
        finally:
            self.events.put((time.perf_counter(), {"event": "error", "message": "adapter stdout closed"}))

    def send(self, payload):
        # A blocked pipe is bounded too; reception and its clock stay independent.
        failure = []
        def write():
            try:
                self.proc.stdin.write(json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n")
                self.proc.stdin.flush()
            except (OSError, ValueError) as exc:
                failure.append(exc)
        writer = threading.Thread(target=write, daemon=True)
        writer.start()
        writer.join(self.timeout_s)
        if writer.is_alive():
            self._kill()
            raise BenchError("streaming adapter input timed out")
        if failure:
            raise BenchError(f"streaming adapter input failed: {failure[0]}")

    def receive(self, stream_id, deadline):
        remaining = deadline - time.perf_counter()
        if remaining <= 0:
            raise BenchError("streaming adapter event timed out")
        try:
            received, event = self.events.get(timeout=remaining)
        except queue.Empty as exc:
            raise BenchError("streaming adapter event timed out") from exc
        if event.get("event") == "error":
            raise BenchError(f"streaming adapter error: {event.get('message')}")
        if event.get("stream_id") != stream_id:
            raise BenchError("streaming adapter stream_id mismatch")
        return received, event

    def close(self):
        # This adapter protocol has no batch shutdown response.
        self._kill()
        reader = getattr(self, "reader", None)
        if reader is not None:
            reader.join(timeout=2)
        super().close()


def replay(session, audio: Path, language: str, stream_id: str, chunk_ms: int):
    with wave.open(str(audio), "rb") as wav:
        if wav.getnchannels() != 1 or wav.getsampwidth() != 2 or wav.getcomptype() != "NONE":
            raise BenchError("replay requires mono PCM16 WAV")
        rate, frames = wav.getframerate(), wav.getnframes()
        if not frames:
            raise BenchError("replay audio is empty")
        chunk_frames = max(1, rate * chunk_ms // 1000)
        session.send({"command": "start", "stream_id": stream_id, "sample_rate": rate,
                      "channels": 1, "sample_width": 2, "language": language})
        _, ready = session.receive(stream_id, time.perf_counter() + session.timeout_s)
        if ready.get("event") != "ready" or any(not isinstance(ready.get(k), str) or not ready[k] for k in IDENTITY_FIELDS):
            raise BenchError("adapter must return ready with all runtime/model/device identity fields")
        started = time.perf_counter()
        chunks = []
        offset = 0
        while offset < frames:
            data = wav.readframes(chunk_frames)
            count = len(data) // 2
            if not count:
                raise BenchError("truncated WAV")
            # A chunk is available only after its last source sample was captured.
            end = offset + count
            deadline = started + end / rate
            time.sleep(max(0, deadline - time.perf_counter()))
            sent = time.perf_counter()
            session.send({"command": "audio", "stream_id": stream_id, "offset_frames": offset,
                          "frames": count, "pcm_s16le": base64.b64encode(data).decode("ascii")})
            completed = time.perf_counter()
            chunks.append({"offset_frames": offset, "frames": count, "source_end_ms": end / rate * 1000,
                           "sent_ms": (sent - started) * 1000,
                           "send_completed_ms": (completed - started) * 1000,
                           "lateness_ms": max(0, sent - deadline) * 1000,
                           "write_completed_lateness_ms": max(0, completed - deadline) * 1000})
            offset = end
        flush_at = time.perf_counter()
        session.send({"command": "flush", "stream_id": stream_id})
        events = []
        first_partial = first_stable = None
        stable = ""
        retractions = 0
        limit = flush_at + session.timeout_s
        while True:
            received, event = session.receive(stream_id, limit)
            kind = event.get("event")
            text = event.get("text")
            if kind not in {"partial", "stable", "final"} or not isinstance(text, str):
                raise BenchError("adapter must emit partial/stable/final with cumulative text")
            elapsed = (received - started) * 1000
            if elapsed < 0:
                raise BenchError("adapter emitted text before source replay started")
            events.append({"event": kind, "text": text, "received_ms": elapsed})
            if kind == "partial" and text and first_partial is None:
                first_partial = elapsed
            if kind == "stable":
                if not text.startswith(stable):
                    retractions += 1
                stable = text
                if text and first_stable is None:
                    first_stable = elapsed
            if kind == "final":
                if received < flush_at:
                    raise BenchError("adapter finalized before flush")
                if not text.startswith(stable):
                    retractions += 1
                break
        duration = frames / rate
        return {"audio": str(audio), "language": language, "text": text, "identity": {k: ready[k] for k in IDENTITY_FIELDS},
                "wall_ms": (received - started) * 1000, "duration_ms": None,
                "audio_duration_s": duration, "replay_rtf": (received - started) / duration,
                "first_partial_ms": first_partial, "first_stable_ms": first_stable,
                "flush_to_final_ms": (received - flush_at) * 1000,
                "stable_retractions": retractions, "chunks": chunks, "events": events}


def mock_adapter():
    """Offline wiring check; never reads references or imitates ASR accuracy."""
    for line in sys.stdin:
        request = json.loads(line)
        command, stream_id = request["command"], request["stream_id"]
        event = {"stream_id": stream_id}
        if command == "start":
            event.update(event="ready", **{k: "synthetic" for k in IDENTITY_FIELDS})
        elif command == "audio":
            event.update(event="partial", text="mock")
            print(json.dumps(event), flush=True)
            event.update(event="stable")
        elif command == "flush":
            event.update(event="final", text="mock")
        else:
            raise ValueError("unknown mock command")
        print(json.dumps(event), flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mock-adapter", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--chunk-ms", type=int, default=100)
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--adapter-cmd", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    if args.mock_adapter:
        mock_adapter()
        return 0
    if not args.manifest or not args.output or not args.adapter_cmd:
        parser.error("--manifest, --output and --adapter-cmd are required")
    if args.chunk_ms <= 0 or not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("chunk-ms and timeout must be positive finite values")
    try:
        manifest, conditions = load_manifest(args.manifest)
        if args.output.exists():
            raise BenchError(f"output already exists: {args.output}")
        samples = []
        with StreamSession(args.adapter_cmd, timeout_s=args.timeout) as session:
            for item in manifest["items"]:
                samples.append(replay(session, Path(item["audio_path"]), item["language"], item["id"], args.chunk_ms))
        result = {"config": {"mode": "streaming-realtime", "manifest": conditions, "chunk_ms": args.chunk_ms,
                             "adapter_cmd": args.adapter_cmd, "identities": [s["identity"] for s in samples],
                             "timing": "source chunk end to local event receipt; startup excluded; no accelerated replay"},
                  "warm": {"wall_ms": summarize([s["wall_ms"] for s in samples]), "samples": samples},
                  "cold": {"samples": []}}
        # Publish a completed JSON atomically without replacing another run.
        write_json_new(args.output, result)
        return 0
    except (BenchError, OSError, ValueError, wave.Error) as exc:
        print(f"[bench_streaming] error: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("[bench_streaming] interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
