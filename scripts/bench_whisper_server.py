#!/usr/bin/env python3
"""Benchmark OpenWhispr's whisper-server using a frozen ASR manifest.

Uses only the standard library. Results are JSON compatible with score_asr.py;
server diagnostics are kept in a separate, uniquely named log file.
"""

from __future__ import annotations

import argparse
import contextlib
import ctypes
from ctypes import wintypes
from dataclasses import dataclass
import http.client
import importlib.util
import json
import math
import os
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
import wave
from pathlib import Path
from typing import Any, Iterator


IS_WINDOWS = os.name == "nt"


class BenchError(RuntimeError):
    """An unrecoverable benchmark failure."""


def load_manifest(path: Path) -> tuple[dict[str, Any], dict[str, Any]]:
    """Use the scorer's cached loader, including imports by file path."""
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


def default_threads() -> int:
    return max(4, min(12, (os.cpu_count() or 1) * 3 // 4))


def primary_language(locale: str) -> str:
    return locale.replace("_", "-").split("-", 1)[0].lower()


def free_port() -> int:
    for port in range(8178, 8200):
        with socket.socket() as sock:
            # Exclusive binding avoids selecting an occupied Windows port.
            if hasattr(socket, "SO_EXCLUSIVEADDRUSE"):
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
            try:
                sock.bind(("127.0.0.1", port))
            except OSError:
                continue
            return port
    raise BenchError("no free whisper-server port in 8178-8199")


def build_server_cmd(args: argparse.Namespace, port: int, language: str) -> list[str]:
    cmd = list(args.server_cmd) if args.server_cmd else [str(Path(args.server_exe).resolve())]
    cmd.extend(["--model", str(args.model.resolve()), "--host", "127.0.0.1", "--port", str(port)])
    threads = effective_threads(args)
    if threads is not None:
        cmd.extend(["--threads", str(threads)])
    cmd.extend(["--language", language, "--max-len", "4096"])
    return cmd


def effective_threads(args: argparse.Namespace) -> int | None:
    if args.threads is not None:
        return args.threads
    automatic = default_threads()
    return automatic if automatic > 4 else None


def request_fields(language: str, beam_size: int | None) -> dict[str, str]:
    fields = {"language": language, "entropy_thold": "2.8", "logprob_thold": "-1.25", "response_format": "json"}
    if beam_size is not None:
        fields["beam_size"] = str(beam_size)
    return fields


def multipart(audio: Path, fields: dict[str, str]) -> tuple[bytes, str]:
    boundary = "asr-bench-" + uuid.uuid4().hex
    parts = []
    for name, value in fields.items():
        parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode("utf-8"))
    # The server needs file bytes, not the machine-local filename. A fixed safe
    # name also handles manifest paths containing quotes or non-ASCII text.
    parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="audio.wav"\r\nContent-Type: audio/wav\r\n\r\n'.encode("ascii"))
    parts.extend([audio.read_bytes(), f"\r\n--{boundary}--\r\n".encode("ascii")])
    return b"".join(parts), f"multipart/form-data; boundary={boundary}"


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: ANN001, ANN201
        return None


def transcribe(port: int, item: dict[str, Any], language: str, args: argparse.Namespace) -> dict[str, Any]:
    try:
        body, content_type = multipart(Path(item["audio_path"]), request_fields(language, args.beam_size))
        request = urllib.request.Request(f"http://127.0.0.1:{port}/inference", data=body,
                                         headers={"Content-Type": content_type}, method="POST")
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        started = time.perf_counter()
        with opener.open(request, timeout=args.timeout) as response:
            if response.status != 200:
                raise BenchError(f"HTTP {response.status}")
            raw = response.read()
        wall_ms = (time.perf_counter() - started) * 1000.0
        data = json.loads(raw)
        if not isinstance(data, dict) or not isinstance(data.get("text"), str):
            raise BenchError("response must be a JSON object with string text")
        return {"audio": item["audio_path"], "language": language, "text": data["text"].strip(),
                "wall_ms": wall_ms, "duration_ms": float("nan")}
    except (OSError, ValueError, BenchError, urllib.error.URLError, http.client.HTTPException) as exc:
        raise BenchError(f"item {item['id']!r}: {exc}") from exc


class JobBasicLimits(ctypes.Structure):
    _fields_ = [("PerProcessUserTimeLimit", ctypes.c_longlong), ("PerJobUserTimeLimit", ctypes.c_longlong),
                ("LimitFlags", wintypes.DWORD), ("MinimumWorkingSetSize", ctypes.c_size_t),
                ("MaximumWorkingSetSize", ctypes.c_size_t), ("ActiveProcessLimit", wintypes.DWORD),
                ("Affinity", ctypes.c_size_t), ("PriorityClass", wintypes.DWORD), ("SchedulingClass", wintypes.DWORD)]


class JobIOCounters(ctypes.Structure):
    _fields_ = [(name, ctypes.c_ulonglong) for name in ("ReadOperationCount", "WriteOperationCount",
                "OtherOperationCount", "ReadTransferCount", "WriteTransferCount", "OtherTransferCount")]


class JobExtendedLimits(ctypes.Structure):
    _fields_ = [("BasicLimitInformation", JobBasicLimits), ("IoInfo", JobIOCounters),
                ("ProcessMemoryLimit", ctypes.c_size_t), ("JobMemoryLimit", ctypes.c_size_t),
                ("PeakProcessMemoryUsed", ctypes.c_size_t), ("PeakJobMemoryUsed", ctypes.c_size_t)]


class WindowsJob:
    """A non-inherited job handle; closing it kills any remaining server processes."""

    def __init__(self, kernel: Any, handle: Any) -> None:
        self.kernel = kernel
        self.handle = handle

    def close(self) -> None:
        if self.handle is not None:
            self.kernel.CloseHandle(self.handle)
            self.handle = None


def attach_windows_job(proc: subprocess.Popen, kernel: Any = None) -> WindowsJob | None:
    if not IS_WINDOWS and kernel is None:
        return None
    job = None
    try:
        if kernel is None:
            kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
        kernel.CreateJobObjectW.restype = wintypes.HANDLE
        kernel.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
        kernel.SetInformationJobObject.restype = wintypes.BOOL
        kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
        kernel.AssignProcessToJobObject.restype = wintypes.BOOL
        kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        kernel.CloseHandle.restype = wintypes.BOOL
        handle = kernel.CreateJobObjectW(None, None)
        if not handle:
            raise OSError("CreateJobObjectW failed")
        job = WindowsJob(kernel, handle)
        limits = JobExtendedLimits()
        limits.BasicLimitInformation.LimitFlags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not kernel.SetInformationJobObject(handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
            raise OSError("SetInformationJobObject failed")
        if not kernel.AssignProcessToJobObject(handle, wintypes.HANDLE(int(proc._handle))):
            raise OSError("AssignProcessToJobObject failed")
        return job
    except (OSError, AttributeError, TypeError, ValueError) as exc:
        if job is not None:
            job.close()
        print(f"[bench_whisper_server] warning: Windows Job Object unavailable: {exc}", file=sys.stderr)
        return None


def listening_pids(output: str, port: int) -> set[int]:
    result = set()
    for line in output.splitlines():
        columns = line.split()
        if len(columns) != 5 or columns[0].upper() != "TCP" or columns[3].upper() != "LISTENING":
            continue
        address, _, number = columns[1].rpartition(":")
        if number == str(port) and address in {"127.0.0.1", "0.0.0.0", "[::]", "[::1]"} and columns[4].isdigit():
            result.add(int(columns[4]))
    return result


def listener_owned(proc: subprocess.Popen, port: int, log: Any, timeout: float) -> tuple[bool, str]:
    fallback_reason = "PID lookup unavailable on this platform"
    if IS_WINDOWS:
        try:
            netstat = Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32" / "netstat.exe"
            result = subprocess.run([str(netstat), "-ano", "-p", "TCP"], capture_output=True,
                                    text=True, errors="replace", timeout=timeout, check=True,
                                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        except (OSError, subprocess.SubprocessError) as exc:
            fallback_reason = f"netstat failed: {exc}"
        else:
            owners = listening_pids(result.stdout, port)
            if owners and owners != {proc.pid}:
                return False, f"port {port} listener PID {sorted(owners)} != server PID {proc.pid}"
            if not owners:
                return False, f"port {port}: no LISTENING row found (netstat output may be unparsable)"
            return True, f"port {port} listener PID matches server PID {proc.pid}"
    output = Path(log.name).read_text(encoding="utf-8", errors="replace")
    ready = re.search(rf"(?:listening|ready)\b[^\r\n]*127\.0\.0\.1:{port}\b", output, re.IGNORECASE) is not None
    return ready, f"{fallback_reason}; port {port}: {'found' if ready else 'no'} listening/ready log marker"


def observed_device(log: Any) -> str | None:
    output = Path(log.name).read_text(encoding="utf-8", errors="replace")
    if re.search(r"(?:found GPU device[^\r\n]*CUDA\d+|using CUDA\d+ backend|CUDA\d+\s+total size)", output, re.IGNORECASE):
        return "cuda"
    if re.search(r"(?:using CPU backend|no GPU (?:device )?found|CPU\s+total size)", output, re.IGNORECASE):
        return "cpu"
    return None


def ensure_running(proc: subprocess.Popen, stage: str) -> None:
    code = proc.poll()
    if code is not None:
        raise BenchError(f"server exited {stage} (exit={code})")


def stop_server(proc: subprocess.Popen) -> None:
    if proc.poll() is None:
        if IS_WINDOWS:
            with contextlib.suppress(OSError, subprocess.TimeoutExpired):
                proc.send_signal(getattr(signal, "CTRL_BREAK_EVENT", 1))
                proc.wait(timeout=2)
        if proc.poll() is None:
            proc.terminate()
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        proc.kill()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            print(f"[bench_whisper_server] warning: server PID {proc.pid} did not exit after kill", file=sys.stderr)


@dataclass
class RunningServer:
    cmd: list[str]
    port: int
    proc: subprocess.Popen
    load_ms: float


@contextlib.contextmanager
def server_session(cmd: list[str], timeout: float, port: int, log: Any) -> Iterator[RunningServer]:
    proc = None
    job = None
    started = time.perf_counter()
    try:
        env = {**os.environ, "CUDA_DEVICE_ORDER": "PCI_BUS_ID", "PYTHONUNBUFFERED": "1"}
        flags = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0) if IS_WINDOWS else 0
        proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT, env=env, creationflags=flags)
        job = attach_windows_job(proc)
        deadline = started + timeout
        last_ownership = f"port {port}: ownership not checked (no connection)"
        while True:
            ensure_running(proc, "before ready")
            remaining = deadline - time.perf_counter()
            if remaining <= 0:
                raise BenchError(f"server readiness timed out after {timeout:g}s; {last_ownership}")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=min(0.1, remaining)):
                    load_ms = (time.perf_counter() - started) * 1000.0
            except OSError:
                pass
            else:
                owned, last_ownership = listener_owned(proc, port, log, max(0.01, deadline - time.perf_counter()))
                if owned:
                    ensure_running(proc, "after readiness")
                    break
            time.sleep(min(0.05, max(0, deadline - time.perf_counter())))
        yield RunningServer(cmd, port, proc, load_ms)
    finally:
        try:
            if proc is not None:
                stop_server(proc)
        finally:
            if job is not None:
                job.close()


