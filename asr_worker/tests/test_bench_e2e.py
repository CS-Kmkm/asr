from __future__ import annotations

import contextlib
import ctypes
import importlib.util
import io
import json
import math
import os
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch
import wave


_SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "bench_e2e.py"


def _load_bench_module():  # noqa: ANN202
    spec = importlib.util.spec_from_file_location("bench_e2e", _SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


bench = _load_bench_module()


class FakeClock:
    def __init__(self) -> None:
        self.now = 0.0

    def perf_counter(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.now += seconds


class FakeWindow:
    def __init__(self, clock: FakeClock) -> None:
        self.clock = clock
        self.events = []
        self.process = None
        self.prepare_count = 0
        self.refocus_calls = []
        self.can_refocus = True
        self.lock = threading.Lock()

    def prepare(self) -> None:
        self.prepare_count += 1
        self.process = None
        self.events.clear()

    def snapshot(self, clock: FakeClock) -> tuple[float, list]:
        with self.lock:
            return clock.perf_counter(), [event for event in self.events if event.at <= self.clock.now]

    def text_at_stop(self) -> tuple[str, bool]:
        _, events = self.snapshot(self.clock)
        return (events[-1].text, True) if events else ("", False)

    def refocus(self, *, timeout_s: float) -> bool:
        self.refocus_calls.append(timeout_s)
        if self.can_refocus:
            self.process = None
        return self.can_refocus

    def focus_process(self) -> str | None:
        return self.process


class FakeClipboard:
    def __init__(self, text: str | None = "original clipboard") -> None:
        self.text = text
        self.writes = []
        self.exclusions = []

    def read(self) -> str | None:
        return self.text

    def write(self, text: str | None, *, exclude_from_history: bool = False) -> None:
        self.writes.append(text)
        self.exclusions.append(exclude_from_history)
        self.text = text


class FakeAudio:
    device_name = "CABLE Input (fake)"

    def __init__(self, clock: FakeClock) -> None:
        self.clock = clock
        self.calls = []

    def play(self, path: Path) -> tuple[float, float, float]:
        self.calls.append(path)
        self.clock.sleep(0.025)
        started = self.clock.now
        self.clock.sleep(0.1)
        return started, self.clock.now, 100.0


class SyntheticFixture(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.wav = self.root / "tiny.wav"
        with wave.open(str(self.wav), "wb") as wav:
            wav.setparams((1, 2, 16000, 0, "NONE", "not compressed"))
            wav.writeframes(b"\0\0" * 1600)
        self.manifest_path = self.root / "manifest.json"
        self.item = {"id": "tiny", "audio_path": "tiny.wav", "reference": "合成音声", "language": "ja-JP", "source": "synthetic"}
        self.manifest_path.write_text(json.dumps({"schema": "asr-bench-manifest/1", "name": "synthetic",
                                                 "sources": ["synthetic"], "items": [self.item]}), encoding="utf-8")
        self.argv = ["--manifest", str(self.manifest_path), "--app-label", "fake-1", "--hotkey", "Ctrl+Win"]
        self.args = bench.parse_args(self.argv)
        self.manifest, self.conditions = bench.validate(self.args)
        self.item = self.manifest["items"][0]
        self.clock = FakeClock()
        self.window = FakeWindow(self.clock)
        self.audio = FakeAudio(self.clock)
        self.clipboard = FakeClipboard()
        self.sent = []
        self.keys = bench.Keyboard(self.send, self.window, bench.parse_hotkey(self.args.hotkey), self.clock)

    def send(self, events: list[tuple[int, bool]]) -> None:
        self.sent.append((self.clock.now, events))

    def trial(self) -> dict:
        return bench.run_trial(self.item, self.args, self.window, self.audio, self.keys, self.clipboard, self.clock)

    def paste_on_stop(self, texts: list[tuple[float, str, str | None]] | None = None,
                      *, clipboard_changed: bool = False) -> None:
        texts = texts if texts is not None else [(0.2, "合成音声", None)]
        sends = 0

        def send(events: list[tuple[int, bool]]) -> None:
            nonlocal sends
            self.send(events)
            sends += 1
            stop_batch = 2 if self.args.mode == "hold" else 3
            if sends == stop_batch:
                self.window.events.extend(bench.Change(self.clock.now + delay, text, process) for delay, text, process in texts)
                if clipboard_changed:
                    self.clipboard.text = "app text"

        self.keys.send = send


class HotkeyTests(unittest.TestCase):
    def test_supported_chords_and_modifier_order(self) -> None:
        cases = {"Ctrl+Win": (0x11, 0x5B), "Ctrl+Shift+Space": (0x11, 0x10, 0x20),
                 "Ctrl+Alt+V": (0x11, 0x12, 0x56), "F24+Win": (0x5B, 0x87),
                 " ctrl + f1 ": (0x11, 0x70), "Ctrl+9": (0x11, 0x39), "Win+Alt": (0x5B, 0x12)}
        for value, expected in cases.items():
            with self.subTest(value=value):
                self.assertEqual(bench.parse_hotkey(value), expected)

    def test_invalid_hotkeys(self) -> None:
        for value in ("", "Ctrl+", "Ctrl+Ctrl", "V+B", "F0", "F25", "Enter", "あ", "Ctrl++V",
                      "Alt", "Ctrl", "Ctrl+Alt", "Shift+V", "Shift", "9", "F1"):
            with self.subTest(value=value), self.assertRaises(bench.BenchError):
                bench.parse_hotkey(value)

    def test_release_order_and_start_menu_suppression(self) -> None:
        clock = FakeClock()
        window = FakeWindow(clock)
        send = Mock()
        keys = bench.Keyboard(send, window, bench.parse_hotkey("Ctrl+Win+V"), clock)
        keys.down()
        keys.release()
        self.assertEqual(send.call_args_list[0].args[0], [(0x11, False), (0x5B, False), (0x56, False)])
        self.assertEqual(send.call_args_list[1].args[0], [(0x56, True), (0xE8, False), (0xE8, True), (0x5B, True), (0x11, True)])
        keys.release(emergency=True)
        self.assertEqual(send.call_count, 2)

    def test_focus_checked_on_down_and_release_but_emergency_releases(self) -> None:
        clock = FakeClock()
        window = FakeWindow(clock)
        send = Mock()
        keys = bench.Keyboard(send, window, (0x11,), clock)
        window.process = "other.exe"
        with self.assertRaises(bench.FocusLost):
            keys.down()
        send.assert_not_called()
        window.process = None
        keys.down()
        window.process = "other.exe"
        with self.assertRaises(bench.FocusLost):
            keys.release()
        self.assertEqual(send.call_count, 1)
        keys.release(emergency=True)
        self.assertEqual(send.call_args.args[0], [(0x11, True)])

    def test_native_input_layout_flags_and_partial_failure(self) -> None:
        api = bench.WinAPI.__new__(bench.WinAPI)
        captured = []

        def send(count, inputs, size):  # noqa: ANN001, ANN202
            captured.extend((item.type, item.ki.wVk, item.ki.dwFlags) for item in inputs)
            self.assertEqual(size, ctypes.sizeof(bench.INPUT))
            return count

        api.user = Mock(SendInput=send)
        api.send([(0x11, False), (0x5B, True), (0xE8, True)])
        self.assertEqual(captured, [(1, 0x11, 0), (1, 0x5B, 3), (1, 0xE8, 2)])
        api.user.SendInput = Mock(return_value=0)
        with self.assertRaisesRegex(bench.BenchError, "partial"):
            api.send([(0x11, False)])

    def test_stop_timestamp_comes_from_sender_after_native_input_preparation(self) -> None:
        clock = FakeClock()
        window = FakeWindow(clock)

        def send(events: list[tuple[int, bool]]) -> float:
            clock.sleep(0.025)
            return clock.now

        keys = bench.Keyboard(send, window, (0x11,), clock)
        self.assertAlmostEqual(keys.down(), 0.025)
        self.assertAlmostEqual(keys.release(), 0.05)
        self.assertAlmostEqual(keys.tap(), 0.075)
        self.assertAlmostEqual(clock.now, 0.14)

    def test_modifier_only_alt_release_is_masked_and_non_win_chord_is_not(self) -> None:
        clock = FakeClock()
        send = Mock(return_value=None)
        keys = bench.Keyboard(send, FakeWindow(clock), bench.parse_hotkey("Win+Alt"), clock)
        keys.down()
        keys.release()
        self.assertEqual(send.call_args.args[0], [(0xE8, False), (0xE8, True), (0x12, True),
                                                (0xE8, False), (0xE8, True), (0x5B, True)])
        keys.chord = bench.parse_hotkey("Ctrl+Alt+V")
        keys.down()
        keys.release()
        self.assertEqual(send.call_args.args[0], [(0x56, True), (0x12, True), (0x11, True)])

    def test_release_verifies_all_injected_keys_and_retries_only_stuck_key_ups(self) -> None:
        clock = FakeClock()
        state = set()
        batches = []

        def send(events: list[tuple[int, bool]]) -> None:
            batches.append(events)
            for key, up in events:
                if not up:
                    state.add(key)
                elif len(batches) != 2 or key != 0x11:
                    state.discard(key)

        keys = bench.Keyboard(send, FakeWindow(clock), (0x11, 0x5B), clock, key_down=lambda key: key in state)
        keys.down()
        keys.release()
        self.assertEqual(batches[-1], [(0x11, True)])
        self.assertFalse(state)
        self.assertEqual(keys.pressed, [])
        keys.key_down = lambda key: key == 0xE8
        keys.down()
        with self.assertRaisesRegex(bench.AbortRun, "STILL DOWN"):
            keys.release()
        self.assertEqual(batches[-1], [(0xE8, True)])

    def test_physical_modifiers_and_non_idle_input_abort_before_injection(self) -> None:
        clock = FakeClock()
        send = Mock()
        keys = bench.Keyboard(send, FakeWindow(clock), (0x11,), clock, key_down=lambda key: key == 0xA4)
        with self.assertRaisesRegex(bench.AbortRun, "physical modifier"):
            keys.before_trial()
        keys.key_down = lambda key: False
        keys.idle_seconds = lambda: 0
        with self.assertRaisesRegex(bench.AbortRun, "within 10 seconds"):
            keys.before_trial()
        self.assertAlmostEqual(clock.now, 10)
        send.assert_not_called()
        keys.idle_seconds = lambda: clock.now - 10
        keys.before_trial()
        self.assertGreaterEqual(clock.now, 11)
        self.assertLess(clock.now, 11.051)

    def test_native_key_state_and_idle_clock_wrap_use_fake_apis(self) -> None:
        api = bench.WinAPI.__new__(bench.WinAPI)
        api.user = Mock()
        api.kernel = Mock()
        api.user.GetAsyncKeyState.return_value = -32768
        self.assertTrue(api.key_down(0x11))
        api.user.GetAsyncKeyState.return_value = 1  # Low bit is not the current down state.
        self.assertFalse(api.key_down(0x11))

        def last_input(pointer):  # noqa: ANN001, ANN202
            self.assertEqual(pointer._obj.cbSize, ctypes.sizeof(bench.LASTINPUTINFO))
            pointer._obj.dwTime = 0xFFFFFF00
            return True

        api.user.GetLastInputInfo.side_effect = last_input
        api.kernel.GetTickCount.return_value = 0x100
        self.assertAlmostEqual(api.idle_seconds(), 0.512)
        api.user.GetLastInputInfo.side_effect = lambda p: False
        with self.assertRaises(bench.AbortRun):
            api.idle_seconds()

    def test_console_close_logoff_shutdown_release_and_unregister(self) -> None:
        api = Mock()
        api.ctrlhandler_type = lambda callback: callback
        keys = Mock()
        with bench.ConsoleKeyGuard(api, keys, "hold") as guard:
            for event in (2, 5, 6):
                self.assertFalse(guard._handle(event))
            self.assertFalse(guard._handle(0))
        self.assertEqual(keys.cleanup.call_count, 3)
        keys.cleanup.assert_called_with("hold")
        self.assertEqual(keys.shutdown.set.call_count, 3)
        self.assertEqual([call.args[1] for call in api.kernel.SetConsoleCtrlHandler.call_args_list], [True, False])


class TrialTests(SyntheticFixture):
    def test_hold_stop_is_release_call_and_settle_uses_last_event(self) -> None:
        self.paste_on_stop([(0.2, " 合成", None), (0.5, " 合成音声 \n", None)])
        result = self.trial()
        self.assertEqual(result["status"], "ok")
        self.assertEqual(result["text"], "合成音声")
        self.assertEqual(result["change_events"], 2)
        self.assertAlmostEqual(self.sent[1][0], 0.6 + 0.025 + 0.1 + 0.3)
        self.assertAlmostEqual(result["stop_to_first_paste_ms"], 200)
        self.assertAlmostEqual(result["stop_to_settled_ms"], 500)
        self.assertGreaterEqual(self.clock.now, self.sent[1][0] + 2)
        self.assertLess(self.clock.now, self.sent[1][0] + 2.011)
        self.assertAlmostEqual(result["wall_ms"], 200)
        self.assertAlmostEqual(result["play_start_lag_ms"], 25)
        self.assertAlmostEqual(result["audio_duration_ms"], 100)
        self.assertTrue(result["clipboard_restored"])
        self.assertTrue(math.isnan(result["duration_ms"]))
        self.assertEqual(self.keys.pressed, [])

    def test_toggle_stop_is_second_down_not_key_up(self) -> None:
        self.args.mode = "toggle"
        sends = 0

        def send(events: list[tuple[int, bool]]) -> None:
            nonlocal sends
            self.send(events)
            sends += 1
            if sends == 3:
                self.window.events.append(bench.Change(self.clock.now + 0.2, "合成音声"))
            self.clock.sleep(0.05)

        self.keys.send = send
        result = self.trial()
        self.assertEqual(result["status"], "ok")
        self.assertEqual(len(self.sent), 4)
        self.assertAlmostEqual(result["stop_to_first_paste_ms"], 200)
        self.assertAlmostEqual(self.sent[2][0], 0.1 + 0.04 + 0.6 + 0.025 + 0.1 + 0.3)

    def test_empty_and_no_change_timeout(self) -> None:
        self.paste_on_stop([(0.1, " \n", None)])
        result = self.trial()
        self.assertEqual(result["status"], "empty")
        self.assertEqual(result["change_events"], 1)
        self.assertTrue(math.isnan(result["wall_ms"]))
        self.keys.send = self.send
        self.args.timeout_s = 0.5
        result = self.trial()
        self.assertEqual(result["status"], "timeout")
        self.assertEqual(result["change_events"], 0)
        self.assertIsNone(result["clipboard_restored"])
        self.assertTrue(math.isnan(result["stop_to_first_paste_ms"]))

    def test_changing_text_at_deadline_is_error_not_success(self) -> None:
        self.args.timeout_s = 0.6
        self.paste_on_stop([(0.1, "合成", None), (0.5, "合成音声", None)])
        result = self.trial()
        self.assertEqual(result["status"], "error")
        self.assertEqual(result["change_events"], 2)
        self.assertIn("settle", result["error"])
        self.assertTrue(math.isnan(result["wall_ms"]))

    def test_early_and_late_events_are_excluded(self) -> None:
        self.args.timeout_s = 2
        self.paste_on_stop([(-0.1, "preview", None), (0.2, "合成音声", None), (2.1, "too late", "other.exe")])
        result = self.trial()
        self.assertEqual(result["status"], "ok")
        self.assertEqual(result["text"], "合成音声")
        self.assertEqual(result["change_events"], 1)
        self.assertTrue(result["pre_stop_changes"])

    def test_focus_loss_at_paste_is_recorded_even_if_now_restored(self) -> None:
        self.paste_on_stop([(0.2, "合成音声", "notepad.exe")])
        result = self.trial()
        self.assertEqual(result["status"], "focus_lost")
        self.assertEqual(result["focus_process"], "notepad.exe")
        self.assertTrue(math.isnan(result["wall_ms"]))

    def test_focus_loss_before_stop_releases_only_then_next_trial_refocuses(self) -> None:
        play = self.audio.play

        def steal(path: Path) -> tuple:
            played = play(path)
            self.window.process = "other.exe"
            return played

        self.audio.play = steal
        result = self.trial()
        self.assertEqual(result["status"], "focus_lost")
        self.assertEqual(result["focus_process"], "other.exe")
        self.assertEqual(len(self.sent), 2)  # Initial down, emergency key-up.
        self.assertEqual(self.keys.pressed, [])
        self.assertEqual(self.window.refocus_calls, [0.5])
        self.audio.play = play
        self.paste_on_stop()
        self.assertEqual(self.trial()["status"], "ok")
        self.assertEqual(self.window.prepare_count, 2)

    def test_focus_loss_during_stop_call_is_detected(self) -> None:
        def send(events: list[tuple[int, bool]]) -> None:
            self.send(events)
            if len(self.sent) == 2:
                self.window.process = "other.exe"

        self.keys.send = send
        self.assertEqual(self.trial()["status"], "focus_lost")

    def test_changed_clipboard_is_reported_without_failing(self) -> None:
        self.paste_on_stop(clipboard_changed=True)
        result = self.trial()
        self.assertEqual(result["status"], "ok")
        self.assertFalse(result["clipboard_restored"])

    def test_unreadable_clipboard_preserves_status_and_records_error(self) -> None:
        self.paste_on_stop()
        self.clipboard.read = Mock(side_effect=bench.BenchError("busy"))
        result = self.trial()
        self.assertEqual(result["status"], "ok")
        self.assertEqual(result["clipboard_error"], "busy")
        self.assertAlmostEqual(result["wall_ms"], 200)
        self.assertIsNone(result["clipboard_restored"])
        self.assertEqual(self.keys.pressed, [])

    def test_audio_error_and_interrupt_always_release_keys(self) -> None:
        self.audio.play = Mock(side_effect=bench.BenchError("fake audio error"))
        self.assertEqual(self.trial()["status"], "error")
        self.assertEqual(self.keys.pressed, [])
        self.audio.play = Mock(side_effect=KeyboardInterrupt)
        with self.assertRaises(KeyboardInterrupt):
            self.trial()
        self.assertEqual(self.keys.pressed, [])

    def test_partial_keydown_is_released_and_cleanup_failure_aborts(self) -> None:
        calls = 0

        def send(events: list[tuple[int, bool]]) -> None:
            nonlocal calls
            calls += 1
            if calls == 1:
                raise bench.BenchError("partial send")
            self.send(events)

        self.keys.send = send
        result = self.trial()
        self.assertEqual(result["status"], "error")
        self.assertEqual(self.keys.pressed, [])
        self.keys.send = Mock(side_effect=bench.BenchError("cannot send"))
        with self.assertRaisesRegex(bench.BenchError, "release"):
            self.trial()

    def test_toggle_failures_between_taps_always_abort_after_key_release(self) -> None:
        self.args.mode = "toggle"
        for error in (bench.BenchError("audio failed"), KeyboardInterrupt()):
            with self.subTest(error=type(error).__name__):
                self.audio.play = Mock(side_effect=error)
                with self.assertRaisesRegex(bench.AbortRun, "desynchronized"):
                    self.trial()
                self.assertEqual(self.keys.pressed, [])
        self.audio.play = FakeAudio(self.clock).play

        def fail_second_down(events: list[tuple[int, bool]]) -> None:
            self.send(events)
            if len(self.sent) == 3:
                raise bench.BenchError("partial second tap")

        self.sent.clear()
        self.keys.send = fail_second_down
        with self.assertRaisesRegex(bench.AbortRun, "desynchronized"):
            self.trial()
        self.assertEqual(self.keys.pressed, [])
        self.assertEqual(len(self.sent), 4)

    def test_hold_failed_refocus_releases_and_aborts(self) -> None:
        play = self.audio.play

        def steal(path: Path) -> tuple:
            result = play(path)
            self.window.process = "other.exe"
            return result

        self.audio.play = steal
        self.window.can_refocus = False
        with self.assertRaisesRegex(bench.AbortRun, "release stopped recording"):
            self.trial()
        self.assertEqual(self.window.refocus_calls, [0.5])
        self.assertEqual(self.keys.pressed, [])
        self.assertEqual(len(self.sent), 2)

    def test_stopping_capture_strips_only_pre_stop_prefix(self) -> None:
        play = self.audio.play

        def preview(path: Path) -> tuple:
            result = play(path)
            self.window.events.append(bench.Change(self.clock.now, "先行文"))
            return result

        self.audio.play = preview
        self.paste_on_stop([(0.2, "先行文合成音声", None)])
        result = self.trial()
        self.assertEqual(result["text"], "合成音声")
        self.assertTrue(result["pre_stop_changes"])
        self.assertEqual(result["change_events"], 1)
        self.assertEqual(result["detected_by"], "en_change")
        self.assertAlmostEqual(result["stop_to_first_paste_ms"], 200)
        self.paste_on_stop([(0.2, "置換された文", None)])
        result = self.trial()
        self.assertEqual(result["text"], "置換された文")
        self.assertTrue(result["pre_stop_changes"])

    def test_poll_detection_uses_same_first_and_settle_semantics(self) -> None:
        def send(events: list[tuple[int, bool]]) -> None:
            self.send(events)
            if len(self.sent) == 2:
                self.window.events.append(bench.Change(self.clock.now + 0.2, "合成音声", detected_by="poll"))

        self.keys.send = send
        result = self.trial()
        self.assertEqual(result["status"], "ok")
        self.assertEqual(result["detected_by"], "poll")
        self.assertAlmostEqual(result["stop_to_first_paste_ms"], 200)
        self.assertAlmostEqual(result["stop_to_settled_ms"], 200)

    def test_no_sentinel_or_no_change_means_null_clipboard_result(self) -> None:
        self.clipboard.write = Mock(side_effect=bench.BenchError("unavailable"))
        self.clipboard.read = Mock(side_effect=AssertionError("must not read"))
        result = self.trial()
        self.assertEqual(result["status"], "error")
        self.assertIsNone(result["clipboard_restored"])
        self.assertEqual(self.sent, [])

    def test_dead_window_aborts_instead_of_recording_trial_error(self) -> None:
        self.window.snapshot = Mock(side_effect=bench.AbortRun("window closed"))
        with self.assertRaisesRegex(bench.BenchError, "window closed"):
            self.trial()
        self.assertEqual(self.keys.pressed, [])


class RunAndOutputTests(SyntheticFixture):
    def run_fake(self) -> dict:
        with contextlib.redirect_stderr(io.StringIO()):
            return bench.run_benchmark(self.args, self.manifest, self.conditions, self.window, self.audio,
                                       self.clipboard, self.keys, self.clock, windows_build="10.0.fake")

    def test_countdown_warmup_gap_unique_sentinels_and_clipboard_restore(self) -> None:
        self.args.warmup = 2
        self.args.gap_s = 0.2
        self.args.limit = 1
        self.args.timeout_s = 0.1
        result = self.run_fake()
        self.assertEqual(len(self.audio.calls), 3)
        self.assertEqual(len(result["warm"]["samples"]), 1)
        self.assertEqual(len(result["cold"]["samples"]), 0)
        self.assertAlmostEqual(self.clock.now, 3 + 0.6 + 3 * (0.6 + 0.125 + 0.3 + 0.1))
        self.assertEqual(self.clipboard.text, "original clipboard")
        self.assertEqual(len(set(self.clipboard.writes[:-1])), 3)
        self.assertEqual(self.clipboard.exclusions, [True, True, True, False])
        self.assertTrue(result["config"]["win_mask_used"])
        self.assertEqual(result["config"]["windows_build"], "10.0.fake")
        self.assertEqual(result["config"]["playback_device"], "CABLE Input (fake)")

    def test_clipboard_restored_on_interrupt_and_nontext_cleared(self) -> None:
        self.audio.play = Mock(side_effect=KeyboardInterrupt)
        with self.assertRaises(KeyboardInterrupt):
            self.run_fake()
        self.assertEqual(self.clipboard.text, "original clipboard")
        self.clipboard.text = None
        self.args.timeout_s = 0.1
        self.audio.play = FakeAudio(self.clock).play
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            bench.run_benchmark(self.args, self.manifest, self.conditions, self.window, self.audio,
                                self.clipboard, self.keys, self.clock)
        self.assertIsNone(self.clipboard.text)
        self.assertIn("non-text", stderr.getvalue())

    def test_stats_and_json_can_be_scored_by_shared_scorer(self) -> None:
        self.paste_on_stop()
        ok = self.trial()
        error = {**ok, "status": "error", "wall_ms": math.nan, "clipboard_restored": False}
        result = bench.make_result(self.args, self.conditions, [ok, {**ok, "wall_ms": 400,
                                   "stop_to_first_paste_ms": 400, "stop_to_settled_ms": 700}, error], "fake", "fake")
        self.assertEqual(result["e2e"]["counts"], {"ok": 2, "empty": 0, "timeout": 0, "focus_lost": 0, "error": 1})
        self.assertAlmostEqual(result["e2e"]["success_rate"], 2 / 3)
        self.assertAlmostEqual(result["e2e"]["clipboard_restored_rate"], 2 / 3)
        self.assertAlmostEqual(result["e2e"]["stop_to_first_paste_ms"]["p50"], 300)
        self.assertAlmostEqual(result["e2e"]["stop_to_first_paste_ms"]["p95"], 390)
        self.assertEqual(result["warm"]["wall_ms"]["count"], 2)
        self.assertTrue(math.isnan(result["warm"]["duration_ms"]["p50"]))
        result_path = self.root / "result.json"
        result_path.write_text(json.dumps(result, ensure_ascii=False), encoding="utf-8")
        scorer = sys.modules["_bench_score_manifest"]
        score_path = self.root / "score.json"
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            code = scorer.main(["--bench", str(result_path), "--manifest", str(self.manifest_path),
                                "--json", "--output", str(score_path)])
        self.assertEqual(code, 0)
        scored = json.loads(score_path.read_text(encoding="utf-8"))
        self.assertEqual(scored["rows"][0]["raw"]["cer"]["cer"], 0)
        self.assertEqual(scored["missing_items"], [])

    def test_empty_stats_are_nan(self) -> None:
        self.assertTrue(math.isnan(bench.summarize([])["p50"]))
        self.assertEqual(bench.summarize([math.nan, 7])["p95"], 7)

    def test_clipboard_rate_denominator_excludes_trials_without_changes(self) -> None:
        self.paste_on_stop()
        ok = self.trial()
        result = bench.make_result(self.args, self.conditions, [ok, {**ok, "status": "timeout", "change_events": 0,
                                   "clipboard_restored": None}], "fake", "fake")
        self.assertEqual(result["e2e"]["clipboard_restored_rate"], 1)
        self.args.hotkey = "Ctrl+Alt+V"
        result = bench.make_result(self.args, self.conditions, [], "fake", "fake")
        self.assertFalse(result["config"]["win_mask_used"])
        self.assertTrue(math.isnan(result["e2e"]["clipboard_restored_rate"]))

    def test_three_consecutive_focus_losses_abort_even_during_warmup(self) -> None:
        self.args.warmup = 5
        lost = {"status": "focus_lost"}
        with patch.object(bench, "run_trial", return_value=lost) as trial:
            with self.assertRaisesRegex(bench.AbortRun, "three consecutive"):
                self.run_fake()
        self.assertEqual(trial.call_count, 3)
        self.assertEqual(self.clipboard.text, "original clipboard")

    def test_terminal_gap_precedes_clipboard_restoration(self) -> None:
        self.args.timeout_s = 0.1
        observed = []
        write = self.clipboard.write

        def track(text: str | None, **kwargs) -> None:
            if text == "original clipboard":
                observed.append(self.clock.now)
            write(text, **kwargs)

        self.clipboard.write = track
        self.run_fake()
        self.assertEqual(observed, [self.clock.now])
        self.assertGreaterEqual(self.clock.now - self.sent[-1][0], self.args.gap_s + self.args.timeout_s)

    def main_fake(self, output: Path, execute: object) -> tuple[int, str]:
        stdout = io.StringIO()
        with patch.object(bench, "execute", execute), contextlib.redirect_stderr(io.StringIO()), contextlib.redirect_stdout(stdout):
            code = bench.main(self.argv + ["--output", str(output)])
        return code, stdout.getvalue()

    def test_existing_output_is_never_overwritten_even_when_empty(self) -> None:
        output = self.root / "out.json"
        for content in ("keep", ""):
            output.write_text(content, encoding="utf-8")
            execute = Mock()
            code, _ = self.main_fake(output, execute)
            self.assertEqual(code, 1)
            self.assertEqual(output.read_text(encoding="utf-8"), content)
            execute.assert_not_called()

    def test_validation_happens_before_output_reservation(self) -> None:
        self.wav.unlink()
        output = self.root / "out.json"
        code, _ = self.main_fake(output, Mock())
        self.assertEqual(code, 1)
        self.assertFalse(output.exists())

    def test_run_failure_and_interrupt_remove_only_own_empty_output(self) -> None:
        output = self.root / "out.json"
        for error, expected in ((bench.BenchError("failed"), 1), (KeyboardInterrupt(), 130)):
            code, _ = self.main_fake(output, Mock(side_effect=error))
            self.assertEqual(code, expected)
            self.assertFalse(output.exists())

    def test_successful_output_and_stdout_fallback(self) -> None:
        output = self.root / "out.json"
        execute = Mock(return_value={"warm": {"samples": []}})
        code, stdout = self.main_fake(output, execute)
        self.assertEqual(code, 0)
        self.assertEqual(stdout, "")
        self.assertEqual(json.loads(output.read_text(encoding="utf-8")), execute.return_value)
        original_open = Path.open
        fallback_output = self.root / "fallback.json"

        class FailingWriter:
            def __init__(self, stream):  # noqa: ANN001
                self.stream = stream

            def fileno(self) -> int:
                return self.stream.fileno()

            def write(self, text: str) -> None:
                raise OSError("disk full")

            def close(self) -> None:
                self.stream.close()

        def open_file(path, *args, **kwargs):  # noqa: ANN001, ANN202
            stream = original_open(path, *args, **kwargs)
            return FailingWriter(stream) if path == fallback_output else stream

        with patch.object(Path, "open", open_file):
            code, stdout = self.main_fake(fallback_output, execute)
        self.assertEqual(code, 1)
        self.assertEqual(json.loads(stdout), execute.return_value)
        self.assertFalse(fallback_output.exists())

    def test_cleanup_keeps_replacement_and_nonempty_files(self) -> None:
        output = self.root / "out.json"

        # Windows may prohibit renaming the open reservation. A fake stat
        # comparison still exercises the identity gate without relying on it.
        with patch.object(os.path, "samestat", return_value=False):
            code, _ = self.main_fake(output, Mock(side_effect=bench.BenchError("fail")))
        self.assertEqual(code, 1)
        self.assertTrue(output.exists())
        output.unlink()

        def nonempty(*unused):  # noqa: ANN002, ANN202
            output.write_text("partial", encoding="utf-8")
            raise bench.BenchError("fail")

        code, _ = self.main_fake(output, nonempty)
        self.assertEqual(code, 1)
        self.assertEqual(output.read_text(encoding="utf-8"), "partial")


class ValidationTests(SyntheticFixture):
    def test_invalid_flags_and_conditions(self) -> None:
        for flags in (["--warmup", "-1"], ["--limit", "0"], ["--timeout-s", "nan"], ["--gap-s", "-1"],
                      ["--tail-ms", "inf"], ["--settle-ms", "0"], ["--pre-roll-ms", "-1"],
                      ["--condition", "missing"], ["--condition", "=bad"],
                      ["--condition", "a=1", "--condition", "a=2"], ["--playback-device", ""],
                      ["--playback-device", "x" * 32]):
            with self.subTest(flags=flags), self.assertRaises(bench.BenchError):
                bench.validate(bench.parse_args(self.argv + flags))
        args = bench.parse_args(self.argv + ["--condition", "model=local=1", "--condition", "cleanup=off"])
        bench.validate(args)
        self.assertEqual(args.conditions, {"model": "local=1", "cleanup": "off"})

    def test_rejects_wrong_audio_format_and_truncated_wav(self) -> None:
        with wave.open(str(self.wav), "wb") as wav:
            wav.setparams((1, 2, 8000, 0, "NONE", "not compressed"))
            wav.writeframes(b"\0\0" * 10)
        with self.assertRaisesRegex(bench.BenchError, "16 kHz"):
            bench.validate(self.args)
        with wave.open(str(self.wav), "wb") as wav:
            wav.setparams((1, 2, 16000, 0, "NONE", "not compressed"))
            wav.writeframes(b"\0\0" * 10)
        self.wav.write_bytes(self.wav.read_bytes()[:-2])
        with self.assertRaisesRegex(bench.BenchError, "truncated"):
            bench.validate(self.args)

    def test_rejects_non_japanese_manifest(self) -> None:
        data = json.loads(self.manifest_path.read_text(encoding="utf-8"))
        data["items"][0]["language"] = "en-US"
        self.manifest_path.write_text(json.dumps(data), encoding="utf-8")
        with self.assertRaisesRegex(bench.BenchError, "Japanese"):
            bench.validate(self.args)

    def test_import_and_fake_tests_do_not_require_win32_runtime(self) -> None:
        with patch.object(sys, "platform", "linux"), self.assertRaisesRegex(bench.BenchError, "Windows"):
            bench.WinAPI()

    def test_windows_10_is_rejected_before_loading_any_native_api(self) -> None:
        with patch.object(sys, "platform", "win32"), patch.object(sys, "getwindowsversion", return_value=Mock(build=19045), create=True):
            with self.assertRaisesRegex(bench.BenchError, "Windows 11"):
                bench.WinAPI()


class WindowAndClipboardTests(unittest.TestCase):
    def test_capture_and_waiter_clock_share_lock_and_poll_detects_same_length_rewrites(self) -> None:
        api = Mock()
        window = bench.TargetWindow(api)
        window.hwnd, window.edit = 1, 2
        window.focus_process = lambda: None
        text = "first"
        clock = FakeClock()

        def send(hwnd, message, wparam, lparam):  # noqa: ANN001, ANN202
            self.assertTrue(window.lock.locked())
            if message == 0xE:
                return len(text)
            self.assertEqual(message, 0xD)
            buffer = ctypes.create_unicode_buffer(text)
            ctypes.memmove(lparam, ctypes.addressof(buffer), ctypes.sizeof(buffer))
            return len(text)

        def now() -> float:
            self.assertTrue(window.lock.locked())
            return clock.now

        api.user.SendMessageW.side_effect = send
        window.message = lambda message: window.capture("poll")
        atomic_clock = Mock(perf_counter=now)
        with patch.object(bench.time, "perf_counter", now):
            window.capture("en_change")
            clock.sleep(0.01)
            observed_now, events = window.snapshot(atomic_clock)
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0].detected_by, "en_change")
            text = "other"  # Same UTF-16 length, changed without EN_CHANGE.
            clock.sleep(0.01)
            observed_now, events = window.snapshot(atomic_clock)
        self.assertEqual(events[-1].text, "other")
        self.assertEqual(events[-1].detected_by, "poll")
        self.assertEqual(observed_now, events[-1].at)
        self.assertEqual(api.user.SendMessageW.call_count, 6)

    def test_cross_thread_calls_timeout_and_missing_hwnd_is_not_own_focus(self) -> None:
        api = Mock()
        window = bench.TargetWindow(api)
        window.thread = Mock()
        window.hwnd, window.edit = 1, 2
        api.user.SendMessageTimeoutW.return_value = 1
        self.assertEqual(window.message(0x8001), 0)
        call = api.user.SendMessageTimeoutW.call_args.args
        self.assertEqual(call[4:6], (0x23, 250))
        api.user.SendMessageTimeoutW.return_value = 0
        with self.assertRaisesRegex(bench.AbortRun, "stopped responding"):
            window.snapshot()
        window.hwnd = None
        api.user.GetForegroundWindow.return_value = None
        self.assertEqual(window.focus_process(), "unknown")
        with self.assertRaises(bench.FocusLost):
            window.message(0x8001)
        window.thread.is_alive.return_value = False
        with self.assertRaisesRegex(bench.AbortRun, "closed"):
            window.snapshot()

    def test_refocus_bounds_each_cross_thread_call_and_gives_up_by_500_ms(self) -> None:
        window = bench.TargetWindow(Mock())
        window.thread = Mock()
        window.focus_process = lambda: "other.exe"
        clock = FakeClock()
        timeouts = []

        def message(code: int, *, timeout_ms: int) -> int:
            self.assertEqual(code, 0x8002)
            timeouts.append(timeout_ms)
            clock.sleep(timeout_ms / 1000)
            return 0

        window.message = message
        with patch.object(bench.time, "perf_counter", clock.perf_counter), patch.object(bench.time, "sleep", clock.sleep):
            self.assertFalse(window.refocus(timeout_s=2))
        self.assertLessEqual(clock.now, 0.501)
        self.assertTrue(all(0 < timeout <= 100 for timeout in timeouts))

    def test_sentinel_registers_monitor_exclusion_without_affecting_restoration(self) -> None:
        api = Mock()
        api.user.RegisterClipboardFormatW.return_value = 0xC001
        clipboard = bench.Clipboard(api, 123)
        clipboard._set_data = Mock()
        clipboard.write("sentinel", exclude_from_history=True)
        api.user.RegisterClipboardFormatW.assert_called_once_with("ExcludeClipboardContentFromMonitorProcessing")
        self.assertEqual([call.args[0] for call in clipboard._set_data.call_args_list], [0xC001, 13])
        clipboard._set_data.reset_mock()
        clipboard.write("original")
        clipboard._set_data.assert_called_once_with(13, "original\0".encode("utf-16-le"))

    @unittest.skipUnless(sys.platform == "win32" and ctypes.sizeof(ctypes.c_void_p) == 8, "64-bit Windows ABI only")
    def test_win64_struct_sizes_without_ui_or_input(self) -> None:
        for structure, expected in ((bench.INPUT, 40), (bench.KEYBDINPUT, 24), (bench.MOUSEINPUT, 32),
                                    (bench.WAVEHDR, 48), (bench.WAVEFORMATEX, 18), (bench.WAVEOUTCAPSW, 84),
                                    (bench.LASTINPUTINFO, 8)):
            with self.subTest(structure=structure.__name__):
                self.assertEqual(ctypes.sizeof(structure), expected)


@unittest.skipUnless(sys.platform == "win32" and os.environ.get("ASR_E2E_WIN32_TESTS") == "1",
                     "opt-in Win32 UI test; set ASR_E2E_WIN32_TESTS=1")
class Win32OptInTests(unittest.TestCase):
    def test_multiline_wm_settext_is_detected_and_closed_window_aborts(self) -> None:
        # No SendInput, audio or clipboard access, even when explicitly enabled.
        api = bench.WinAPI()
        with bench.TargetWindow(api) as window:
            text = ctypes.create_unicode_buffer("合成音声")
            result = bench.ULONG_PTR()
            self.assertTrue(api.user.SendMessageTimeoutW(window.edit, 0xC, 0, ctypes.addressof(text),
                                                        0x23, 250, ctypes.byref(result)))
            _, changes = window.snapshot()
            self.assertEqual(changes[-1].text, "合成音声")
            self.assertEqual(changes[-1].detected_by, "poll")
        with self.assertRaises(bench.AbortRun):
            window.snapshot()


class WavePlayerTests(SyntheticFixture):
    def test_unique_prefix_and_missing_or_ambiguous_device(self) -> None:
        def caps(index, pointer, size):  # noqa: ANN001, ANN202
            pointer._obj.szPname = ["Speakers", "CABLE Input (VB-Audio Virtual Ca"][index]
            return 0

        api = Mock()
        api.winmm.waveOutGetNumDevs.return_value = 2
        api.winmm.waveOutGetDevCapsW.side_effect = caps
        player = bench.WavePlayer(api, "cable input")
        self.assertEqual(player.device_id, 1)
        with self.assertRaisesRegex(bench.BenchError, "matched 0"):
            bench.WavePlayer(api, "missing")
        api.winmm.waveOutGetDevCapsW.side_effect = lambda i, p, s: setattr(p._obj, "szPname", "CABLE Input") or 0
        with self.assertRaisesRegex(bench.BenchError, "matched 2"):
            bench.WavePlayer(api, "CABLE")

    def test_waveout_buffers_format_completion_and_cleanup_with_fake_api(self) -> None:
        api = Mock()
        api.waveproc_type = lambda callback: callback
        callback = None

        def opened(handle, device, fmt, procedure, instance, flags):  # noqa: ANN001, ANN202
            nonlocal callback
            callback = procedure
            handle._obj.value = 42
            self.assertEqual((fmt._obj.wFormatTag, fmt._obj.nSamplesPerSec, fmt._obj.wBitsPerSample), (1, 16000, 16))
            self.assertEqual(flags, 0x30000)
            return 0

        def write(handle, header, size):  # noqa: ANN001, ANN202
            self.assertEqual(header._obj.dwBufferLength, 3200)
            self.assertEqual(ctypes.string_at(header._obj.lpData, 3200), b"\0\0" * 1600)
            callback(handle, 0x3BD, 0, 0, 0)
            return 0

        api.winmm.waveOutOpen.side_effect = opened
        api.winmm.waveOutWrite.side_effect = write
        api.winmm.waveOutPrepareHeader.return_value = 0
        player = bench.WavePlayer.__new__(bench.WavePlayer)
        player.api, player.device_id = api, 1
        started, completed, duration = player.play(self.wav)
        self.assertGreaterEqual(completed, started)
        self.assertEqual(duration, 100)
        api.winmm.waveOutReset.assert_called_once()
        api.winmm.waveOutUnprepareHeader.assert_called_once()
        api.winmm.waveOutClose.assert_called_once()
        api.winmm.reset_mock()
        api.winmm.waveOutWrite.side_effect = KeyboardInterrupt
        with self.assertRaises(KeyboardInterrupt):
            player.play(self.wav)
        api.winmm.waveOutReset.assert_called_once()
        api.winmm.waveOutUnprepareHeader.assert_called_once()
        api.winmm.waveOutClose.assert_called_once()


if __name__ == "__main__":
    unittest.main()
