#!/usr/bin/env python3
"""ASR benchmark harness for the JSONL worker protocol.

This script drives an ``asr_worker`` subprocess over the JSON Lines protocol
(``load`` -> ``transcribe`` x N -> ``shutdown``) and records timing statistics.

It distinguishes two kinds of timing:

* ``duration_ms`` -- the pure ASR processing time reported by the worker
  itself (does not include JSON serialization or pipe latency).
* ``wall_ms`` -- the full request/response round trip measured by this script
  (includes IPC, JSON encode/decode and the worker compute time).

By default each ``transcribe`` request has the shape the app sends for final
recognition (``src-tauri/src/asr.rs`` ``transcribe_request``): ``prompt`` is
the dictionary rendered exactly as ``Storage::dictionary_asr_prompt_for``
renders it and ``language`` is the speech locale, both null when unset.
``--idle-gap-s`` and ``--live-preview`` reproduce idle time and serial
live-preview load before warm requests; ``--request-shape legacy`` keeps the
request earlier runs sent.

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
import csv
import hashlib
import importlib.util
import io
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
from dataclasses import dataclass, replace
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


def load_manifest(path: Path) -> tuple[dict[str, Any], dict[str, Any]]:
    """Share the scorer's loader, including when this script is loaded by file path."""
    module_name = "_bench_score_manifest"
    scorer = sys.modules.get(module_name)
    if scorer is None:
        spec = importlib.util.spec_from_file_location(module_name, Path(__file__).with_name("score_asr.py"))
        assert spec is not None and spec.loader is not None
        scorer = importlib.util.module_from_spec(spec)
        sys.modules[module_name] = scorer
        try:
            spec.loader.exec_module(scorer)
        except BaseException:
            sys.modules.pop(module_name, None)
            raise
    try:
        manifest = scorer.load_manifest(path)
    except (OSError, ValueError) as exc:
        raise BenchError(f"could not load manifest: {exc}") from exc
    return manifest, scorer.manifest_conditions(manifest, path)


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
        # A text-mode pipe writes os.linesep (CRLF on Windows); the app ends
        # each request with a bare LF, so requests are written the same way.
        if self.proc.stdin is not None:
            self.proc.stdin = io.TextIOWrapper(
                self.proc.stdin.detach(),
                encoding="utf-8",
                newline="\n",
                line_buffering=True,
                write_through=True,
            )
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
        # The app serializes requests with serde_json, whose default map sorts
        # keys; sorting here makes a production-shape line byte-comparable.
        line = json.dumps(payload, ensure_ascii=False, separators=(",", ":"), sort_keys=True) + "\n"

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
# Production request parity
# --------------------------------------------------------------------------- #
# Characters Rust's `str::trim` removes (Unicode White_Space). Python's
# argument-less `str.strip()` also removes U+001C..U+001F, so the set is explicit.
_RUST_WHITESPACE = (
    "\t\n\x0b\x0c\r \x85\xa0        "
    "        　"
)

# Column order of the app's dictionary CSV import (`storage.rs`
# `import_dictionary_csv`). The bench also accepts a trailing `source` column.
DICTIONARY_CSV_HEADER = ["reading", "surface", "category", "aliases", "priority", "app_scope"]


def rust_trim(value: str) -> str:
    return value.strip(_RUST_WHITESPACE)


@dataclass(frozen=True)
class DictionaryEntry:
    """The fields of a stored dictionary entry that the ASR prompt reads."""

    reading: str
    surface: str
    aliases: tuple[str, ...] = ()
    priority: int = 0
    app_scope: str | None = None
    source: str = "manual"


def _normalized_scope(scope: str | None) -> str | None:
    """`storage.rs` `normalized_scope`: blank and `global` mean unscoped."""
    if scope is None:
        return None
    scope = rust_trim(scope)
    return scope if scope and scope != "global" else None


def _scope_matches(scope: str | None, app_key: str | None, app_category: str | None) -> bool:
    """`storage.rs` `scope_matches` with an optional app context."""
    if scope is None or scope == "" or scope == "global":
        return True
    if app_key is None and app_category is None:
        return False  # no app context (Edit/Ask, or no target app)
    app_scope = f"app:{app_key}" if app_key is not None else None
    return app_scope == scope or scope == f"category:{app_category or ''}"


