#!/usr/bin/env python3
"""Measure Windows dictation apps from stop hotkey to paste in a native EDIT.

The app must already be running, with its microphone set to CABLE Output.
Only the standard library is used. Importing this module has no UI effects.
"""

from __future__ import annotations

import argparse
import contextlib
import ctypes
from ctypes import wintypes as wt
from dataclasses import dataclass
import importlib.util
import json
import math
import os
from pathlib import Path
import sys
import threading
import time
from typing import Any
import uuid
import wave

HARNESS_VERSION = "2"
STATUSES = ("ok", "empty", "timeout", "focus_lost", "error")
NAN = float("nan")


class BenchError(RuntimeError):
    """An actionable harness failure."""


class AbortRun(BenchError):
    """Continuing trials would be unsafe or invalidate the measurements."""


class FocusLost(BenchError):
    def __init__(self, process: str) -> None:
        self.process = process
        super().__init__(f"focus left the measurement window ({process})")


def load_manifest(path: Path) -> tuple[dict[str, Any], dict[str, Any]]:
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


def summarize(values: list[float]) -> dict[str, Any]:
    values = sorted(value for value in values if math.isfinite(value))

    def percentile(fraction: float) -> float:
        if not values:
            return NAN
        rank = fraction * (len(values) - 1)
        low, high = math.floor(rank), math.ceil(rank)
        return values[low] + (values[high] - values[low]) * (rank - low)

    return {"count": len(values), "min": min(values, default=NAN), "max": max(values, default=NAN),
            "p50": percentile(0.5), "p95": percentile(0.95)}


def parse_hotkey(value: str) -> tuple[int, ...]:
    modifiers = {"ctrl": 0x11, "shift": 0x10, "alt": 0x12, "win": 0x5B}
    keys = []
    main_key = False
    for token in value.split("+"):
        token = token.strip().lower()
        if token in modifiers:
            key = modifiers[token]
        else:
            if main_key:
                raise BenchError("hotkey accepts at most one main key")
            main_key = True
            if token == "space":
                key = 0x20
            elif len(token) == 1 and token in "abcdefghijklmnopqrstuvwxyz0123456789":
                key = ord(token.upper())
            elif token.startswith("f") and token[1:].isdigit() and 1 <= int(token[1:]) <= 24:
                key = 0x70 + int(token[1:]) - 1
            else:
                raise BenchError(f"unsupported hotkey token: {token!r}")
        if key in keys:
            raise BenchError("hotkey contains a duplicate key")
        keys.append(key)
    if not any(key in keys for key in (0x11, 0x12, 0x5B)):
        raise BenchError("hotkey requires a non-Shift modifier (Ctrl, Alt or Win)")
    if not main_key and 0x5B not in keys:
        raise BenchError("modifier-only hotkeys must include Win")
    # Press modifiers before the main key even if the user lists it first.
    return tuple([key for key in keys if key in modifiers.values()] +
                 [key for key in keys if key not in modifiers.values()])


class Keyboard:
    """Guard input batches and verify release, including during console shutdown."""

    def __init__(self, send: Any, window: Any, chord: tuple[int, ...], clock: Any = time,
                 *, key_down: Any = None, idle_seconds: Any = None) -> None:
        self.send = send
        self.window = window
        self.chord = chord
        self.clock = clock
        self.pressed: list[int] = []
        self.key_down = key_down if key_down is not None else lambda key: False
        self.idle_seconds = idle_seconds if idle_seconds is not None else lambda: math.inf
        self.lock = threading.RLock()
        self.shutdown = threading.Event()
        self.release_failure: AbortRun | None = None

    def before_trial(self) -> None:
        deadline = self.clock.perf_counter() + 10
        while True:
            if self.shutdown.is_set():
                raise AbortRun("console shutdown requested")
            modifiers = (0x10, 0x11, 0x12, 0x5B, 0x5C, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5)
            if any(self.key_down(key) for key in modifiers):
                raise AbortRun("a physical modifier is down; refusing to inject keys")
            if self.idle_seconds() >= 1:
                return
            remaining = deadline - self.clock.perf_counter()
            if remaining <= 0:
                raise AbortRun("input did not remain idle for 1 second within 10 seconds")
            self.clock.sleep(min(0.05, remaining))

    def check_focus(self) -> None:
        process = self.window.focus_process()
        if process is not None:
            raise FocusLost(process)

    def down(self) -> float:
        with self.lock:
            if self.shutdown.is_set():
                raise AbortRun("console shutdown requested")
            self.check_focus()
            self.pressed = list(self.chord)  # Include partial SendInput failures in cleanup.
            started = self.clock.perf_counter()
            sent_at = self.send([(key, False) for key in self.chord])
            return started if sent_at is None else sent_at

    def release(self, *, emergency: bool = False) -> float:
        with self.lock:
            if self.release_failure is not None:
                raise self.release_failure
            if not self.pressed:
                return self.clock.perf_counter()
            if not emergency:
                self.check_focus()
            events = []
            for key in reversed(self.pressed):
                if key == 0x5B or (key == 0x12 and all(k in (0x10, 0x11, 0x12, 0x5B) for k in self.chord)):
                    # Mask standalone Win / modifier-only Alt menu activation.
                    events.extend([(0xE8, False), (0xE8, True)])
                events.append((key, True))
            watched = list(dict.fromkeys(key for key, _ in events))
            started = self.clock.perf_counter()
            sent = False
            try:
                sent_at = self.send(events)
                sent = True
            finally:
                try:
                    stuck = [key for key in watched if self.key_down(key)]
                    if stuck:
                        # Only key-up events: safe even if focus moved during release.
                        self.send([(key, True) for key in stuck])
                        self.clock.sleep(0.01)  # Let the input queue apply the retry before checking it.
                        if any(self.key_down(key) for key in watched):
                            raise AbortRun("INJECTED KEYS ARE STILL DOWN after the key-up retry; aborting")
                except Exception as exc:
                    self.release_failure = AbortRun(f"could not verify injected key release: {exc}")
                    raise self.release_failure from exc
                if sent:
                    self.pressed.clear()
            return started if sent_at is None else sent_at

    def cleanup(self, mode: str) -> None:
        with self.lock:
            lost = bool(self.pressed) and mode == "hold" and self.window.focus_process() is not None
            regained = not lost
            if lost:
                try:
                    regained = self.window.refocus(timeout_s=0.5)
                except BenchError:
                    regained = False
            try:
                self.release(emergency=True)
            except Exception as exc:
                raise AbortRun(f"could not release injected keys: {exc}") from exc
            if not regained:
                raise AbortRun("could not regain focus within 500 ms; release stopped recording and the app may paste into another window")

    def tap(self) -> float:
        started = self.down()
        self.clock.sleep(0.04)
        self.release()
        return started


