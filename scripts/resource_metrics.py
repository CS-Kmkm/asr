"""Opt-in sampled process-tree resources. Missing measurements stay null.

RSS is a sum (shared pages can be counted more than once). CPU percent uses
one logical core = 100%. GPU memory is per-process nvidia-smi memory, not
device-wide usage; WDDM/unsupported queries are missing, never zero.
"""
from __future__ import annotations

import importlib
import math
import shutil
import subprocess
import threading
import time


class ResourceMonitor:
    def __init__(self, pid: int | None, enabled: bool = True, interval_s: float = 0.25, process_module=None) -> None:
        if not math.isfinite(interval_s) or interval_s < 0.05:
            raise ValueError("resource interval must be at least 0.05 seconds")
        self.pid, self.enabled, self.interval = pid, enabled, interval_s
        self.module = process_module
        self.stop_signal = threading.Event()
        self.thread = None
        self.cpu_previous = {}
        self.last_at = None
        self.cpu_seconds = 0.0
        self.cpu_peak = self.rss_peak = self.gpu_peak = None
        self.samples = self.cpu_samples = self.gpu_samples = 0
        self.status = "disabled" if not enabled else "unavailable"
        self.gpu_tool = shutil.which("nvidia-smi") if enabled else None

    def __enter__(self):
        if not self.enabled or self.pid is None:
            return self
        try:
            self.module = self.module or importlib.import_module("psutil")
            self.root = self.module.Process(self.pid)
            self.root_identity = self.root.create_time()
            self.sample()
        except (ImportError, OSError, self._process_error()):
            return self
        self.status = "sampled"
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()
        return self

    def _process_error(self):
        return getattr(self.module, "Error", OSError)

    def _run(self):
        while not self.stop_signal.wait(self.interval):
            try:
                self.sample()
            except (OSError, self._process_error()):
                pass

    def sample(self):
        # A recycled PID is never treated as the original inference process.
        if self.root.create_time() != self.root_identity:
            return
        processes = [self.root, *self.root.children(recursive=True)]
        now = time.perf_counter()
        rss, cpu_delta, observed = 0, 0.0, set()
        for process in processes:
            try:
                key = (process.pid, process.create_time())
                times = process.cpu_times()
                cpu = float(times.user + times.system)
                # The process may predate its first successful observation.
                # Never attribute its entire lifetime CPU to this interval.
                previous = self.cpu_previous.get(key, cpu)
                cpu_delta += max(0.0, cpu - previous)
                self.cpu_previous[key] = cpu
                rss += process.memory_info().rss
                observed.add(process.pid)
            except (OSError, self._process_error()):
                continue
        if observed:
            self.samples += 1
            self.rss_peak = max(self.rss_peak or 0, rss)
            if self.last_at is not None and now > self.last_at:
                self.cpu_samples += 1
                self.cpu_seconds += cpu_delta
                value = cpu_delta / (now - self.last_at) * 100
                self.cpu_peak = max(self.cpu_peak or 0.0, value)
            self.last_at = now
            gpu = self._gpu_memory(observed)
            if gpu is not None:
                self.gpu_samples += 1
                self.gpu_peak = max(self.gpu_peak or 0, gpu)

    def _gpu_memory(self, pids):
        if not self.gpu_tool:
            return None
        try:
            result = subprocess.run([self.gpu_tool, "--query-compute-apps=pid,used_gpu_memory", "--format=csv,noheader,nounits"],
                                    capture_output=True, text=True, timeout=1,
                                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0), check=True)
            values = []
            for line in result.stdout.splitlines():
                parts = [part.strip() for part in line.split(",")]
                if len(parts) == 2 and parts[0].isdigit() and int(parts[0]) in pids:
                    if not parts[1].isdigit():
                        return None
                    values.append(int(parts[1]) * 1024 * 1024)
            return sum(values) if values else None
        except (OSError, subprocess.SubprocessError):
            return None

    def __exit__(self, *exc):
        self.stop_signal.set()
        if self.thread:
            self.thread.join(timeout=2)
            try:
                self.sample()
            except (OSError, self._process_error()):
                pass

    def report(self):
        return {"status": self.status, "scope": "root_process_and_observed_descendants",
                "interval_ms": self.interval * 1000, "samples": self.samples,
                "cpu_samples": self.cpu_samples, "cpu_observed_seconds": self.cpu_seconds if self.cpu_samples else None,
                "cpu_peak_percent_one_core_100": self.cpu_peak,
                "rss_sum_sampled_peak_bytes": self.rss_peak, "gpu_memory_sampled_peak_bytes": self.gpu_peak,
                "gpu_samples": self.gpu_samples,
                "limitations": "Sampled peaks may miss short spikes/short-lived children; first CPU observations are baselines, not lifetime CPU; RSS sums shared pages; external servers are excluded; missing GPU values are not zero."}