def render_asr_prompt(
    entries: list[DictionaryEntry],
    backend: str,
    *,
    app_key: str | None = None,
    app_category: str | None = None,
) -> str | None:
    """Render the dictionary the way the app builds a transcription prompt.

    A line-for-line port of `Storage::dictionary_asr_prompt_for`
    (`src-tauri/src/storage.rs`): entries are read in `priority DESC, surface
    ASC` order and filtered by scope; faster-whisper gets comma-separated
    hotwords within 200 characters, every other backend gets one
    `reading => surface (aliases: ...)` line per entry within 1200 characters.
    """
    ordered = sorted(entries, key=lambda entry: (-entry.priority, entry.surface))
    selected = [entry for entry in ordered if _scope_matches(entry.app_scope, app_key, app_category)]
    terms: list[str] = []
    length = 0

    def push_within(term: str, separator: int, budget: int) -> None:
        nonlocal length
        size = len(term) + (separator if terms else 0)
        if length + size <= budget:
            length += size
            terms.append(term)

    if backend == "faster-whisper":
        seen: set[str] = set()
        candidates = (
            [entry.surface for entry in selected]
            + [alias for entry in selected for alias in entry.aliases]
            + [entry.reading for entry in selected if entry.source == "manual"]
        )
        for candidate in candidates:
            term = rust_trim(candidate)
            if term and term.lower() not in seen:
                seen.add(term.lower())
                push_within(term, 2, 200)
        return ", ".join(terms) if terms else None
    for entry in selected:
        aliases = f" (aliases: {', '.join(entry.aliases)})" if entry.aliases else ""
        if entry.source == "auto":
            term = f"{entry.surface}{aliases}"
        else:
            term = f"{entry.reading} => {entry.surface}{aliases}"
        push_within(term, 1, 1200)
    return "\n".join(terms) if terms else None


def _validated_text(value: str, name: str, row: int) -> str:
    trimmed = rust_trim(value)
    if not trimmed or any(ord(char) < 0x20 or 0x7F <= ord(char) <= 0x9F for char in trimmed):
        raise BenchError(f"dictionary CSV row {row}: {name} is empty or contains control characters")
    return trimmed


def load_dictionary_csv(path: Path) -> list[DictionaryEntry]:
    """Read dictionary entries in the app's CSV import format.

    The header must be `reading,surface,category,aliases,priority,app_scope`
    (aliases separated by `|`, a blank priority meaning 0), optionally followed
    by a `source` column (`manual`, the default, or `auto`) so confirmed
    candidates, which the app stores as `auto`, can be reproduced too.
    """
    try:
        with path.open(encoding="utf-8-sig", newline="") as stream:
            rows = list(csv.reader(stream))
    except (OSError, UnicodeDecodeError, csv.Error) as exc:
        raise BenchError(f"could not read dictionary CSV {path}: {exc}") from exc
    if not rows:
        raise BenchError("dictionary CSV is empty; it needs the app's import header")
    header = rows[0]
    with_source = header == [*DICTIONARY_CSV_HEADER, "source"]
    if header != DICTIONARY_CSV_HEADER and not with_source:
        raise BenchError(
            "dictionary CSV header must be " + ",".join(DICTIONARY_CSV_HEADER) + "[,source]"
        )
    entries: list[DictionaryEntry] = []
    for index, row in enumerate(rows[1:], start=2):
        if not row:
            continue
        if len(row) != len(header):
            raise BenchError(f"dictionary CSV row {index} has an invalid field count")
        priority_cell = rust_trim(row[4])
        try:
            priority = int(priority_cell) if priority_cell else 0
        except ValueError as exc:
            raise BenchError(f"dictionary CSV row {index} has an invalid priority") from exc
        aliases = tuple(rust_trim(alias) for alias in row[3].split("|")) if rust_trim(row[3]) else ()
        if any(not alias for alias in aliases):
            raise BenchError(f"dictionary CSV row {index}: aliases must be non-empty")
        source = rust_trim(row[6]) if with_source else "manual"
        if source not in ("manual", "auto"):
            raise BenchError(f"dictionary CSV row {index}: source must be manual or auto")
        entries.append(
            DictionaryEntry(
                reading=_validated_text(row[0], "reading", index),
                surface=_validated_text(row[1], "surface", index),
                aliases=aliases,
                priority=priority,
                app_scope=_normalized_scope(row[5]),
                source=source,
            )
        )
    return entries