@dataclass(frozen=True)
class Change:
    at: float
    text: str
    focus_process: str | None = None
    detected_by: str = "en_change"


def wait_for_paste(window: Any, stop: float, args: argparse.Namespace, clock: Any = time,
                   pre_stop_text: str = "", pre_stop_changes: bool = False) -> dict[str, Any]:
    deadline = stop + args.timeout_s
    while True:
        now, events = window.snapshot(clock)
        before = [event for event in events if event.at < stop]
        if before:
            pre_stop_text = before[-1].text
            pre_stop_changes = True
        changes = [event for event in events if stop <= event.at <= deadline]
        first = (changes[0].at - stop) * 1000 if changes else NAN
        last = (changes[-1].at - stop) * 1000 if changes else NAN
        text = changes[-1].text if changes else ""
        if changes and text.startswith(pre_stop_text):
            text = text[len(pre_stop_text):]
        elif changes:
            pre_stop_changes = True
        result = {"text": text.strip(), "change_events": len(changes), "pre_stop_changes": pre_stop_changes,
                  "detected_by": changes[0].detected_by if changes else None,
                  "stop_to_first_paste_ms": first, "stop_to_settled_ms": last}
        lost = next((event.focus_process for event in changes if event.focus_process is not None), None)
        if lost is not None:
            return {**result, "status": "focus_lost", "focus_process": lost}
        if changes and changes[-1].at + args.settle_ms / 1000 <= min(now, deadline):
            return {**result, "status": "ok" if result["text"] else "empty"}
        if now >= deadline:
            if changes:
                return {**result, "status": "error", "error": "text did not settle before timeout"}
            return {**result, "status": "timeout"}
        clock.sleep(min(0.01, deadline - now))


def run_trial(item: dict[str, Any], args: argparse.Namespace, window: Any, audio: Any,
              keys: Keyboard, clipboard: Any, clock: Any = time) -> dict[str, Any]:
    sample = {"audio": item["audio_path"], "language": item["language"], "text": "", "wall_ms": NAN,
              "duration_ms": NAN, "status": "error", "stop_to_first_paste_ms": NAN,
              "stop_to_settled_ms": NAN, "audio_duration_ms": NAN, "play_start_lag_ms": NAN,
              "change_events": 0, "clipboard_restored": None, "pre_stop_changes": False, "detected_by": None}
    sentinel = f"asr-e2e-{uuid.uuid4()}"
    sentinel_written = False
    toggle_active = False
    try:
        window.prepare()
        keys.before_trial()
        clipboard.write(sentinel, exclude_from_history=True)
        sentinel_written = True
        if args.mode == "hold":
            keys.down()
        else:
            toggle_active = True
            keys.tap()
        clock.sleep(args.pre_roll_ms / 1000)
        requested = clock.perf_counter()
        started, completed, duration_ms = audio.play(Path(item["audio_path"]))
        sample.update(audio_duration_ms=duration_ms, play_start_lag_ms=(started - requested) * 1000,
                      playback_ms=(completed - started) * 1000)
        clock.sleep(args.tail_ms / 1000)
        pre_stop_text, pre_stop_changes = window.text_at_stop()
        sample["pre_stop_changes"] = pre_stop_changes
        stop = keys.release() if args.mode == "hold" else keys.tap()
        toggle_active = False
        keys.check_focus()
        sample.update(wait_for_paste(window, stop, args, clock, pre_stop_text, pre_stop_changes))
    except BaseException as exc:
        if toggle_active:
            raise AbortRun(f"toggle recording may still be active; aborting desynchronized run: {exc}") from exc
        if isinstance(exc, AbortRun):
            raise
        if isinstance(exc, FocusLost):
            sample.update(status="focus_lost", focus_process=exc.process)
        elif isinstance(exc, Exception):
            sample.update(status="error", error=str(exc))
        else:
            raise
    finally:
        keys.cleanup(args.mode)
    if sentinel_written and sample["change_events"] > 0:
        try:
            sample["clipboard_restored"] = clipboard.read() == sentinel
        except Exception as exc:
            sample["clipboard_error"] = str(exc)
    if sample["status"] == "ok":
        sample["wall_ms"] = sample["stop_to_first_paste_ms"]
    return sample


