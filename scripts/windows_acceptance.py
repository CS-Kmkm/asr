"""Interactive Windows acceptance runner; launch locally with uv run.

Uses only enrolled scratch controls and the product's actual hotkeys/pipeline.
No Computer Use, global key hooks, clipboard observer, or permission bypass.
"""
from __future__ import annotations

import argparse
import contextlib
import ctypes
from ctypes import wintypes as wt
import hashlib
import json
from pathlib import Path
import queue
import re
import sqlite3
import subprocess
import sys
import threading
import time
import wave

from bench_e2e import (WinAPI, TargetWindow, WavePlayer, Keyboard, ConsoleKeyGuard,
                       KEYBDINPUT, INPUT, load_manifest, parse_hotkey, BenchError)
from evaluation_events import EventReader, EvidenceError
from score_asr import cer_stats

SCENARIOS = ('dictate', 'focus-away', 'selection-change', 'minimize', 'cancel')
OPTIONAL_SCENARIOS = ('ime-composition', 'cancel-processing')
HOLD_SCENARIOS = ('dictate', 'focus-away', 'selection-change', 'cancel')
SEED = 'ASR acceptance sentinel'


class UnsupportedTarget(EvidenceError):
    pass


class ScenarioEvidenceError(EvidenceError):
    """A fixed diagnostic code that can be saved without receiver text."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def profile_settings(path, device, recording_mode='toggle'):
    if recording_mode not in ('toggle', 'hold'):
        raise EvidenceError('unknown recording mode')
    with contextlib.closing(sqlite3.connect(path.resolve().as_uri() + '?mode=ro', uri=True, timeout=1)) as db:
        row = db.execute("SELECT value FROM settings WHERE key='app_settings'").fetchone()
    try:
        settings = json.loads(row[0]) if row else {}
    except (json.JSONDecodeError, TypeError) as exc:
        raise EvidenceError('invalid profile JSON') from exc
    if not isinstance(settings, dict) or not isinstance(settings.get('holdToTalk'), dict) or not isinstance(settings.get('shortcuts'), dict):
        raise EvidenceError('invalid profile shape')
    if (settings.get('asrBackend') not in ('speech-cpp', 'faster-whisper', 'sherpa-onnx', 'transformers-asr')
            or any(settings.get(key) is not False for key in (
                'textCorrectionEnabled', 'automaticVoiceMode', 'liveTargetInsertion'))
            or settings.get('holdToTalk', {}).get('dictate') is not (recording_mode == 'hold')
            or settings.get('microphoneId') != device):
        raise EvidenceError('use a prepared local profile with matching recording mode, correction/live insertion/automatic mode OFF, and the selected virtual microphone')
    shortcuts = settings.get('shortcuts', {}).get('dictate', [])
    if not isinstance(shortcuts, list) or len(shortcuts) != 1 or not isinstance(shortcuts[0], str):
        raise EvidenceError('configure one Dictate shortcut for this test profile')
    parse_hotkey(shortcuts[0])
    return {'hotkey': shortcuts[0], 'holdToTalkDictate': settings['holdToTalk']['dictate'], **{key: settings.get(key) for key in (
        'asrBackend', 'modelId', 'modelQuantization', 'asrDevice', 'speechLocale',
        'statefulStreaming', 'qwenDemoStreaming', 'incrementalFinalAsr',
        'livePreviewEnabled', 'smartDictateEdit', 'clipboardRestore')}}


def runtime_metadata(api, app_pid):
    source = {'revision': None, 'dirty': None}
    root = Path(__file__).resolve().parents[1]
    try:
        head = subprocess.run(['git', 'rev-parse', 'HEAD'], cwd=root, capture_output=True,
                              text=True, timeout=3, check=True).stdout.strip()
        dirty = subprocess.run(['git', 'status', '--porcelain'], cwd=root, capture_output=True,
                               timeout=3, check=True).stdout
        if re.fullmatch('[0-9a-f]{40,64}', head):
            source = {'revision': head, 'dirty': bool(dirty)}
    except (OSError, subprocess.SubprocessError):
        pass
    handle = api.kernel.OpenProcess(0x1000, False, app_pid)
    digest = None
    if handle:
        try:
            buffer = ctypes.create_unicode_buffer(32768)
            length = wt.DWORD(len(buffer))
            if api.kernel.QueryFullProcessImageNameW(handle, 0, buffer, ctypes.byref(length)):
                hasher = hashlib.sha256()
                with Path(buffer.value).open('rb') as binary:
                    for chunk in iter(lambda: binary.read(1024 * 1024), b''):
                        hasher.update(chunk)
                digest = hasher.hexdigest()
        except OSError:
            pass
        finally:
            api.kernel.CloseHandle(handle)
    return {'runner_source': source,
            'runner_script_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            'runner_files_sha256': {name: hashlib.sha256((Path(__file__).parent / name).read_bytes()).hexdigest()
                                    for name in ('windows_acceptance.py', 'bench_e2e.py', 'evaluation_events.py',
                                                 'score_asr.py', 'windows_uia_bridge.ps1')},
            'os_build': sys.getwindowsversion().build,
            'application_binary_sha256': digest,
            'application_identity_scope': 'loaded executable file; debug frontend is separate'}


class UIABridge:
    def __init__(self):
        self.process = subprocess.Popen(
            ['pwsh.exe', '-NoProfile', '-NonInteractive', '-File',
             str(Path(__file__).with_name('windows_uia_bridge.ps1'))],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, encoding='utf-8', creationflags=subprocess.CREATE_NO_WINDOW)
        self.responses = queue.Queue()
        def read():
            for line in self.process.stdout:
                self.responses.put(line)
            self.responses.put(None)
        threading.Thread(target=read, daemon=True).start()

    def call(self, action, target):
        try:
            self.process.stdin.write(json.dumps({'action': action, 'target': target}) + '\n')
            self.process.stdin.flush()
            line = self.responses.get(timeout=.75 if action == 'focused' else 8)
            result = json.loads(line) if line else {}
            if result.get('ok') is not True:
                raise UnsupportedTarget('UI Automation target unsupported, blocked, or changed')
            return result
        except (OSError, ValueError, queue.Empty) as exc:
            self.close()
            raise UnsupportedTarget('UI Automation unavailable; no fallback input was sent') from exc

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=3)
        for stream in (self.process.stdin, self.process.stdout):
            if stream:
                stream.close()


class ExternalTarget:
    def __init__(self, bridge, name, root=None):
        self.bridge, self.name, self.root = bridge, name, root

    def focus(self):
        if not self.bridge.call('focus', self.name)['focused']:
            raise UnsupportedTarget('Windows refused target focus')

    def focus_process(self):
        try:
            return None if self.bridge.call('focused', self.name)['focused'] else 'unenrolled_focus'
        except UnsupportedTarget:
            # Keyboard cleanup expects a value, never an exception before key-up.
            return 'unavailable'

    def refocus(self, **unused):
        return False  # Cleanup must not steal focus or retry a toggle.

    def read(self):
        return self.bridge.call('read', self.name)['text']

    def select_all(self):
        self.bridge.call('select_all', self.name)

    def minimize(self):
        self.bridge.call('minimize', self.name)


class NativeTarget:
    def __init__(self, api, window):
        self.api, self.window = api, window
        self.root = window.hwnd
        api.bind(api.user, 'GetFocus', wt.HWND)

    def focus(self):
        if not self.window.refocus(timeout_s=.5):
            raise UnsupportedTarget('Windows refused native target focus; start from an interactive terminal')
        self.window.call_on_ui(lambda: self.api.user.SetFocus(self.window.edit))
        require_focus(self)

    def focus_process(self):
        process = self.window.focus_process()
        if process is not None:
            return process
        try:
            focused = self.window.call_on_ui(self.api.user.GetFocus)
            return None if focused == self.window.edit else 'other_control'
        except BenchError:
            return 'unavailable'

    def refocus(self, **unused):
        return False

    def read(self):
        _, changes = self.window.snapshot()
        return changes[-1].text if changes else ''

    def select_all(self):
        result = ctypes.c_size_t()
        if not self.api.user.SendMessageTimeoutW(self.window.edit, 0xB1, 0, -1, 0x23, 250, ctypes.byref(result)):
            raise UnsupportedTarget('native selection timed out')

    def change_held_selection(self):
        # Window messages avoid sending an arrow with the Dictate modifiers down.
        # The acceptance sentinel is short enough for EM_GETSEL's packed result.
        require_focus(self)
        def selection():
            result = ctypes.c_size_t()
            if not self.api.user.SendMessageTimeoutW(self.window.edit, 0xB0, 0, 0, 0x23, 250, ctypes.byref(result)):
                raise UnsupportedTarget('native selection read timed out')
            return result.value
        before = selection()
        self.select_all()
        require_focus(self)
        after = selection()
        if before == after or after != len(SEED) << 16:
            raise UnsupportedTarget('could not verify selection changed to the full sentinel')

    def minimize(self):
        self.api.user.ShowWindow(self.window.hwnd, 7)


class NativeIME:
    """Capability-gated IMM composition test on our owned native EDIT only."""
    def __init__(self, api, target):
        if not isinstance(target, NativeTarget):
            raise UnsupportedTarget('external TSF composition needs separate verification')
        self.api, self.target = api, target
        self.imm = ctypes.WinDLL('imm32', use_last_error=True)
        api.bind(self.imm, 'ImmGetContext', wt.HANDLE, wt.HWND)
        api.bind(self.imm, 'ImmReleaseContext', wt.BOOL, wt.HWND, wt.HANDLE)
        api.bind(self.imm, 'ImmGetOpenStatus', wt.BOOL, wt.HANDLE)
        api.bind(self.imm, 'ImmSetOpenStatus', wt.BOOL, wt.HANDLE, wt.BOOL)
        api.bind(self.imm, 'ImmGetCompositionStringW', ctypes.c_long, wt.HANDLE, wt.DWORD, ctypes.c_void_p, wt.DWORD)
        self.context = self.call(lambda: self.imm.ImmGetContext(target.window.edit))
        if not self.context:
            raise UnsupportedTarget('native IME context unavailable')
        self.was_open = bool(self.call(lambda: self.imm.ImmGetOpenStatus(self.context)))

    def call(self, callback):
        return self.target.window.call_on_ui(callback)

    def active(self):
        return self.call(lambda: self.imm.ImmGetCompositionStringW(self.context, 8, None, 0)) > 0

    def wait_active(self, expected, timeout=.5):
        deadline = time.perf_counter() + timeout
        while time.perf_counter() < deadline:
            if self.active() is expected:
                return True
            time.sleep(.02)  # Let the UI thread dispatch queued keyboard input.
        return False

    def compose(self):
        self.call(lambda: self.imm.ImmSetOpenStatus(self.context, True))
        for character in 'nihongo':
            tap(self.api, self.target, (ord(character.upper()),))
        tap(self.api, self.target, (0x20,))
        if not self.wait_active(True):
            raise UnsupportedTarget('active Japanese IME composition could not be verified')

    def cancel_composition(self):
        for _ in range(3):
            if not self.active():
                return
            require_focus(self.target)
            tap(self.api, self.target, (0x1B,))
            if self.wait_active(False):
                return
        if self.active():
            raise UnsupportedTarget('IME composition did not cancel')

    def close(self):
        if not self.context:
            return
        try:
            self.cancel_composition()
        finally:
            def restore():
                try:
                    if not self.imm.ImmSetOpenStatus(self.context, self.was_open):
                        raise UnsupportedTarget('could not restore IME open status')
                finally:
                    self.imm.ImmReleaseContext(self.target.window.edit, self.context)
            try:
                self.call(restore)
            finally:
                self.context = None


def tap(api, target, chord):
    if any(api.key_down(key) for key in (0x10, 0x11, 0x12, 0x5B, 0x5C)):
        raise EvidenceError('a physical modifier is down; no new keys were sent')
    keys = Keyboard(api.send, target, chord, key_down=api.key_down, idle_seconds=api.idle_seconds)
    with ConsoleKeyGuard(api, keys, 'toggle'):
        try:
            return keys.tap()
        finally:
            keys.cleanup('toggle')


def unicode_text(api, target, text):
    # KEYEVENTF_UNICODE produces actual text input, not IME composition.
    if target.focus_process() is not None:
        raise UnsupportedTarget('focus mismatch before text input')
    if not text:
        return
    encoded = text.encode('utf-16-le')
    inputs = (INPUT * len(encoded))()
    for index in range(0, len(encoded), 2):
        code = int.from_bytes(encoded[index:index + 2], 'little')
        for offset, flags in ((0, 4), (1, 6)):
            item = inputs[index + offset]
            item.type = 1
            item.ki = KEYBDINPUT(0, code, flags, 0, 0)
    if api.user.SendInput(len(inputs), inputs, ctypes.sizeof(INPUT)) != len(inputs):
        releases = (INPUT * (len(encoded) // 2))()
        for index, item in enumerate(releases):
            item.type = 1
            item.ki = KEYBDINPUT(0, int.from_bytes(encoded[index*2:index*2+2], 'little'), 6, 0, 0)
        api.user.SendInput(len(releases), releases, ctypes.sizeof(INPUT))
        raise EvidenceError('Unicode input failed or was partial; stop the run')


def prepare(api, target, previous, text):
    target.focus()
    if target.read() != previous:
        raise EvidenceError('scratch contents changed; refusing to replace them')
    target.select_all()
    tap(api, target, (0x08,))
    unicode_text(api, target, text)
    deadline = time.perf_counter() + 3
    while time.perf_counter() < deadline:
        if target.read() == text:
            return
        time.sleep(.05)
    raise UnsupportedTarget('scratch control did not accept deterministic setup')


def judge(scenario, primary, secondary, take, reference, max_cer):
    if scenario == 'dictate':
        cer = cer_stats(reference, primary)['cer']
        passed = bool(primary.strip()) and not secondary and take['outcome'] == 'pasted' and cer <= max_cer
        return {'status': 'PASS' if passed else 'FAIL', 'normalized_cer': cer}
    expected = 'cancelled' if scenario in ('cancel', 'cancel-processing') else 'clipboard_only'
    passed = primary == SEED and not secondary and take['outcome'] == expected
    return {'status': 'PASS' if passed else 'FAIL', 'normalized_cer': None}


def observed_playback(player, path, reader, target, check=lambda: None, *, abort_on_failure=False):
    with wave.open(str(path), 'rb') as source:
        deadline = time.perf_counter() + source.getnframes() / source.getframerate() + 15
    completed = queue.Queue()
    def play():
        try:
            completed.put((player.play(path), None))
        except Exception:
            completed.put((None, EvidenceError('audio playback failed')))
    thread = threading.Thread(target=play, daemon=True)
    thread.start()
    failure = None
    while True:
        if time.perf_counter() > deadline:
            raise EvidenceError('playback worker timed out; stop/verify the app manually')
        try:
            reader.poll()
            require_focus(target)
            check()
        except EvidenceError as exc:
            if abort_on_failure:
                raise
            failure = failure or exc
        try:
            timing, error = completed.get(timeout=.02)
            thread.join(timeout=1)
            if failure or error:
                raise failure or error
            return timing
        except queue.Empty:
            continue


def run_scenario(scenario, *, api, primary, secondary, reader, player, item, hotkey,
                 max_cer=.5, timeout=60, guard_seconds=2, clock=time, ime=None,
                 recording_mode='toggle'):
    arguments = dict(api=api, primary=primary, secondary=secondary, reader=reader,
                     player=player, item=item, hotkey=hotkey, max_cer=max_cer,
                     timeout=timeout, guard_seconds=guard_seconds, clock=clock, ime=ime)
    if recording_mode == 'toggle':
        return _run_scenario(scenario, **arguments)
    if recording_mode != 'hold' or scenario not in HOLD_SCENARIOS or ime:
        raise EvidenceError('unsupported hold scenario')
    if scenario == 'selection-change' and not isinstance(primary, NativeTarget):
        raise UnsupportedTarget('hold selection-change requires the owned native EDIT')
    keys = Keyboard(api.send, primary, hotkey, clock, key_down=api.key_down,
                    idle_seconds=api.idle_seconds)
    held_since = None
    def hold_start():
        nonlocal held_since
        if any(api.key_down(key) for key in (0x10, 0x11, 0x12, 0x5B, 0x5C)):
            raise EvidenceError('a physical modifier is down; no new keys were sent')
        result = keys.down()
        held_since = clock.perf_counter()
        return result
    def hold_check():
        if held_since is not None and keys.pressed:
            elapsed = clock.perf_counter() - held_since
            if elapsed > .2 and api.idle_seconds() < elapsed - .15:
                raise ScenarioEvidenceError('input_activity_during_hold',
                                            'input detected during hold; releasing injected keys')
    def still_held():
        if keys.shutdown.is_set() or not keys.pressed or not all(api.key_down(key) for key in hotkey):
            raise ScenarioEvidenceError('hold_chord_lost', 'hold chord was lost before the intended action')
    def release_at(target):
        # Verify the intended receiver and use the normal focused release path.
        keys.window = target
        return keys.release()
    with ConsoleKeyGuard(api, keys, 'hold'):
        try:
            return _run_scenario(scenario, start=hold_start, stop=release_at,
                                 recording_timeout=min(timeout, 3), hold_check=hold_check,
                                 still_held=still_held, **arguments)
        except BaseException as original:
            released = bool(keys.pressed)
            release_failed = False
            try:
                keys.release(emergency=True)
            except Exception:
                release_failed = True
            error = EvidenceError('hold aborted; release may stop recording and deliver output')
            error.hold_abort = {'hold_released_on_abort': released and not release_failed,
                                'hold_release_failed': release_failed,
                                'original_exception': type(original).__name__}
            if isinstance(original, ScenarioEvidenceError):
                error.hold_abort['hold_abort_cause'] = original.code
            raise error from original


def _run_scenario(scenario, *, api, primary, secondary, reader, player, item, hotkey,
                  max_cer=.5, timeout=60, guard_seconds=2, clock=time, ime=None,
                  start=None, stop=None, recording_timeout=None, hold_check=lambda: None,
                  still_held=lambda: None):
    reader.ready()
    if reader.phase != 'idle':
        raise EvidenceError('app not idle; never retry its toggle blindly')
    baseline = reader.sequence
    primary.focus()
    original = primary.read()
    if original != ('' if scenario == 'dictate' else SEED) or secondary.read():
        raise EvidenceError('unexpected scratch state before recording')
    if ime:
        ime.compose()
        ime.cancel_composition()
        if primary.read() != original:
            raise UnsupportedTarget('IME preflight changed the scratch text; recording was not started')
    if start is None:
        tap(api, primary, hotkey)
    else:
        start()
    def recording_check():
        require_focus(primary)
        hold_check()
    reader.wait(lambda r: r.recording_after(baseline) is not None,
                timeout if recording_timeout is None else recording_timeout, check=recording_check)
    operation = reader.operation_id
    # This is the real recording path, not a WAV-import shortcut.
    def before_stop_check():
        hold_check()
        if primary.read() != original or secondary.read():
            raise ScenarioEvidenceError('receiver_changed_during_playback',
                                        'scratch text changed before stop; no further input sent')
    playback_options = {'abort_on_failure': True} if start is not None else {}
    started, ended, duration = observed_playback(player, Path(item['audio_path']), reader, primary,
                                                before_stop_check, **playback_options)
    def require_recording():
        reader.poll()
        if reader.phase != 'recording' or reader.operation_id != operation:
            raise ScenarioEvidenceError('recording_ended_before_action',
                                        'recording ended before the intended stop or cancel')
    require_recording()
    require_focus(primary)
    hold_check()
    receiver_violation = False
    secondary_violation = False
    def monitor_receiver():
        nonlocal receiver_violation, secondary_violation
        secondary_violation |= bool(secondary.read())
        if secondary_violation or (scenario != 'dictate' and not ime and primary.read() != original):
            receiver_violation = True
    if ime:
        ime.compose()
    if scenario == 'selection-change':
        if stop is None:
            tap(api, primary, (0x10, 0x25))  # Shift+Left, observed by product input monitor.
        else:
            primary.change_held_selection()
    target = primary
    if scenario in ('focus-away', 'minimize'):
        secondary.focus()
        target = secondary
    if scenario == 'minimize':
        primary.minimize()
    processing_cancel_attempted = False
    if scenario == 'cancel':
        if stop is None:
            tap(api, target, (0x1B,))
        else:
            still_held()
            require_recording()
            print('今から8秒以内に録音オーバーレイのキャンセルをクリックしてください。キーや他のウィンドウには触れないでください。', flush=True)
            def check_hold_cancel():
                require_focus(primary)
                still_held()
                monitor_receiver()
            reader.wait(lambda r: r.completed(operation), min(timeout, 8), check=check_hold_cancel)
            stop(primary)
    else:
        if stop is None:
            tap(api, target, hotkey)
        else:
            hold_check()
            still_held()
            require_recording()
            stop(target)
        if scenario == 'cancel-processing':
            reader.wait(lambda r: (r.phase == 'processing' and r.operation_id == operation)
                        or r.completed(operation), timeout,
                        check=lambda: require_focus(primary))
            if reader.phase == 'processing' and reader.operation_id == operation:
                tap(api, primary, (0x1B,))
                processing_cancel_attempted = True
    def check_delivery():
        monitor_receiver()
        if stop is not None and scenario != 'dictate':
            require_focus(target)
    reader.wait(lambda r: r.completed(operation), timeout, check=check_delivery)
    # Look for delayed insertion after completion, including cancellation.
    deadline = clock.perf_counter() + guard_seconds
    while clock.perf_counter() < deadline:
        reader.poll()
        if reader.phase != 'idle':
            raise EvidenceError('another operation started during the late-output guard')
        check_delivery()
        clock.sleep(.05)
    take = reader.takes[operation]
    if ime:
        if not ime.active():
            raise EvidenceError('IME composition disappeared during the guard')
        ime.close()
    first, second = primary.read(), secondary.read()
    if stop is not None and scenario != 'dictate':
        require_focus(target)
    result = judge(scenario, first, second, take, item['reference'], max_cer)
    if receiver_violation:
        result['status'] = 'FAIL'
    if scenario != 'dictate':
        result['receiver_unchanged'] = first == original and not second and not receiver_violation
    if scenario == 'cancel' and stop is not None:
        result['requested_cancel_method'] = 'operator_overlay'
        result['cancel_source_verified'] = False
        if (result['receiver_unchanged'] and any(
                event['operation_id'] == operation and event['phase'] == 'processing'
                for event in reader.events)):
            result.update(status='BLOCKED', reason='processing_observed_before_cancel_completion')
    if scenario == 'cancel-processing':
        result['processing_cancel_attempted'] = processing_cancel_attempted
        if secondary_violation or second:
            result.update(status='FAIL', reason='secondary_receiver_changed')
        elif not processing_cancel_attempted and (not receiver_violation or (
                take['outcome'] == 'pasted' and first.startswith(original) and len(first) > len(original))):
            result.update(status='BLOCKED', reason='processing_window_missed')
        elif take['outcome'] == 'pasted' and first.startswith(original) and len(first) > len(original):
            result.update(status='BLOCKED', reason='processing_cancel_commit_race_unconfirmed')
        elif take['outcome'] != 'cancelled':
            result.update(status='FAIL', reason='processing_cancel_not_honored')
    result.update(scenario=scenario, operation_id=operation,
                  app_outcome=take['outcome'], app_spans_ms=take['spans_ms'],
                  audio_duration_ms=duration, playback_wall_ms=(ended-started)*1000,
                  late_output_guard_seconds=guard_seconds)
    if ime:
        result['verification_scope'] = 'IME typing while recording; active composition verified, isolated IME guard not established'
    return result, first, second


def require_focus(target):
    if target.focus_process() is not None:
        raise UnsupportedTarget('focus left the enrolled control')


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--item-id', required=True)
    parser.add_argument('--events', type=Path, required=True)
    parser.add_argument('--profile-db', type=Path, required=True)
    parser.add_argument('--output-dir', type=Path, required=True)
    parser.add_argument('--target', choices=('native', 'external'), default='native')
    parser.add_argument('--playback-device', default='CABLE Input')
    parser.add_argument('--microphone-id', default='name:CABLE Output (VB-Audio Virtual Cable)')
    parser.add_argument('--scenario', action='append', choices=SCENARIOS + OPTIONAL_SCENARIOS)
    parser.add_argument('--recording-mode', choices=('toggle', 'hold'), default='toggle')
    parser.add_argument('--max-cer', type=float, default=.5)
    args = parser.parse_args(argv)
    if not 0 <= args.max_cer <= 1:
        parser.error('--max-cer must be between 0 and 1')
    scenarios = args.scenario or (('dictate',) if args.recording_mode == 'hold' else SCENARIOS)
    if args.recording_mode == 'hold' and any(scenario not in HOLD_SCENARIOS for scenario in scenarios):
        parser.error('--recording-mode hold supports dictate, focus-away, selection-change and cancel')
    if args.recording_mode == 'hold' and args.target != 'native' and 'selection-change' in scenarios:
        parser.error('hold selection-change requires --target native')
    manifest, _ = load_manifest(args.manifest)
    item = next((item for item in manifest['items'] if item['id'] == args.item_id), None)
    if not item or not item['reference'].strip():
        parser.error('choose a nonempty-reference item from the manifest')
    if hashlib.sha256(Path(item['audio_path']).read_bytes()).hexdigest() != item['audio_sha256']:
        parser.error('audio hash mismatch')
    profile = profile_settings(args.profile_db, args.microphone_id, args.recording_mode)
    hotkey = parse_hotkey(profile['hotkey'])
    args.output_dir.mkdir(parents=True, exist_ok=False)
    report = {'schema': 'asr-windows-acceptance/1', 'target': args.target,
              'item_id': item['id'], 'audio_sha256': item['audio_sha256'],
              'max_cer': args.max_cer, 'recording_mode': args.recording_mode,
              'requested_scenarios': list(scenarios),
              'profile_projection_schema': 'asr-acceptance-profile/2',
              'profile_sha256': hashlib.sha256(json.dumps(profile, sort_keys=True).encode()).hexdigest(),
              'results': [],
              'manual_remaining': ['IME active composition', 'terminal controls',
                                   'clipboard full-format restoration/conflict', 'hold-to-talk',
                                   'Smart selected-text edit', 'cancel during processing',
                                   'physical microphone/device changes', 'UI layout and permission dialogs']}
    api, reader = WinAPI(), EventReader(args.events)
    player = WavePlayer(api, args.playback_device)
    reader.ready()
    report['runtime'] = runtime_metadata(api, reader.pid)
    print('評価中は専用の空欄だけを使います。アプリのモデル・録音権限を確認し、停止してから開始してください。')
    input('準備できたら Enter（終了は Ctrl+C）: ')
    active_scenario = 'setup'
    try:
        with contextlib.ExitStack() as stack:
            if args.target == 'native':
                report['applications'] = [{'process': 'owned_native_edit', 'version': None}]
                windows = [stack.enter_context(TargetWindow(api, foreground_mode='plain')) for _ in range(2)]
                primary, secondary = [NativeTarget(api, window) for window in windows]
                print('5秒以内に最初に開いた評価用ウィンドウをクリックしてください。')
                time.sleep(5)
            else:
                bridge = UIABridge()
                stack.callback(bridge.close)
                captures = {}
                for name in ('primary', 'secondary'):
                    input(f'{name}: メモ帳・ブラウザ・VS Codeに空の専用欄を用意して Enter: ')
                    print('5秒以内にその入力欄をクリックしてください。')
                    time.sleep(5)
                    captures[name] = bridge.call('capture', name)
                    report.setdefault('applications', []).append({
                        'slot': name, 'process': captures[name]['process'],
                        'version': captures[name].get('version')})
                primary, secondary = [ExternalTarget(bridge, name, captures[name]['window']) for name in ('primary', 'secondary')]
            previous = ['', '']
            for scenario in scenarios:
                active_scenario = scenario
                if scenario == 'minimize' and primary.root == secondary.root:
                    raise UnsupportedTarget('minimize requires two separate scratch windows')
                if profile_settings(args.profile_db, args.microphone_id, args.recording_mode) != profile:
                    raise EvidenceError('profile changed')
                reader.ready()
                if reader.phase != 'idle':
                    raise EvidenceError('app not idle before scratch preparation')
                Keyboard(api.send, primary, hotkey, key_down=api.key_down,
                         idle_seconds=api.idle_seconds).before_trial()
                prepare(api, secondary, previous[1], '')
                prepare(api, primary, previous[0], '' if scenario == 'dictate' else SEED)
                print(f'{scenario}: 開始', flush=True)
                with contextlib.ExitStack() as scenario_cleanup:
                    ime = NativeIME(api, primary) if scenario == 'ime-composition' else None
                    if ime:
                        scenario_cleanup.callback(ime.close)
                    result, previous[0], previous[1] = run_scenario(
                        scenario, api=api, primary=primary, secondary=secondary, reader=reader,
                        player=player, item=item, hotkey=hotkey, max_cer=args.max_cer, ime=ime,
                        recording_mode=args.recording_mode)
                if profile_settings(args.profile_db, args.microphone_id, args.recording_mode) != profile:
                    raise EvidenceError('profile changed during trial')
                report['results'].append(result)
                if result['status'] == 'PASS' and scenario == 'cancel-processing' and 'cancel during processing' in report['manual_remaining']:
                    report['manual_remaining'][report['manual_remaining'].index('cancel during processing')] = 'processing cancel with correction/Translate/Edit/Ask'
                print(f"{scenario}: {result['status']}")
                if result['status'] != 'PASS':
                    break  # Do not erase a failing result or continue from uncertain state.
    except (EvidenceError, BenchError, OSError, sqlite3.Error, KeyboardInterrupt) as exc:
        reason = ('interrupted' if isinstance(exc, KeyboardInterrupt) else
                  'uia_focus_or_ime_unavailable' if isinstance(exc, UnsupportedTarget) else
                  'os_input_audio_or_process_unavailable' if isinstance(exc, (BenchError, OSError)) else
                  'profile_database_unavailable' if isinstance(exc, sqlite3.Error) else
                  'invalid_evidence_or_profile')
        report['results'].append({'scenario': active_scenario, 'status': 'BLOCKED', 'reason': reason})
        if hasattr(exc, 'hold_abort'):
            report['results'][-1].update(exc.hold_abort)
            if 'hold_abort_cause' in exc.hold_abort:
                print(f"Hold中断理由: {exc.hold_abort['hold_abort_cause']}", flush=True)
            if exc.hold_abort['hold_release_failed']:
                print('警告: 注入キーの解放を確認できません。キーが押された状態で残っている可能性があります。')
            print('Holdの解放は録音停止として処理され、出力やクリップボード書き込みが続く場合があります。アプリの完了状態と入力先を確認してください。')
        print('中断しました。アプリの録音状態を確認し、必要なら手動で停止してください。')
    finally:
        if args.recording_mode == 'hold':
            passed = {row['scenario'] for row in report['results'] if row['status'] == 'PASS'}
            report['manual_remaining'].remove('hold-to-talk')
            report['manual_remaining'].extend(f'hold-to-talk {scenario}' for scenario in HOLD_SCENARIOS if scenario not in passed)
            report['manual_remaining'].extend(['hold-to-talk minimize', 'hold-to-talk cancel-processing'])
        with (args.output_dir / 'acceptance.json').open('x', encoding='utf-8') as output:
            json.dump(report, output, ensure_ascii=False, indent=2, allow_nan=False)
    return 0 if len(report['results']) == len(scenarios) and all(
        row['status'] == 'PASS' for row in report['results']) else 1


if __name__ == '__main__':
    raise SystemExit(main())