def prompt_terms_as_entries(terms: list[str] | None) -> list[DictionaryEntry]:
    """`--prompt` terms as manual global entries whose reading is the surface."""
    entries = []
    for term in terms or []:
        term = rust_trim(term)
        if term:
            entries.append(DictionaryEntry(reading=term, surface=term))
    return entries


@dataclass(frozen=True)
class LivePreviewLoad:
    """Live-preview requests issued while a take records (`live_dictation.rs`).

    The production loop is serial: sleep `interval_s`, prepare the window
    (`prep_s`), send one preview request and wait for it, repeat. Stop ends
    the sleep at once, but a preparation or a preview request already running
    finishes first, and only then is the final request sent (`stop.rs` joins
    the live task before final recognition). The bench decodes on every
    iteration, the upper bound of the app's segmentation gate.
    """

    interval_s: float = 0.150
    prep_s: float = 0.0
    recording_s: float | None = None  # None: the final audio's duration
    greedy: bool = False
    audio: Path | None = None  # None: the final audio


@dataclass(frozen=True)
class RequestConditions:
    """Everything that shapes a transcription request and its timing."""

    shape: str = "production"  # or "legacy": the pre-parity request shape
    prompt: str | list[str] | None = None
    language: str | None = None
    idle_gap_s: float = 0.0
    live: LivePreviewLoad | None = None
    item_languages: dict[Path, str] | None = None


def transcribe_payload(audio: Path, conditions: RequestConditions, *, preview: bool = False) -> dict[str, Any]:
    """A `transcribe` request without its id.

    The production shape matches `transcribe_request` in
    `src-tauri/src/asr.rs`: `prompt` and `language` are always present
    (null when unset) and only a greedy live-preview request carries
    `decoding`. The legacy shape is what earlier bench runs sent.
    """
    if conditions.shape == "legacy":
        payload: dict[str, Any] = {"command": "transcribe", "audio_path": str(audio)}
        if conditions.prompt:
            payload["prompt"] = conditions.prompt
        if conditions.language is not None:
            payload["language"] = conditions.language
        return payload
    payload = {
        "command": "transcribe",
        "audio_path": str(audio),
        "prompt": conditions.prompt,
        "language": conditions.language,
    }
    if preview and conditions.live is not None and conditions.live.greedy:
        payload["decoding"] = "greedy"
    return payload


def wav_duration_s(path: Path) -> float:
    try:
        with wave.open(str(path), "rb") as wav:
            return wav.getnframes() / float(wav.getframerate())
    except (OSError, EOFError, wave.Error, ZeroDivisionError) as exc:
        raise BenchError(f"could not read the duration of {path}: {exc}") from exc


def wav_format(path: Path) -> dict[str, int] | None:
    """Sample rate, channels and sample width, to compare with the app's WAVs."""
    try:
        with wave.open(str(path), "rb") as wav:
            return {
                "sample_rate": wav.getframerate(),
                "channels": wav.getnchannels(),
                "sample_width_bytes": wav.getsampwidth(),
            }
    except (OSError, EOFError, wave.Error):
        return None


# Clock and sleep seams, replaced by tests with a simulated clock.
_now = time.perf_counter
_sleep = time.sleep


def _wait_until(deadline: float) -> None:
    remaining = deadline - _now()
    if remaining > 0:
        _sleep(remaining)


def simulate_live_preview(
    session: WorkerSession, audio: Path, conditions: RequestConditions
) -> dict[str, Any]:
    """Run the live-preview loop for one take and return how stop was delayed.

    `live_drain_ms` is the time from the simulated stop until the final
    request may be sent: the remainder of a preparation or preview request
    that was running when stop arrived (0 when stop interrupted the sleep).
    """
    live = conditions.live
    assert live is not None
    recording_s = live.recording_s if live.recording_s is not None else wav_duration_s(audio)
    preview_audio = live.audio or audio
    payload = transcribe_payload(preview_audio, conditions, preview=True)
    preview_wall_ms: list[float] = []
    stop_at = _now() + recording_s
    while True:
        now = _now()
        if now + live.interval_s >= stop_at:
            # Stop interrupts the sleep; nothing is in flight.
            _wait_until(stop_at)
            drain_end = stop_at
            break
        _sleep(live.interval_s)
        if live.prep_s > 0:
            _sleep(live.prep_s)
            if _now() >= stop_at:
                # The preparation finishes, then the loop sees stop.
                drain_end = _now()
                break
        _, wall_ms = session.request(payload)
        preview_wall_ms.append(wall_ms)
        if _now() >= stop_at:
            drain_end = _now()
            break
    return {
        "live_drain_ms": max(0.0, drain_end - stop_at) * 1000.0,
        "preview_wall_ms": preview_wall_ms,
    }