def make_result(args: argparse.Namespace, conditions: dict[str, Any], samples: list[dict[str, Any]],
                device_name: str, windows_build: str) -> dict[str, Any]:
    ok = [sample for sample in samples if sample["status"] == "ok"]
    clipboard_trials = [sample for sample in samples if sample["change_events"] > 0]
    config = {key: getattr(args, key) for key in ("app_label", "hotkey", "mode", "pre_roll_ms", "tail_ms",
                                               "settle_ms", "timeout_s", "gap_s", "warmup", "limit")}
    config.update(manifest=conditions, language_source="manifest", conditions=args.conditions,
                  playback_device=device_name, harness_version=HARNESS_VERSION,
                  win_mask_used=0x5B in parse_hotkey(args.hotkey),
                  timestamp=time.strftime("%Y-%m-%dT%H:%M:%S%z"), windows_build=windows_build)
    return {"config": config,
            "warm": {"wall_ms": summarize([s["wall_ms"] for s in ok]), "duration_ms": summarize([]), "samples": samples},
            "cold": {"load_ms": summarize([]), "wall_ms": summarize([]), "duration_ms": summarize([]), "samples": []},
            "e2e": {"counts": {status: sum(s["status"] == status for s in samples) for status in STATUSES},
                    "success_rate": len(ok) / len(samples) if samples else NAN,
                    "stop_to_first_paste_ms": summarize([s["stop_to_first_paste_ms"] for s in ok]),
                    "stop_to_settled_ms": summarize([s["stop_to_settled_ms"] for s in ok]),
                    "clipboard_restored_rate": sum(s["clipboard_restored"] is True for s in clipboard_trials) / len(clipboard_trials) if clipboard_trials else NAN}}


# ctypes declarations live here rather than in a third-party Win32 dependency.
ULONG_PTR = ctypes.c_size_t
LRESULT = ctypes.c_ssize_t


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wt.WORD), ("wScan", wt.WORD), ("dwFlags", wt.DWORD),
                ("time", wt.DWORD), ("dwExtraInfo", ULONG_PTR)]


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", wt.LONG), ("dy", wt.LONG), ("mouseData", wt.DWORD), ("dwFlags", wt.DWORD),
                ("time", wt.DWORD), ("dwExtraInfo", ULONG_PTR)]


class INPUTUNION(ctypes.Union):
    _fields_ = [("ki", KEYBDINPUT), ("mi", MOUSEINPUT)]


class INPUT(ctypes.Structure):
    _anonymous_ = ("u",)
    _fields_ = [("type", wt.DWORD), ("u", INPUTUNION)]


class WAVEFORMATEX(ctypes.Structure):
    _pack_ = 1
    _fields_ = [("wFormatTag", wt.WORD), ("nChannels", wt.WORD), ("nSamplesPerSec", wt.DWORD),
                ("nAvgBytesPerSec", wt.DWORD), ("nBlockAlign", wt.WORD), ("wBitsPerSample", wt.WORD),
                ("cbSize", wt.WORD)]


class WAVEHDR(ctypes.Structure):
    _fields_ = [("lpData", ctypes.c_void_p), ("dwBufferLength", wt.DWORD), ("dwBytesRecorded", wt.DWORD),
                ("dwUser", ULONG_PTR), ("dwFlags", wt.DWORD), ("dwLoops", wt.DWORD),
                ("lpNext", ctypes.c_void_p), ("reserved", ULONG_PTR)]


class WAVEOUTCAPSW(ctypes.Structure):
    _fields_ = [("wMid", wt.WORD), ("wPid", wt.WORD), ("vDriverVersion", wt.DWORD),
                ("szPname", wt.WCHAR * 32), ("dwFormats", wt.DWORD), ("wChannels", wt.WORD),
                ("wReserved1", wt.WORD), ("dwSupport", wt.DWORD)]


class LASTINPUTINFO(ctypes.Structure):
    _fields_ = [("cbSize", wt.UINT), ("dwTime", wt.DWORD)]


