#!/usr/bin/env python3
"""ASR benchmark harness for the JSONL worker protocol.

This script drives an ``asr_worker`` subprocess over the JSON Lines protocol
(``load`` -> ``transcribe`` x N -> ``shutdown``) and records timing statistics.

It distinguishes two kinds of timing:

* ``duration_ms`` -- the pure ASR processing time reported by the worker
  itself (does not include JSON serialization or pipe latency).
* ``wall_ms`` -- the full request/response round trip measured by this script
  (includes IPC, JSON encode/decode and the worker compute time).

Only the Python standard library is used so the harness runs anywhere the
worker runs, including the offline WSL2 dev box using the ``mock`` backend.

Phase 0 gate context: see ``docs/benchmarks.md``. The headline KPI being
validated is "3 second utterance, warm, stop -> insertion complete p50 < 1.5s,
p95 < 2.5s", of which the ASR component budget is roughly p50 < 800ms.

Examples
--------
Smoke test (offline, generates a synthetic sine wav, mock backend)::

    python scripts/bench_asr.py --backend mock --smoke

Real run on the Windows GPU box::

    python scripts/bench_asr.py --backend vibevoice --quantization 4bit \\
        --audio bench_audio/ja_3s.wav --audio bench_audio/en_3s.wav \\
        --repeat 10 --cold-runs 3 --json > results/vibevoice_4bit.json
"""

from __future__ import annotations

import argparse
import contextlib
import json
import math
import os
import struct
import subprocess
import sys
import tempfile
import threading
import time
import wave
from collections import deque
from pathlib import Path
from typing import Any

# Default per-request timeout (seconds). Generous because a cold load of a
# large LLM-style ASR model can take a while, and a 60s utterance transcription
# is itself non-trivial. Override with --timeout.
DEFAULT_TIMEOUT_S = 300.0

# Worker stderr lines kept for failure reports. The rest is read and dropped so
# the worker never blocks on a full stderr pipe.
STDERR_TAIL_LINES = 200

# Synthetic audio parameters for --smoke.
SMOKE_SAMPLE_RATE = 24_000
SMOKE_SECONDS = 3.0
SMOKE_FREQ_HZ = 440.0


class BenchError(RuntimeError):
    """Raised for any unrecoverable benchmark failure (clean exit, no hang)."""


# --------------------------------------------------------------------------- #
# Statistics helpers (stdlib only; we avoid `statistics.quantiles` to keep the
# percentile definition explicit and stable across sample sizes).
# --------------------------------------------------------------------------- #
def percentile(values: list[float], pct: float) -> float:
    """Return the ``pct`` (0-100) percentile using linear interpolation.

    Matches the "inclusive" / numpy-default ('linear') method so results line
    up with what an analyst would compute in a notebook.
    """
    if not values:
        return float("nan")
    if len(values) == 1:
        return values[0]
    ordered = sorted(values)
    rank = (pct / 100.0) * (len(ordered) - 1)
    low = math.floor(rank)
    high = math.ceil(rank)
    if low == high:
        return ordered[int(rank)]
    frac = rank - low
    return ordered[low] + (ordered[high] - ordered[low]) * frac


def summarize(values: list[float]) -> dict[str, float]:
    return {
        "count": len(values),
        "min": min(values) if values else float("nan"),
        "p50": percentile(values, 50),
        "p95": percentile(values, 95),
        "max": max(values) if values else float("nan"),
    }