def run_transcribe_loop(
    session: WorkerSession,
    audio_paths: list[Path],
    repeat: int,
    conditions: RequestConditions,
    record_device: bool = False,
) -> list[dict[str, Any]]:
    """Run `repeat` transcriptions per audio file, return per-call records.

    Before each request the worker idles for `conditions.idle_gap_s` and, with
    live-preview load, serves preview requests for the simulated recording.
    Neither is included in `wall_ms`.
    """
    records: list[dict[str, Any]] = []
    for audio in audio_paths:
        item_conditions = conditions
        if conditions.item_languages is not None and conditions.language is None:
            item_conditions = replace(conditions, language=conditions.item_languages[audio])
        for _ in range(repeat):
            if conditions.idle_gap_s > 0:
                _sleep(conditions.idle_gap_s)
            live = simulate_live_preview(session, audio, item_conditions) if conditions.live else None
            response, wall_ms = session.request(transcribe_payload(audio, item_conditions))
            record: dict[str, Any] = {
                "audio": str(audio),
                "wall_ms": wall_ms,
                "duration_ms": float(response.get("duration_ms", float("nan"))),
                "text": response.get("text", ""),
            }
            if conditions.item_languages is not None:
                record["language"] = item_conditions.language
            if live is not None:
                record["live_drain_ms"] = live["live_drain_ms"]
                record["stop_to_final_ms"] = live["live_drain_ms"] + wall_ms
                record["preview_requests"] = len(live["preview_wall_ms"])
                record["preview_wall_ms"] = live["preview_wall_ms"]
            records.append(record)
            if record_device:
                # Query after every inference: auto can switch device during a
                # request. Health overhead is excluded from that request's timer.
                response, _ = session.request({"command": "health"})
                health = response.get("health", {})
                records[-1]["device"] = health.get("device")
                records[-1]["compute_type"] = health.get("computeType")
    return records


def query_health(session: WorkerSession) -> dict[str, Any] | None:
    """Device and compute type the loaded worker actually uses, if it reports them."""
    try:
        response, _ = session.request({"command": "health"})
    except BenchError:
        return None  # e.g. a --worker-cmd worker without health support
    health = response.get("health")
    if not isinstance(health, dict):
        return None
    return {
        "backend": health.get("backend"),
        "device": health.get("device"),
        "compute_type": health.get("computeType"),
    }


def build_worker_cmd(args: argparse.Namespace) -> list[str]:
    if args.worker_cmd:
        return args.worker_cmd
    return [sys.executable, "-m", "asr_worker", "--backend", args.backend]


def worker_env(args: argparse.Namespace) -> dict[str, str]:
    env = dict(os.environ)
    # Make the backend selection explicit for default-cmd invocations too.
    env.setdefault("ASR_WORKER_BACKEND", args.backend)
    # A piped Python stderr uses the Windows ANSI code page (cp932 on Japanese
    # systems); UTF-8 keeps warnings and tracebacks readable in failure reports.
    env.setdefault("PYTHONIOENCODING", "utf-8")
    profile = getattr(args, "whisper_profile", "standard")
    if args.backend == "faster-whisper":
        # An explicit profile prevents ambient settings from contaminating A/B runs.
        env["ASR_WHISPER_DECODE_PROFILE"] = profile
    elif profile != "standard":
        raise BenchError(f"--whisper-profile {profile} requires --backend faster-whisper")
    return env