class WinAPI:
    def __init__(self) -> None:
        if sys.platform != "win32":
            raise BenchError("product E2E requires Windows 11")
        if sys.getwindowsversion().build < 22000:
            raise BenchError("product E2E requires Windows 11 (build 22000 or newer)")
        self.user = ctypes.WinDLL("user32", use_last_error=True)
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.winmm = ctypes.WinDLL("winmm", use_last_error=True)
        self.wndproc_type = ctypes.WINFUNCTYPE(LRESULT, wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM)
        self.waveproc_type = ctypes.WINFUNCTYPE(None, wt.HANDLE, wt.UINT, ULONG_PTR, ULONG_PTR, ULONG_PTR)
        self.ctrlhandler_type = ctypes.WINFUNCTYPE(wt.BOOL, wt.DWORD)

        def bind(dll: Any, name: str, restype: Any, *argtypes: Any) -> Any:
            function = getattr(dll, name)
            function.restype, function.argtypes = restype, argtypes
            return function

        self.bind = bind
        bind(self.user, "SendInput", wt.UINT, wt.UINT, ctypes.POINTER(INPUT), ctypes.c_int)
        bind(self.user, "GetAsyncKeyState", wt.SHORT, ctypes.c_int)
        bind(self.user, "GetLastInputInfo", wt.BOOL, ctypes.POINTER(LASTINPUTINFO))
        bind(self.kernel, "GetTickCount", wt.DWORD)
        bind(self.kernel, "SetConsoleCtrlHandler", wt.BOOL, self.ctrlhandler_type, wt.BOOL)
        bind(self.user, "GetForegroundWindow", wt.HWND)
        bind(self.user, "GetWindowThreadProcessId", wt.DWORD, wt.HWND, ctypes.POINTER(wt.DWORD))
        bind(self.user, "SetForegroundWindow", wt.BOOL, wt.HWND)
        bind(self.user, "AttachThreadInput", wt.BOOL, wt.DWORD, wt.DWORD, wt.BOOL)
        bind(self.user, "SetFocus", wt.HWND, wt.HWND)
        bind(self.user, "ShowWindow", wt.BOOL, wt.HWND, ctypes.c_int)
        bind(self.user, "SendMessageW", LRESULT, wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM)
        bind(self.user, "SendMessageTimeoutW", LRESULT, wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM,
             wt.UINT, wt.UINT, ctypes.POINTER(ULONG_PTR))
        bind(self.user, "PostMessageW", wt.BOOL, wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM)
        bind(self.user, "DefWindowProcW", LRESULT, wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM)
        bind(self.user, "DestroyWindow", wt.BOOL, wt.HWND)
        bind(self.user, "PostQuitMessage", None, ctypes.c_int)
        bind(self.user, "GetMessageW", wt.BOOL, ctypes.POINTER(wt.MSG), wt.HWND, wt.UINT, wt.UINT)
        bind(self.user, "TranslateMessage", wt.BOOL, ctypes.POINTER(wt.MSG))
        bind(self.user, "DispatchMessageW", LRESULT, ctypes.POINTER(wt.MSG))
        bind(self.user, "CreateWindowExW", wt.HWND, wt.DWORD, wt.LPCWSTR, wt.LPCWSTR, wt.DWORD,
             ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int, wt.HWND, wt.HMENU, wt.HINSTANCE, ctypes.c_void_p)
        bind(self.user, "UnregisterClassW", wt.BOOL, wt.LPCWSTR, wt.HINSTANCE)
        bind(self.kernel, "GetModuleHandleW", wt.HMODULE, wt.LPCWSTR)
        bind(self.kernel, "GetCurrentThreadId", wt.DWORD)
        bind(self.kernel, "OpenProcess", wt.HANDLE, wt.DWORD, wt.BOOL, wt.DWORD)
        bind(self.kernel, "CloseHandle", wt.BOOL, wt.HANDLE)
        bind(self.kernel, "QueryFullProcessImageNameW", wt.BOOL, wt.HANDLE, wt.DWORD, wt.LPWSTR, ctypes.POINTER(wt.DWORD))
        for name in ("OpenClipboard", "EmptyClipboard", "CloseClipboard", "IsClipboardFormatAvailable"):
            types = {"OpenClipboard": (wt.HWND,), "IsClipboardFormatAvailable": (wt.UINT,)}.get(name, ())
            bind(self.user, name, wt.BOOL, *types)
        bind(self.user, "GetClipboardData", wt.HANDLE, wt.UINT)
        bind(self.user, "SetClipboardData", wt.HANDLE, wt.UINT, wt.HANDLE)
        bind(self.user, "RegisterClipboardFormatW", wt.UINT, wt.LPCWSTR)
        bind(self.kernel, "GlobalAlloc", wt.HANDLE, wt.UINT, ctypes.c_size_t)
        bind(self.kernel, "GlobalLock", ctypes.c_void_p, wt.HANDLE)
        bind(self.kernel, "GlobalUnlock", wt.BOOL, wt.HANDLE)
        bind(self.kernel, "GlobalFree", wt.HANDLE, wt.HANDLE)
        bind(self.winmm, "waveOutGetNumDevs", wt.UINT)
        bind(self.winmm, "waveOutGetDevCapsW", wt.UINT, ULONG_PTR, ctypes.POINTER(WAVEOUTCAPSW), wt.UINT)
        bind(self.winmm, "waveOutOpen", wt.UINT, ctypes.POINTER(wt.HANDLE), wt.UINT,
             ctypes.POINTER(WAVEFORMATEX), self.waveproc_type, ULONG_PTR, wt.DWORD)
        for name in ("waveOutPrepareHeader", "waveOutWrite", "waveOutUnprepareHeader"):
            bind(self.winmm, name, wt.UINT, wt.HANDLE, ctypes.POINTER(WAVEHDR), wt.UINT)
        bind(self.winmm, "waveOutReset", wt.UINT, wt.HANDLE)
        bind(self.winmm, "waveOutClose", wt.UINT, wt.HANDLE)

    def send(self, events: list[tuple[int, bool]]) -> float:
        inputs = (INPUT * len(events))()
        for target, (key, up) in zip(inputs, events):
            target.type = 1
            target.ki = KEYBDINPUT(key, 0, (2 if up else 0) | (1 if key == 0x5B else 0), 0, 0)
        started = time.perf_counter()
        if self.user.SendInput(len(inputs), inputs, ctypes.sizeof(INPUT)) != len(inputs):
            raise BenchError("SendInput failed or was partial; check app/harness integrity levels")
        return started

    def key_down(self, key: int) -> bool:
        return bool(self.user.GetAsyncKeyState(key) & 0x8000)

    def idle_seconds(self) -> float:
        info = LASTINPUTINFO(ctypes.sizeof(LASTINPUTINFO), 0)
        if not self.user.GetLastInputInfo(ctypes.byref(info)):
            raise AbortRun("GetLastInputInfo failed; refusing to inject keys")
        return ((self.kernel.GetTickCount() - info.dwTime) & 0xFFFFFFFF) / 1000