def summarize(values: list[float]) -> dict[str, Any]:
    ordered = sorted(values)

    def percentile(pct: float) -> float:
        if not ordered or any(math.isnan(value) for value in ordered):
            return float("nan")
        rank = pct * (len(ordered) - 1)
        low, high = math.floor(rank), math.ceil(rank)
        return ordered[low] + (ordered[high] - ordered[low]) * (rank - low)

    return {"count": len(values), "min": min(values, default=float("nan")),
            "p50": percentile(0.5), "p95": percentile(0.95), "max": max(values, default=float("nan"))}


def run_benchmark(args: argparse.Namespace, manifest: dict[str, Any], conditions: dict[str, Any], log: Any,
                  server: RunningServer) -> dict[str, Any]:
    items = manifest["items"]
    languages = [primary_language(args.language or item["language"]) for item in items]
    warnings = []
    device = observed_device(log)
    if device is None:
        warnings.append("server log did not identify a CUDA or CPU backend")
    elif device != args.device_label.lower():
        warnings.append(f"observed device {device!r} disagrees with --device-label {args.device_label!r}")
    for warning in warnings:
        print(f"[bench_whisper_server] warning: {warning}", file=sys.stderr)
    samples = []
    for _ in range(args.warmup):
        transcribe(server.port, items[0], languages[0], args)
        ensure_running(server.proc, f"after warmup item {items[0]['id']!r}")
    ensure_running(server.proc, "after warmup")
    for item, language in zip(items, languages):
        for _ in range(args.repeat):
            samples.append(transcribe(server.port, item, language, args))
            ensure_running(server.proc, f"after request for item {item['id']!r}")
    config = {
        "backend": "openwhispr-whisper-server", "engine_version": args.engine_version,
        "model": args.model.name, "quantization": args.quantization, "device": args.device_label,
        "decode": f"beam{args.beam_size}" if args.beam_size is not None else "greedy-bestof2",
        "threads": effective_threads(args), "worker_cmd": server.cmd,
        "whisper_decode_profile": None,
        "observed_devices": [{"device": device, "compute_type": args.quantization}] if device else [],
        "request_fields": [request_fields(language, args.beam_size) for language in dict.fromkeys(languages)],
        "manifest": conditions, "language_source": "argument" if args.language else "manifest",
        "language": args.language, "repeat": args.repeat, "warmup": args.warmup,
        "timeout_s": args.timeout, "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }
    if args.model_sha256 is not None:
        config["model_sha256"] = args.model_sha256
    return {"config": config, "warnings": warnings,
            "warm": {"wall_ms": summarize([s["wall_ms"] for s in samples]),
                     "duration_ms": summarize([s["duration_ms"] for s in samples]), "samples": samples},
            "cold": {"load_ms": summarize([server.load_ms]), "wall_ms": summarize([]),
                     "duration_ms": summarize([]), "samples": []}}


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--model", type=Path, required=True)
    parser.add_argument("--server-exe", "--exe", default="whisper-server-win32-x64-cuda.exe")
    parser.add_argument("--engine-version", default="openwhispr-whisper.cpp-0.0.11")
    parser.add_argument("--model-sha256")
    parser.add_argument("--quantization", default="f16")
    parser.add_argument("--device-label", default="cuda", help="metadata label; does not change server GPU settings")
    parser.add_argument("--language", help="override manifest locales (mapped to primary language subtag)")
    parser.add_argument("--threads", type=int, help="explicit server threads, including values <= 4")
    parser.add_argument("--beam-size", type=int)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--repeat", type=int, default=5)
    parser.add_argument("--timeout", type=float, default=300.0)
    parser.add_argument("--output", type=Path, help="new JSON file; existing files are never overwritten")
    parser.add_argument("--json", action="store_true", help="accepted for bench_asr parity; output is always JSON")
    parser.add_argument("--server-cmd", nargs=argparse.REMAINDER,
                        help="replace executable prefix; app launch flags are appended; must be last")
    return parser.parse_args(argv)


def validate(args: argparse.Namespace) -> tuple[dict[str, Any], dict[str, Any]]:
    if args.repeat < 1 or args.warmup < 0 or (args.threads is not None and args.threads < 1):
        raise BenchError("repeat and threads must be positive; warmup must be nonnegative")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        raise BenchError("timeout must be finite and positive")
    if args.beam_size is not None and args.beam_size < 2:
        raise BenchError("beam-size must be >= 2 (1 selects greedy in whisper-server)")
    if args.server_cmd == []:
        raise BenchError("server-cmd requires a command")
    if args.language is not None and not primary_language(args.language).isalpha():
        raise BenchError("language must have an alphabetic primary subtag")
    if args.model_sha256 is not None and (len(args.model_sha256) != 64 or any(c not in "0123456789abcdefABCDEF" for c in args.model_sha256)):
        raise BenchError("model-sha256 must contain 64 hexadecimal digits")
    args.model = args.model.resolve()
    args.server_exe = str(Path(args.server_exe).resolve())
    if not args.model.is_file():
        raise BenchError(f"model file not found: {args.model}; bare names are not searched on PATH")
    if args.server_cmd is None and not Path(args.server_exe).is_file():
        raise BenchError(f"--server-exe file not found: {args.server_exe}; bare names are not searched on PATH")
    manifest, conditions = load_manifest(args.manifest)
    for item in manifest["items"]:
        try:
            if not primary_language(item["language"]).isalpha():
                raise BenchError("invalid manifest language")
            with wave.open(item["audio_path"], "rb") as audio:
                if (audio.getframerate(), audio.getnchannels(), audio.getsampwidth(), audio.getcomptype()) != (16000, 1, 2, "NONE"):
                    raise BenchError("expected 16 kHz mono PCM16 WAV")
                if audio.getnframes() < 1 or len(audio.readframes(audio.getnframes())) != audio.getnframes() * 2:
                    raise BenchError("empty or truncated WAV")
        except (OSError, EOFError, wave.Error, BenchError) as exc:
            raise BenchError(f"item {item['id']!r}: {exc}") from exc
    return manifest, conditions


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    stream = None
    created = None
    try:
        manifest, conditions = validate(args)
        if args.output is not None:
            stream = args.output.open("x", encoding="utf-8", newline="\n")
            created = os.fstat(stream.fileno())
        with tempfile.NamedTemporaryFile(mode="wb", prefix=(args.output.name + ".") if args.output else "bench_whisper_server_",
                                         suffix=".server.log", dir=args.output.parent if args.output else None, delete=False) as log:
            print(f"[bench_whisper_server] server log: {log.name}", file=sys.stderr)
            port = free_port()
            language = primary_language(args.language or manifest["items"][0]["language"])
            cmd = build_server_cmd(args, port, language)
            with server_session(cmd, args.timeout, port, log) as server:
                result = run_benchmark(args, manifest, conditions, log, server)
                rendered = json.dumps(result, ensure_ascii=False, indent=2) + "\n"
                ensure_running(server.proc, "before writing results")
                if stream is None:
                    print(rendered, end="")
                else:
                    try:
                        stream.write(rendered)
                        stream.flush()
                    except OSError:
                        print(rendered, end="")
                        raise
        return 0
    except (BenchError, OSError, ValueError) as exc:
        print(f"[bench_whisper_server] error: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("[bench_whisper_server] interrupted", file=sys.stderr)
        return 130
    finally:
        if stream is not None:
            with contextlib.suppress(OSError):
                stream.close()
            # Only remove our own empty reservation; retain partial writes for
            # diagnostics, and never remove an existing file rejected by 'x'.
            with contextlib.suppress(OSError):
                current = args.output.lstat()
                if created is not None and current.st_size == 0 and os.path.samestat(created, current):
                    args.output.unlink()


if __name__ == "__main__":
    raise SystemExit(main())