def request_conditions(args: argparse.Namespace) -> tuple[RequestConditions, dict[str, Any]]:
    """Build the request conditions and their content-free description."""
    shape = getattr(args, "request_shape", "production")
    terms = getattr(args, "prompt", None) or None
    dictionary_csv = getattr(args, "dictionary_csv", None)
    app_key = getattr(args, "app_key", None)
    app_category = getattr(args, "app_category", None)
    language = getattr(args, "language", None)
    idle_gap_s = getattr(args, "idle_gap_s", 0.0) or 0.0
    live: LivePreviewLoad | None = None
    if getattr(args, "live_preview", False):
        audio = getattr(args, "live_preview_audio", None)
        live = LivePreviewLoad(
            interval_s=getattr(args, "live_interval_ms", 150.0) / 1000.0,
            prep_s=(getattr(args, "live_prep_ms", 0.0) or 0.0) / 1000.0,
            recording_s=getattr(args, "live_recording_s", None),
            greedy=getattr(args, "live_preview_greedy", False),
            audio=Path(audio) if audio else None,
        )
    elif getattr(args, "live_preview_greedy", False) or getattr(args, "live_preview_audio", None):
        raise BenchError("--live-preview-greedy and --live-preview-audio require --live-preview")

    prompt: str | list[str] | None
    entries: list[DictionaryEntry] = []
    if shape == "legacy":
        if dictionary_csv or app_key is not None or app_category is not None or live is not None:
            raise BenchError(
                "--dictionary-csv, --app-key, --app-category and --live-preview require --request-shape production"
            )
        prompt = terms
    else:
        if dictionary_csv:
            entries.extend(load_dictionary_csv(Path(dictionary_csv)))
        entries.extend(prompt_terms_as_entries(terms))
        prompt = render_asr_prompt(entries, args.backend, app_key=app_key, app_category=app_category)

    rendered = prompt if isinstance(prompt, str) else ("\n".join(prompt) if prompt else None)
    description = {
        "request_shape": shape,
        "prompt": {
            "dictionary_csv": str(dictionary_csv) if dictionary_csv else None,
            "terms": len(terms or []),
            "entries": len(entries),
            "app_key": app_key,
            "app_category": app_category,
            "format_backend": args.backend if shape == "production" else None,
            # The prompt holds dictionary text: record a fingerprint, not the text.
            "chars": len(rendered) if rendered else 0,
            "sha256": hashlib.sha256(rendered.encode("utf-8")).hexdigest() if rendered else None,
        },
        "idle_gap_s": idle_gap_s,
        "live_preview": None
        if live is None
        else {
            "interval_ms": live.interval_s * 1000.0,
            "prep_ms": live.prep_s * 1000.0,
            "recording_s": live.recording_s,
            "greedy": live.greedy,
            "audio": str(live.audio) if live.audio else None,
            "model": "serial: sleep, prepare, preview request; stop waits for an in-flight preview before the final request",
        },
    }
    return RequestConditions(shape=shape, prompt=prompt, language=language, idle_gap_s=idle_gap_s, live=live), description