class ConsoleKeyGuard:
    def __init__(self, api: WinAPI, keys: Keyboard, mode: str) -> None:
        self.api, self.keys, self.mode = api, keys, mode
        self.callback = api.ctrlhandler_type(self._handle)

    def _handle(self, event: int) -> bool:
        if event in (2, 5, 6):  # CTRL_CLOSE / LOGOFF / SHUTDOWN
            self.keys.shutdown.set()
            try:
                self.keys.cleanup(self.mode)
            except BaseException as exc:
                print(f"[bench_e2e] CONSOLE SHUTDOWN key cleanup failed: {exc}", file=sys.stderr)
        # Preserve the default OS handler; Ctrl+C continues to raise KeyboardInterrupt.
        return False

    def __enter__(self) -> ConsoleKeyGuard:
        if not self.api.kernel.SetConsoleCtrlHandler(self.callback, True):
            raise AbortRun("could not install console key-release handler")
        return self

    def __exit__(self, *exc: object) -> None:
        self.api.kernel.SetConsoleCtrlHandler(self.callback, False)


class TargetWindow:
    def __init__(self, api: WinAPI) -> None:
        self.api = api
        self.hwnd = None
        self.edit = None
        self.events: list[Change] = []
        self.lock = threading.Lock()
        self.last_text = ""
        self.ready = threading.Event()
        self.failure: BaseException | None = None
        self.thread = threading.Thread(target=self._loop, name="e2e-edit", daemon=True)

    def __enter__(self) -> TargetWindow:
        self.thread.start()
        if not self.ready.wait(10):
            raise BenchError("measurement window did not start")
        if self.failure is not None:
            raise BenchError(f"measurement window failed: {self.failure}")
        return self

    def __exit__(self, *exc: object) -> None:
        if self.hwnd:
            self.api.user.PostMessageW(self.hwnd, 0x10, 0, 0)
        self.thread.join(timeout=5)

    def focus_process(self) -> str | None:
        foreground = self.api.user.GetForegroundWindow()
        if self.hwnd and foreground == self.hwnd:
            return None
        if not foreground:
            return "unknown"
        pid = wt.DWORD()
        self.api.user.GetWindowThreadProcessId(foreground, ctypes.byref(pid))
        handle = self.api.kernel.OpenProcess(0x1000, False, pid.value)
        if not handle:
            return "unknown"
        try:
            buffer = ctypes.create_unicode_buffer(32768)
            length = wt.DWORD(len(buffer))
            if self.api.kernel.QueryFullProcessImageNameW(handle, 0, buffer, ctypes.byref(length)):
                return Path(buffer.value).name
            return "unknown"
        finally:
            self.api.kernel.CloseHandle(handle)

    def ensure_alive(self) -> None:
        if self.failure is not None or not self.thread.is_alive():
            raise AbortRun(f"measurement window closed or failed: {self.failure}")

    def message(self, message: int, *, timeout_ms: int = 250) -> int:
        self.ensure_alive()
        if not self.hwnd:
            raise FocusLost("unknown")
        result = ULONG_PTR()
        if not self.api.user.SendMessageTimeoutW(self.hwnd, message, 0, 0, 0x23, timeout_ms, ctypes.byref(result)):
            raise AbortRun("measurement window closed or stopped responding (SendMessageTimeoutW)")
        self.ensure_alive()
        return result.value

    def snapshot(self, clock: Any = time) -> tuple[float, list[Change]]:
        self.message(0x8001)  # Poll on the EDIT's thread, before taking the lock.
        with self.lock:
            return clock.perf_counter(), list(self.events)

    def text_at_stop(self) -> tuple[str, bool]:
        self.message(0x8001)
        with self.lock:
            return self.last_text, bool(self.events) or bool(self.last_text)

    def refocus(self, *, timeout_s: float = 0.5) -> bool:
        deadline = time.perf_counter() + min(timeout_s, 0.5)
        while time.perf_counter() < deadline:
            if self.focus_process() is None:
                return True
            remaining_ms = max(1, int((deadline - time.perf_counter()) * 1000))
            try:
                self.message(0x8002, timeout_ms=min(100, remaining_ms))
            except BenchError:
                self.ensure_alive()
                return False
            if self.focus_process() is None:
                return True
            time.sleep(min(0.01, max(0, deadline - time.perf_counter())))
        return False

    def prepare(self) -> None:
        self.ensure_alive()
        if not self.refocus(timeout_s=0.5):
            raise FocusLost(self.focus_process() or "unknown")
        self.message(0x8000)

    def _bring_foreground(self) -> None:
        # Runs on the window thread. The caller uses SendMessageTimeoutW so a
        # foreground app with a hung input queue cannot block emergency cleanup.
        user = self.api.user
        user.ShowWindow(self.hwnd, 9)  # SW_RESTORE
        user.SetForegroundWindow(self.hwnd)
        if self.focus_process() is not None:
            # Attach the caller to the current foreground input queue, as
            # documented for SetForegroundWindow foreground-lock restrictions.
            foreground = user.GetForegroundWindow()
            foreground_thread = user.GetWindowThreadProcessId(foreground, None)
            current = self.api.kernel.GetCurrentThreadId()
            attached = foreground_thread != current and user.AttachThreadInput(current, foreground_thread, True)
            try:
                user.SetForegroundWindow(self.hwnd)
            finally:
                if attached:
                    user.AttachThreadInput(current, foreground_thread, False)
        if self.focus_process() is None:
            user.SetFocus(self.edit)

    def capture(self, detected_by: str) -> None:
        # Both native notifications and fallback polling run on the UI thread.
        # Keep timestamp, text and event publication in the same critical section.
        user = self.api.user
        with self.lock:
            changed = time.perf_counter()
            length = user.SendMessageW(self.edit, 0xE, 0, 0)
            buffer = ctypes.create_unicode_buffer(length + 1)
            user.SendMessageW(self.edit, 0xD, len(buffer), ctypes.addressof(buffer))
            if detected_by == "en_change" or buffer.value != self.last_text:
                self.events.append(Change(changed, buffer.value, self.focus_process(), detected_by))
            self.last_text = buffer.value

    def _loop(self) -> None:
        api, user = self.api, self.api.user
        class WNDCLASSW(ctypes.Structure):
            _fields_ = [("style", wt.UINT), ("lpfnWndProc", api.wndproc_type), ("cbClsExtra", ctypes.c_int),
                        ("cbWndExtra", ctypes.c_int), ("hInstance", wt.HINSTANCE), ("hIcon", wt.HICON),
                        ("hCursor", wt.HANDLE), ("hbrBackground", wt.HBRUSH), ("lpszMenuName", wt.LPCWSTR),
                        ("lpszClassName", wt.LPCWSTR)]

        def procedure(hwnd: Any, message: int, wparam: int, lparam: int) -> int:
            try:
                if message == 0x111 and (wparam >> 16) & 0xFFFF == 0x300 and lparam == self.edit:
                    self.capture("en_change")
                    return 0
                if message == 0x8000:
                    empty = ctypes.create_unicode_buffer("")
                    user.SendMessageW(self.edit, 0xC, 0, ctypes.addressof(empty))
                    user.SetFocus(self.edit)
                    user.SendMessageW(self.edit, 0xB1, 0, 0)  # EM_SETSEL / caret
                    with self.lock:
                        self.events.clear()
                        self.last_text = ""
                    return 0
                if message == 0x8001:
                    self.capture("poll")
                    return 0
                if message == 0x8002:
                    self._bring_foreground()
                    return 0
                if message == 0x10:
                    user.DestroyWindow(hwnd)
                    return 0
                if message == 2:
                    user.PostQuitMessage(0)
                    return 0
                return user.DefWindowProcW(hwnd, message, wparam, lparam)
            except BaseException as exc:
                self.failure = exc
                user.PostQuitMessage(1)
                return 0

        class_name = f"ASRE2E_{uuid.uuid4().hex}"
        instance = api.kernel.GetModuleHandleW(None)
        callback = api.wndproc_type(procedure)  # Keep alive for the whole loop.
        registered = False
        try:
            api.bind(user, "RegisterClassW", wt.ATOM, ctypes.POINTER(WNDCLASSW))
            definition = WNDCLASSW(0, callback, 0, 0, instance, None, None, 6, None, class_name)
            if not user.RegisterClassW(ctypes.byref(definition)):
                raise ctypes.WinError(ctypes.get_last_error())
            registered = True
            self.hwnd = user.CreateWindowExW(0, class_name, "ASR E2E measurement — do not touch keyboard/mouse",
                                             0x00CF0000, 100, 100, 800, 500, None, None, instance, None)
            if not self.hwnd:
                raise ctypes.WinError(ctypes.get_last_error())
            self.edit = user.CreateWindowExW(0x200, "EDIT", "", 0x503010C4, 10, 10, 760, 430,
                                             self.hwnd, 1, instance, None)
            if not self.edit:
                raise ctypes.WinError(ctypes.get_last_error())
            user.SendMessageW(self.edit, 0xC5, 0x7FFFFFFE, 0)  # EM_SETLIMITTEXT
            user.ShowWindow(self.hwnd, 5)
            self.ready.set()
            message = wt.MSG()
            while True:
                result = user.GetMessageW(ctypes.byref(message), None, 0, 0)
                if result == -1:
                    raise ctypes.WinError(ctypes.get_last_error())
                if result == 0:
                    break
                user.TranslateMessage(ctypes.byref(message))
                user.DispatchMessageW(ctypes.byref(message))
        except BaseException as exc:
            self.failure = exc
        finally:
            self.ready.set()
            if self.hwnd:
                user.DestroyWindow(self.hwnd)
                self.hwnd = None
            if registered:
                user.UnregisterClassW(class_name, instance)