# --------------------------------------------------------------------------- #
# Synthetic audio generation (for --smoke).
# --------------------------------------------------------------------------- #
def write_sine_wav(path: Path, *, seconds: float, sample_rate: int, freq_hz: float) -> None:
    """Write a mono 16-bit PCM sine wave WAV using only the stdlib `wave`."""
    n_frames = int(seconds * sample_rate)
    amplitude = 0.3 * 32767  # keep some headroom so nothing clips
    with wave.open(str(path), "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)  # 16-bit
        wav.setframerate(sample_rate)
        frames = bytearray()
        for i in range(n_frames):
            sample = int(amplitude * math.sin(2.0 * math.pi * freq_hz * (i / sample_rate)))
            frames += struct.pack("<h", sample)
        wav.writeframes(bytes(frames))


# --------------------------------------------------------------------------- #
# Worker session: a single subprocess lifecycle (load -> transcribe* -> shutdown)
# --------------------------------------------------------------------------- #
class WorkerSession:
    """Manages one worker subprocess and the JSONL conversation with it."""

    def __init__(self, cmd: list[str], *, timeout_s: float, env: dict[str, str] | None = None) -> None:
        self.cmd = cmd
        self.timeout_s = timeout_s
        self.env = env
        self.proc: subprocess.Popen[str] | None = None
        self._next_id = 0
        self._stderr_lines: deque[str] = deque(maxlen=STDERR_TAIL_LINES)
        self._stderr_thread: threading.Thread | None = None

    def __enter__(self) -> "WorkerSession":
        try:
            self.proc = subprocess.Popen(
                self.cmd,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                # The protocol on stdout is UTF-8, but stderr is whatever the
                # worker's libraries write, often in the console code page.
                # Undecodable bytes must not stop the stderr drain.
                errors="replace",
                bufsize=1,  # line-buffered
                env=self.env,
            )
        except (OSError, ValueError) as exc:
            raise BenchError(f"failed to start worker {self.cmd!r}: {exc}") from exc
        # Model libraries write warnings and progress bars to stderr. Unread,
        # they fill the pipe buffer and block the worker, which then looks like
        # a model hang ("worker timed out").
        self._stderr_thread = threading.Thread(target=self._drain_stderr, daemon=True)
        self._stderr_thread.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def _alloc_id(self) -> int:
        self._next_id += 1
        return self._next_id

    def _drain_stderr(self) -> None:
        """Keep reading worker stderr, remembering only the most recent lines.

        The worker blocks once the stderr pipe is full, so this must keep
        reading until end of file whatever the content is.
        """
        stream = self.proc.stderr if self.proc is not None else None
        if stream is None:
            return
        try:
            for line in stream:
                self._stderr_lines.append(line)
            return
        except Exception as exc:  # noqa: BLE001 - must not end the drain
            if stream.closed:
                return  # close() ended the session
            self._stderr_lines.append(
                f"[bench_asr: cannot read worker stderr as text ({type(exc).__name__}); "
                "discarding the rest]\n"
            )
        # Keep the pipe empty even though the rest cannot be shown.
        with contextlib.suppress(Exception):
            while stream.buffer.read(65536):
                pass

    def _stderr_tail(self) -> str:
        """The most recent stderr lines the worker emitted, for diagnostics."""
        thread = self._stderr_thread
        if thread is not None and self.proc is not None:
            # Failure reports often come from a worker that is still exiting.
            # Once it has exited, the drain thread reaches end of file, so its
            # last lines are included.
            with contextlib.suppress(subprocess.TimeoutExpired):
                self.proc.wait(timeout=1)
            if self._poll() is not None:
                thread.join(timeout=2)
        return "".join(list(self._stderr_lines))

    def request(self, payload: dict[str, Any]) -> tuple[dict[str, Any], float]:
        """Send one JSONL request, return (response, wall_ms).

        Uses a watchdog thread to enforce the timeout so a wedged worker can
        never hang the harness indefinitely.
        """
        if self.proc is None or self.proc.stdin is None or self.proc.stdout is None:
            raise BenchError("worker process is not running")

        payload = {"id": self._alloc_id(), **payload}
        line = json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n"

        # perf_counter uses QueryPerformanceCounter on Windows. monotonic uses
        # GetTickCount64 there and would quantize fast requests to 0/16 ms.
        started = time.perf_counter()
        try:
            self.proc.stdin.write(line)
            self.proc.stdin.flush()
        except (BrokenPipeError, OSError) as exc:
            raise BenchError(
                f"worker closed its input early (exit={self._poll()}). stderr:\n{self._stderr_tail()}"
            ) from exc

        response_line = self._read_line_with_timeout()
        wall_ms = (time.perf_counter() - started) * 1000.0

        if response_line is None:
            self._kill()
            raise BenchError(
                f"worker timed out after {self.timeout_s:.0f}s on {payload.get('command')!r}; "
                f"process killed. stderr:\n{self._stderr_tail()}"
            )
        if response_line == "":
            raise BenchError(
                f"worker exited without responding to {payload.get('command')!r} "
                f"(exit={self._poll()}). stderr:\n{self._stderr_tail()}"
            )

        try:
            response = json.loads(response_line)
        except json.JSONDecodeError as exc:
            raise BenchError(f"worker emitted non-JSON line: {response_line!r}") from exc

        if not isinstance(response, dict) or not response.get("ok", False):
            err = response.get("error", {}) if isinstance(response, dict) else {}
            raise BenchError(
                f"worker error on {payload.get('command')!r}: "
                f"{err.get('code', 'unknown')}: {err.get('message', response_line)}"
            )
        return response, wall_ms

    def _read_line_with_timeout(self) -> str | None:
        """Read one stdout line, enforcing self.timeout_s.

        Returns the line (with trailing newline), "" on EOF, or None on timeout.
        Implemented with a reader thread because select() on pipes is not
        portable to Windows, and the real runs happen on Windows.
        """
        assert self.proc is not None and self.proc.stdout is not None
        result: dict[str, str | None] = {"line": None}

        def _reader() -> None:
            with contextlib.suppress(Exception):
                result["line"] = self.proc.stdout.readline()  # type: ignore[union-attr]

        thread = threading.Thread(target=_reader, daemon=True)
        thread.start()
        thread.join(self.timeout_s)
        if thread.is_alive():
            return None  # timed out; caller will kill the process
        return result["line"]

    def _poll(self) -> int | None:
        return self.proc.poll() if self.proc is not None else None

    def _kill(self) -> None:
        if self.proc is None:
            return
        with contextlib.suppress(Exception):
            self.proc.kill()
        with contextlib.suppress(Exception):
            self.proc.wait(timeout=5)

    def close(self) -> None:
        """Attempt a graceful shutdown, then make sure the process is gone."""
        if self.proc is None:
            return
        if self.proc.poll() is None and self.proc.stdin is not None:
            with contextlib.suppress(Exception):
                self.request({"command": "shutdown"})
        if self.proc.poll() is None:
            with contextlib.suppress(Exception):
                self.proc.wait(timeout=5)
        if self.proc.poll() is None:
            self._kill()
        if self._stderr_thread is not None:
            self._stderr_thread.join(timeout=2)
            self._stderr_thread = None
        # Drain pipes so the OS can reclaim the fds.
        for stream in (self.proc.stdin, self.proc.stdout, self.proc.stderr):
            with contextlib.suppress(Exception):
                if stream is not None:
                    stream.close()
        self.proc = None


# --------------------------------------------------------------------------- #
# Benchmark driver
# --------------------------------------------------------------------------- #
def build_worker_cmd(args: argparse.Namespace) -> list[str]:
    if args.worker_cmd:
        return args.worker_cmd
    return [sys.executable, "-m", "asr_worker", "--backend", args.backend]


def run_transcribe_loop(
    session: WorkerSession,
    audio_paths: list[Path],
    repeat: int,
    prompt: list[str] | None,
) -> list[dict[str, Any]]:
    """Run `repeat` transcriptions per audio file, return per-call records."""
    records: list[dict[str, Any]] = []
    for audio in audio_paths:
        for _ in range(repeat):
            payload: dict[str, Any] = {"command": "transcribe", "audio_path": str(audio)}
            if prompt:
                payload["prompt"] = prompt
            response, wall_ms = session.request(payload)
            records.append(
                {
                    "audio": str(audio),
                    "wall_ms": wall_ms,
                    "duration_ms": float(response.get("duration_ms", float("nan"))),
                    "text": response.get("text", ""),
                }
            )
    return records


def worker_env(args: argparse.Namespace) -> dict[str, str]:
    env = dict(os.environ)
    # Make the backend selection explicit for default-cmd invocations too.
    env.setdefault("ASR_WORKER_BACKEND", args.backend)
    # A piped Python stderr uses the Windows ANSI code page (cp932 on Japanese
    # systems); UTF-8 keeps warnings and tracebacks readable in failure reports.
    env.setdefault("PYTHONIOENCODING", "utf-8")
    return env


def run_benchmark(args: argparse.Namespace, audio_paths: list[Path]) -> dict[str, Any]:
    cmd = build_worker_cmd(args)
    env = worker_env(args)

    cold_load_ms: list[float] = []
    warm_records: list[dict[str, Any]] = []
    cold_records: list[dict[str, Any]] = []

    # --- Cold runs: each restarts the process so load time is included. ---
    for cold_i in range(args.cold_runs):
        with WorkerSession(cmd, timeout_s=args.timeout, env=env) as session:
            load_resp, load_wall_ms = session.request(
                {"command": "load", "quantization": args.quantization}
            )
            cold_load_ms.append(load_wall_ms)
            # One transcription per audio file counts as the "cold path"
            # (process start + load + first transcription).
            cold_records.extend(run_transcribe_loop(session, audio_paths, 1, args.prompt))
            if cold_i == 0:
                model_name = load_resp.get("model", "?")

    # --- Warm run: single process, repeated transcriptions (steady state). ---
    with WorkerSession(cmd, timeout_s=args.timeout, env=env) as session:
        warm_load_resp, warm_load_ms = session.request(
            {"command": "load", "quantization": args.quantization}
        )
        model_name = warm_load_resp.get("model", "?")
        warm_records = run_transcribe_loop(session, audio_paths, args.repeat, args.prompt)

    return {
        "config": {
            "backend": args.backend,
            "quantization": args.quantization,
            "worker_cmd": cmd,
            "audio": [str(p) for p in audio_paths],
            "repeat": args.repeat,
            "cold_runs": args.cold_runs,
            "timeout_s": args.timeout,
            "model": model_name,
            "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        },
        "warm": {
            "wall_ms": summarize([r["wall_ms"] for r in warm_records]),
            "duration_ms": summarize([r["duration_ms"] for r in warm_records]),
            "samples": warm_records,
        },
        "cold": {
            "load_ms": summarize(cold_load_ms),
            "wall_ms": summarize([r["wall_ms"] for r in cold_records]),
            "duration_ms": summarize([r["duration_ms"] for r in cold_records]),
            "samples": cold_records,
        },
    }


# --------------------------------------------------------------------------- #
# Reporting
# --------------------------------------------------------------------------- #
def _fmt(value: float) -> str:
    if value != value:  # NaN
        return "-"
    return f"{value:.0f}"


def render_markdown(result: dict[str, Any]) -> str:
    cfg = result["config"]
    warm = result["warm"]
    cold = result["cold"]
    lines: list[str] = []
    lines.append("# ASR benchmark result")
    lines.append("")
    lines.append(f"- backend: `{cfg['backend']}`  quantization: `{cfg['quantization']}`")
    lines.append(f"- model: `{cfg['model']}`")
    lines.append(f"- worker cmd: `{' '.join(cfg['worker_cmd'])}`")
    lines.append(f"- audio files: {len(cfg['audio'])}  repeat (warm): {cfg['repeat']}  cold runs: {cfg['cold_runs']}")
    lines.append(f"- timestamp: {cfg['timestamp']}")
    lines.append("")
    lines.append("## Timing (ms)")
    lines.append("")
    lines.append("| metric | count | min | p50 | p95 | max |")
    lines.append("|---|---:|---:|---:|---:|---:|")

    def row(label: str, s: dict[str, float]) -> str:
        return (
            f"| {label} | {int(s['count'])} | {_fmt(s['min'])} | "
            f"{_fmt(s['p50'])} | {_fmt(s['p95'])} | {_fmt(s['max'])} |"
        )

    lines.append(row("warm wall (round trip)", warm["wall_ms"]))
    lines.append(row("warm duration (ASR pure)", warm["duration_ms"]))
    lines.append(row("cold wall (round trip)", cold["wall_ms"]))
    lines.append(row("cold duration (ASR pure)", cold["duration_ms"]))
    lines.append(row("cold load (model load)", cold["load_ms"]))
    lines.append("")

    # Quick gate hint against the ASR component budget (p50 < 800ms warm).
    warm_p50 = warm["wall_ms"]["p50"]
    if warm_p50 == warm_p50:  # not NaN
        verdict = "PASS" if warm_p50 < 800 else "REVIEW"
        lines.append(
            f"> ASR component budget check (warm wall p50 < 800ms): "
            f"**{verdict}** (p50 = {_fmt(warm_p50)} ms)"
        )
        lines.append(
            "> Note: this is the ASR slice only. Compare against the full "
            "stop->insertion budget in docs/benchmarks.md before deciding the gate."
        )
    return "\n".join(lines)


# --------------------------------------------------------------------------- #
# Argument parsing / main
# --------------------------------------------------------------------------- #
def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Benchmark an ASR worker over the JSONL protocol.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--backend", default="mock", help="worker backend (default: mock)")
    parser.add_argument("--quantization", default="4bit", help="model quantization (default: 4bit)")
    parser.add_argument(
        "--audio",
        action="append",
        default=[],
        metavar="WAV",
        help="path to a wav file to transcribe; repeatable",
    )
    parser.add_argument("--repeat", type=int, default=5, help="warm transcriptions per audio (default: 5)")
    parser.add_argument(
        "--cold-runs",
        type=int,
        default=1,
        help="number of full process-restart cold measurements (default: 1)",
    )
    parser.add_argument(
        "--prompt",
        action="append",
        default=[],
        metavar="TERM",
        help="hotword/dictionary term passed as prompt; repeatable",
    )
    parser.add_argument(
        "--worker-cmd",
        nargs=argparse.REMAINDER,
        default=None,
        help="override the worker launch command (everything after this flag)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=DEFAULT_TIMEOUT_S,
        help=f"per-request timeout in seconds (default: {DEFAULT_TIMEOUT_S:.0f})",
    )
    parser.add_argument("--json", action="store_true", help="emit JSON instead of a markdown summary")
    parser.add_argument(
        "--smoke",
        action="store_true",
        help="generate a synthetic wav and run a self-contained mock benchmark",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    args.prompt = args.prompt or None

    try:
        if args.smoke:
            with tempfile.TemporaryDirectory(prefix="bench_asr_smoke_") as tmp:
                wav_path = Path(tmp) / "smoke_sine.wav"
                write_sine_wav(
                    wav_path,
                    seconds=SMOKE_SECONDS,
                    sample_rate=SMOKE_SAMPLE_RATE,
                    freq_hz=SMOKE_FREQ_HZ,
                )
                # Smoke mode is intentionally fast and offline-friendly.
                if args.backend != "mock":
                    print(
                        f"[bench_asr] note: --smoke forces backend 'mock' (was '{args.backend}')",
                        file=sys.stderr,
                    )
                    args.backend = "mock"
                args.timeout = min(args.timeout, 30.0)
                result = run_benchmark(args, [wav_path])
                _emit(result, args)
                return 0

        audio_paths = [Path(p) for p in args.audio]
        if not audio_paths:
            raise BenchError("no audio supplied; pass --audio WAV (or use --smoke)")
        missing = [p for p in audio_paths if not p.is_file()]
        if missing:
            raise BenchError("audio file(s) not found: " + ", ".join(str(p) for p in missing))

        result = run_benchmark(args, audio_paths)
        _emit(result, args)
        return 0

    except BenchError as exc:
        print(f"[bench_asr] error: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("[bench_asr] interrupted", file=sys.stderr)
        return 130


def _emit(result: dict[str, Any], args: argparse.Namespace) -> None:
    if args.json:
        print(json.dumps(result, ensure_ascii=False, indent=2))
    else:
        print(render_markdown(result))


if __name__ == "__main__":
    raise SystemExit(main())