def run_benchmark(args: argparse.Namespace, audio_paths: list[Path]) -> dict[str, Any]:
    cmd = build_worker_cmd(args)
    env = worker_env(args)
    conditions, condition_config = request_conditions(args)
    manifest = getattr(args, "manifest_data", None)
    if manifest is not None:
        conditions = replace(conditions, item_languages={Path(item["audio_path"]): item["language"] for item in manifest["items"]})
        condition_config["manifest"] = args.manifest_conditions
    condition_config["language_source"] = "argument" if conditions.language is not None else "manifest" if manifest is not None else "auto"
    # Cold runs measure process start + load + the first request only; the
    # idle gap and live-preview load apply to warm requests.
    cold_conditions = replace(conditions, idle_gap_s=0.0, live=None)
    record_device = getattr(args, "record_device", False)

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
            cold_records.extend(run_transcribe_loop(session, audio_paths, 1, cold_conditions, record_device))
            if cold_i == 0:
                model_name = load_resp.get("model", "?")

    # --- Warm run: single process, repeated transcriptions (steady state). ---
    with WorkerSession(cmd, timeout_s=args.timeout, env=env) as session:
        warm_load_resp, warm_load_ms = session.request(
            {"command": "load", "quantization": args.quantization}
        )
        model_name = warm_load_resp.get("model", "?")
        # Health after load is outside every timer.
        worker_health = query_health(session)
        warm_records = run_transcribe_loop(session, audio_paths, args.repeat, conditions, record_device)

    observed = sorted(
        {
            (record.get("device"), record.get("compute_type"))
            for record in warm_records + cold_records
            if "device" in record
        },
        key=lambda pair: (str(pair[0]), str(pair[1])),
    )
    warm: dict[str, Any] = {
        "wall_ms": summarize([r["wall_ms"] for r in warm_records]),
        "duration_ms": summarize([r["duration_ms"] for r in warm_records]),
    }
    if conditions.live is not None:
        warm["live_drain_ms"] = summarize([r["live_drain_ms"] for r in warm_records])
        warm["stop_to_final_ms"] = summarize([r["stop_to_final_ms"] for r in warm_records])
        warm["preview_wall_ms"] = summarize([ms for r in warm_records for ms in r["preview_wall_ms"]])
    warm["samples"] = warm_records

    return {
        "config": {
            "backend": args.backend,
            "whisper_decode_profile": env.get("ASR_WHISPER_DECODE_PROFILE") if args.backend == "faster-whisper" else None,
            "device_policy": env.get("ASR_DEVICE", "auto"),
            "record_device": record_device,
            "worker_health": worker_health,
            "observed_devices": [{"device": d, "compute_type": c} for d, c in observed] if record_device else None,
            "quantization": args.quantization,
            "language": getattr(args, "language", None),
            **condition_config,
            "worker_cmd": cmd,
            "audio": [str(p) for p in audio_paths],
            "audio_format": [wav_format(p) for p in audio_paths],
            "repeat": args.repeat,
            "cold_runs": args.cold_runs,
            "timeout_s": args.timeout,
            "model": model_name,
            "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        },
        "warm": warm,
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
    health = cfg.get("worker_health")
    if health:
        lines.append(
            f"- device after load: `{health.get('device') or '-'}`  compute type: `{health.get('compute_type') or '-'}`"
            f"  (policy `{cfg.get('device_policy', 'auto')}`)"
        )
    observed = cfg.get("observed_devices")
    if observed:
        lines.append(
            "- devices observed after requests: "
            + ", ".join(f"`{item['device'] or '-'}/{item['compute_type'] or '-'}`" for item in observed)
        )
    language = "per-item (manifest)" if cfg.get("language_source") == "manifest" else cfg.get("language") or "auto"
    lines.append(f"- language: `{language}`")
    if cfg.get("manifest"):
        lines.append(f"- manifest: `{cfg['manifest']['name']}`")
    prompt = cfg.get("prompt")
    if prompt is not None:
        lines.append(
            f"- request shape: `{cfg.get('request_shape')}`  prompt: {prompt['chars']} chars"
            f" from {prompt['entries']} entries"
            + (f" (sha256 `{prompt['sha256'][:12]}`)" if prompt.get("sha256") else "")
        )
    if cfg.get("idle_gap_s"):
        lines.append(f"- idle gap before each warm request: {cfg['idle_gap_s']:g} s")
    live = cfg.get("live_preview")
    if live:
        lines.append(
            f"- live preview load: every {live['interval_ms']:g} ms + {live['prep_ms']:g} ms prep, "
            f"{'greedy' if live['greedy'] else 'standard'} decoding, recording "
            + (f"{live['recording_s']:g} s" if live.get("recording_s") is not None else "= audio duration")
        )
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
    if "stop_to_final_ms" in warm:
        lines.append(row("warm live drain (stop wait)", warm["live_drain_ms"]))
        lines.append(row("warm stop -> final (drain + wall)", warm["stop_to_final_ms"]))
        lines.append(row("warm preview wall", warm["preview_wall_ms"]))
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
def language_argument(value: str) -> str:
    if not value.strip() or len(value) > 32:
        raise argparse.ArgumentTypeError("language must be a nonempty string of at most 32 characters")
    return value


def non_negative_float(value: str) -> float:
    try:
        number = float(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("must be a number") from exc
    if not math.isfinite(number) or number < 0:
        raise argparse.ArgumentTypeError("must be a finite number >= 0")
    return number


def positive_float(value: str) -> float:
    number = non_negative_float(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be > 0")
    return number


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Benchmark an ASR worker over the JSONL protocol.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--backend", default="mock", help="worker backend (default: mock)")
    parser.add_argument("--quantization", default="4bit", help="model quantization (default: 4bit)")
    parser.add_argument("--whisper-profile", choices=["standard", "fast", "greedy"], default="standard", help="faster-whisper decode profile: standard keeps library defaults (beam_size=5, VAD off); fast uses beam_size=1 and VAD; greedy changes only beam_size=1, keeping VAD off (default: standard)")
    parser.add_argument("--language", type=language_argument, help="speech locale passed unchanged to the worker, e.g. ja-JP (default: backend language detection)")
    parser.add_argument(
        "--audio",
        action="append",
        default=[],
        metavar="WAV",
        help="path to a wav file to transcribe; repeatable",
    )
    parser.add_argument("--manifest", metavar="FILE", help="benchmark audio and per-item languages from a frozen manifest")
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
        help="dictionary term (a manual entry whose reading is the term); repeatable",
    )
    parser.add_argument("--dictionary-csv", metavar="CSV", help="dictionary in the app's CSV import format (optional trailing source column); rendered into the prompt as the app does")
    parser.add_argument("--app-key", help="app context key for app:KEY dictionary scopes (default: no app context)")
    parser.add_argument("--app-category", help="app context category for category:NAME dictionary scopes")
    parser.add_argument("--request-shape", choices=["production", "legacy"], default="production", help="production: the app's transcribe request (prompt/language always present, prompt rendered as the app does); legacy: the request earlier bench runs sent (default: production)")
    parser.add_argument("--idle-gap-s", type=non_negative_float, default=0.0, metavar="SECONDS", help="idle time before each warm request, excluded from timings (default: 0)")
    parser.add_argument("--live-preview", action="store_true", help="before each warm request, simulate a recording with serial live-preview requests; stop waits for an in-flight preview")
    parser.add_argument("--live-preview-greedy", action="store_true", help="send decoding=greedy on preview requests (the app's lighter live preview setting)")
    parser.add_argument("--live-preview-audio", metavar="WAV", help="audio for preview requests (default: the final request's audio)")
    parser.add_argument("--live-recording-s", type=positive_float, metavar="SECONDS", help="simulated recording length before stop (default: the audio's duration)")
    parser.add_argument("--live-interval-ms", type=positive_float, default=150.0, metavar="MS", help="sleep between live iterations (default: 150, live_dictation.rs LIVE_INTERVAL)")
    parser.add_argument("--live-prep-ms", type=non_negative_float, default=0.0, metavar="MS", help="simulated audio preparation per live iteration (default: 0)")
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
    parser.add_argument("--output", metavar="PATH", help="write UTF-8 results to a new file; existing files are never overwritten")
    parser.add_argument("--record-device", action="store_true", help="query worker health after every transcription and record actual device/compute type (requires health support)")
    parser.add_argument(
        "--smoke",
        action="store_true",
        help="generate a synthetic wav and run a self-contained mock benchmark",
    )
    args = parser.parse_args(argv)
    if args.manifest and (args.audio or args.smoke):
        parser.error("--manifest cannot be combined with --audio or --smoke")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    args.prompt = args.prompt or None

    try:
        if args.output:
            try:
                # Reserve the destination before any model work. Exclusive
                # creation also closes the check/write race against another run.
                args.output_stream = Path(args.output).open("x", encoding="utf-8", newline="\n")
            except OSError as exc:
                raise BenchError("could not create a new result file; check its path and permissions") from exc
        if args.live_preview_audio and not Path(args.live_preview_audio).is_file():
            raise BenchError(f"live preview audio not found: {args.live_preview_audio}")
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

        if args.manifest:
            args.manifest_data, args.manifest_conditions = load_manifest(Path(args.manifest))
            audio_paths = [Path(item["audio_path"]) for item in args.manifest_data["items"]]
        else:
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
    finally:
        stream = getattr(args, "output_stream", None)
        if stream is not None:
            with contextlib.suppress(OSError):
                stream.close()


def _emit(result: dict[str, Any], args: argparse.Namespace) -> None:
    rendered = json.dumps(result, ensure_ascii=False, indent=2) if args.json else render_markdown(result)
    output = getattr(args, "output", None)
    if output:
        try:
            reserved = getattr(args, "output_stream", None)
            if reserved is not None:
                reserved.write(rendered + "\n")
                reserved.flush()
            else:
                with Path(output).open("x", encoding="utf-8", newline="\n") as stream:
                    stream.write(rendered + "\n")
        except OSError as exc:
            # Preserve measured output on stdout if storage fails mid-run.
            if getattr(args, "output_stream", None) is not None:
                print(rendered)
            raise BenchError("could not create a new result file; check its path and permissions") from exc
    else:
        print(rendered)


if __name__ == "__main__":
    raise SystemExit(main())