class Clipboard:
    def __init__(self, api: WinAPI, hwnd: Any) -> None:
        self.api, self.hwnd = api, hwnd
        self.exclude_format = api.user.RegisterClipboardFormatW("ExcludeClipboardContentFromMonitorProcessing")
        if not self.exclude_format:
            raise BenchError("could not register clipboard monitor exclusion format")

    @contextlib.contextmanager
    def opened(self) -> Any:
        deadline = time.perf_counter() + 1
        while not self.api.user.OpenClipboard(self.hwnd):
            if time.perf_counter() >= deadline:
                raise BenchError("could not open clipboard")
            time.sleep(0.01)
        try:
            yield
        finally:
            self.api.user.CloseClipboard()

    def read(self) -> str | None:
        with self.opened():
            if not self.api.user.IsClipboardFormatAvailable(13):
                return None
            handle = self.api.user.GetClipboardData(13)
            pointer = self.api.kernel.GlobalLock(handle) if handle else None
            if not pointer:
                raise BenchError("could not read clipboard text")
            try:
                return ctypes.wstring_at(pointer)
            finally:
                self.api.kernel.GlobalUnlock(handle)

    def _set_data(self, format_id: int, data: bytes) -> None:
        handle = self.api.kernel.GlobalAlloc(0x2, len(data))
        if not handle:
            raise BenchError("could not allocate clipboard data")
        try:
            pointer = self.api.kernel.GlobalLock(handle)
            if not pointer:
                raise BenchError("could not lock clipboard data")
            try:
                ctypes.memmove(pointer, data, len(data))
            finally:
                self.api.kernel.GlobalUnlock(handle)
            if not self.api.user.SetClipboardData(format_id, handle):
                raise BenchError("could not set clipboard data")
            handle = None  # Clipboard now owns this allocation.
        finally:
            if handle:
                self.api.kernel.GlobalFree(handle)

    def write(self, text: str | None, *, exclude_from_history: bool = False) -> None:
        with self.opened():
            if not self.api.user.EmptyClipboard():
                raise BenchError("could not clear clipboard")
            if text is None:
                return
            if exclude_from_history:
                self._set_data(self.exclude_format, b"\0" * 4)
            self._set_data(13, (text + "\0").encode("utf-16-le"))


class WavePlayer:
    def __init__(self, api: WinAPI, prefix: str) -> None:
        self.api = api
        devices = []
        matches = []
        for index in range(api.winmm.waveOutGetNumDevs()):
            caps = WAVEOUTCAPSW()
            self.check(api.winmm.waveOutGetDevCapsW(index, ctypes.byref(caps), ctypes.sizeof(caps)), "get device")
            devices.append(caps.szPname)
            if caps.szPname.casefold().startswith(prefix.casefold()):
                matches.append((index, caps.szPname))
        if len(matches) != 1:
            raise BenchError(f"playback prefix {prefix!r} matched {len(matches)} devices; available: {devices}")
        self.device_id, self.device_name = matches[0]

    @staticmethod
    def check(code: int, operation: str) -> None:
        if code:
            raise BenchError(f"waveOut {operation} failed (MMRESULT {code})")

    def play(self, path: Path) -> tuple[float, float, float]:
        with wave.open(str(path), "rb") as wav:
            channels, rate, width, count = wav.getnchannels(), wav.getframerate(), wav.getsampwidth(), wav.getnframes()
            if width != 2 or wav.getcomptype() != "NONE":
                raise BenchError("waveOut requires PCM16 WAV")
            data = wav.readframes(count)
        if not count or len(data) != count * channels * width:
            raise BenchError("empty or truncated WAV")
        fmt = WAVEFORMATEX(1, channels, rate, rate * channels * width, channels * width, 16, 0)
        buffer = ctypes.create_string_buffer(data)
        header = WAVEHDR(ctypes.addressof(buffer), len(data), 0, 0, 0, 0, None, 0)
        done = threading.Event()
        completed = []

        def callback(handle: Any, message: int, instance: int, param1: int, param2: int) -> None:
            if message == 0x3BD:  # WOM_DONE, recorded on the winmm callback thread.
                completed.append(time.perf_counter())
                done.set()

        callback_pointer = self.api.waveproc_type(callback)
        handle = wt.HANDLE()
        mm = self.api.winmm
        self.check(mm.waveOutOpen(ctypes.byref(handle), self.device_id, ctypes.byref(fmt), callback_pointer, 0, 0x30000), "open")
        prepared = False
        try:
            self.check(mm.waveOutPrepareHeader(handle, ctypes.byref(header), ctypes.sizeof(header)), "prepare")
            prepared = True
            started = time.perf_counter()
            self.check(mm.waveOutWrite(handle, ctypes.byref(header), ctypes.sizeof(header)), "write")
            deadline = started + count / rate + 10
            while not done.wait(0.02):
                if time.perf_counter() >= deadline:
                    raise BenchError("waveOut buffer completion timed out")
            return started, completed[0], count / rate * 1000
        finally:
            # Reset before freeing buffers, including on Ctrl+C.
            mm.waveOutReset(handle)
            if prepared:
                mm.waveOutUnprepareHeader(handle, ctypes.byref(header), ctypes.sizeof(header))
            mm.waveOutClose(handle)


def run_benchmark(args: argparse.Namespace, manifest: dict[str, Any], conditions: dict[str, Any],
                  window: Any, audio: Any, clipboard: Any, keys: Keyboard,
                  clock: Any = time, windows_build: str = "unknown") -> dict[str, Any]:
    original = clipboard.read()
    if original is None:
        print("[bench_e2e] clipboard has no Unicode text; non-text formats cannot be restored; clearing at end", file=sys.stderr)
    try:
        print("[bench_e2e] do not touch keyboard/mouse during the run", file=sys.stderr)
        for remaining in (3, 2, 1):
            print(f"[bench_e2e] starting in {remaining}...", file=sys.stderr)
            clock.sleep(1)
        items = manifest["items"][:args.limit]
        samples = []
        consecutive_focus_lost = 0
        trials = [items[0]] * args.warmup + items
        for index, item in enumerate(trials):
            if index:
                clock.sleep(args.gap_s)
            sample = run_trial(item, args, window, audio, keys, clipboard, clock)
            consecutive_focus_lost = consecutive_focus_lost + 1 if sample["status"] == "focus_lost" else 0
            if consecutive_focus_lost >= 3:
                raise AbortRun("three consecutive focus_lost trials; aborting")
            if index >= args.warmup:
                samples.append(sample)
        return make_result(args, conditions, samples, audio.device_name, windows_build)
    finally:
        try:
            keys.cleanup(args.mode)
        finally:
            try:
                clock.sleep(args.gap_s)
            finally:
                clipboard.write(original)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--app-label", required=True, help="app name and version (metadata only)")
    parser.add_argument("--hotkey", required=True, help="e.g. Ctrl+Win or Ctrl+Shift+Space")
    parser.add_argument("--mode", choices=("hold", "toggle"), default="hold")
    parser.add_argument("--playback-device", default="CABLE Input", help="unique waveOut device name prefix (max 31 chars)")
    parser.add_argument("--pre-roll-ms", type=float, default=600)
    parser.add_argument("--tail-ms", type=float, default=300)
    parser.add_argument("--settle-ms", type=float, default=1500)
    parser.add_argument("--timeout-s", type=float, default=30)
    parser.add_argument("--gap-s", type=float, default=2)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--limit", type=int)
    parser.add_argument("--condition", action="append", default=[], metavar="KEY=VALUE")
    parser.add_argument("--output", type=Path, help="new JSON file; existing files are never overwritten")
    return parser.parse_args(argv)


def validate(args: argparse.Namespace) -> tuple[dict[str, Any], dict[str, Any]]:
    parse_hotkey(args.hotkey)
    if args.warmup < 0 or (args.limit is not None and args.limit < 1):
        raise BenchError("warmup must be nonnegative; limit must be positive")
    for name in ("pre_roll_ms", "tail_ms", "settle_ms", "timeout_s", "gap_s"):
        value = getattr(args, name)
        if not math.isfinite(value) or value < 0 or (name in ("settle_ms", "timeout_s") and value == 0):
            raise BenchError(f"{name} must be finite and {'positive' if name in ('settle_ms', 'timeout_s') else 'nonnegative'}")
    if not args.playback_device.strip() or len(args.playback_device) > 31:
        raise BenchError("playback-device must be a nonempty prefix of at most 31 characters")
    if not args.app_label.strip():
        raise BenchError("app-label must not be empty")
    args.conditions = {}
    for condition in args.condition:
        key, separator, value = condition.partition("=")
        if not separator or not key.strip() or key in args.conditions:
            raise BenchError("conditions must be unique nonempty KEY=VALUE pairs")
        args.conditions[key] = value
    manifest, conditions = load_manifest(args.manifest)
    for item in manifest["items"]:
        if item["language"].replace("_", "-").lower().split("-")[0] != "ja":
            raise BenchError("product E2E currently requires a Japanese manifest")
        try:
            with wave.open(item["audio_path"], "rb") as wav:
                if (wav.getnchannels(), wav.getsampwidth(), wav.getframerate(), wav.getcomptype()) != (1, 2, 16000, "NONE"):
                    raise BenchError("expected 16 kHz mono PCM16 WAV")
                if not wav.getnframes() or len(wav.readframes(wav.getnframes())) != wav.getnframes() * 2:
                    raise BenchError("empty or truncated WAV")
        except (OSError, EOFError, wave.Error, BenchError) as exc:
            raise BenchError(f"item {item['id']!r}: {exc}") from exc
    return manifest, conditions


def execute(args: argparse.Namespace, manifest: dict[str, Any], conditions: dict[str, Any]) -> dict[str, Any]:
    api = WinAPI()
    audio = WavePlayer(api, args.playback_device)
    with TargetWindow(api) as window:
        clipboard = Clipboard(api, window.hwnd)
        keys = Keyboard(api.send, window, parse_hotkey(args.hotkey), key_down=api.key_down, idle_seconds=api.idle_seconds)
        version = sys.getwindowsversion()
        with ConsoleKeyGuard(api, keys, args.mode):
            return run_benchmark(args, manifest, conditions, window, audio, clipboard, keys,
                                 windows_build=f"{version.major}.{version.minor}.{version.build}")


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    stream = None
    created = None
    try:
        manifest, conditions = validate(args)
        if args.output is not None:
            stream = args.output.open("x", encoding="utf-8", newline="\n")
            created = os.fstat(stream.fileno())
        result = execute(args, manifest, conditions)
        rendered = json.dumps(result, ensure_ascii=False, indent=2) + "\n"
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
        print(f"[bench_e2e] error: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("[bench_e2e] interrupted", file=sys.stderr)
        return 130
    finally:
        if stream is not None:
            with contextlib.suppress(OSError):
                stream.close()
            with contextlib.suppress(OSError):
                current = args.output.lstat()
                if created is not None and current.st_size == 0 and os.path.samestat(created, current):
                    args.output.unlink()


if __name__ == "__main__":
    raise SystemExit(main())
