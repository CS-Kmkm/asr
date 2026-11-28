"""Interactive Windows acceptance runner; launch locally with uv run.

Uses only enrolled scratch controls and the product's actual hotkeys/pipeline.
No Computer Use, global key hooks, clipboard observer, or permission bypass.
"""
from __future__ import annotations

import argparse
import base64
import contextlib
import ctypes
from ctypes import wintypes as wt
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import socket
import sqlite3
import struct
import subprocess
import sys
import threading
import time
import uuid
import wave

from bench_e2e import (WinAPI, TargetWindow, WavePlayer, Keyboard, ConsoleKeyGuard, Clipboard,
                       KEYBDINPUT, INPUT, load_manifest, parse_hotkey, BenchError)
from evaluation_events import EventReader, EvidenceError
from score_asr import cer_stats

SCENARIOS = ('dictate', 'focus-away', 'selection-change', 'minimize', 'cancel')
OPTIONAL_SCENARIOS = ('ime-composition', 'cancel-processing')
HOLD_SCENARIOS = ('dictate', 'focus-away', 'selection-change', 'cancel')
SEED = 'ASR acceptance sentinel'
MOCK_INSERTION = 'mock-insertion'
MOCK_NOTEPAD = 'mock-notepad'
REAL_NOTEPAD = 'real-notepad'
NATIVE_NOTEPAD = 'native-notepad'
NATIVE_PROFILE = 'org.localvoice.input.eval-native'
NATIVE_MODEL = 'qwen3-asr-0.6b'
NATIVE_MODELS = {
    NATIVE_MODEL: (NATIVE_PROFILE, 'Qwen3-ASR-0.6B-Q8_0.gguf'),
    'haas-ja-v1.0': ('org.localvoice.input.eval-haas', 'Haas-616M-ja-v1.0-Q8_0.gguf'),
}
REAL_EXTERNAL = {'real-vscode': 'code', 'real-chrome': 'chrome'}
REAL_EXTERNAL_REPEATS = 2
REAL_RECEIVERS = {REAL_NOTEPAD: 'notepad', **REAL_EXTERNAL}
MOCK_GUARDS = ('focus-away', 'cancel')
VM_SMOKE_SCENARIOS = (MOCK_INSERTION, *MOCK_GUARDS)
VM_EXTRA_GUARDS = ('selection-change', 'minimize')
WORKER_CRASH = 'worker-crash'
WORKER_RECOVERY = 'worker-crash-recovery'
MOCK_EXTERNAL = {'mock-notepad': 'notepad', 'mock-edge': 'edge', 'mock-vscode': 'code', 'mock-chrome': 'chrome',
                 'mock-terminal': 'terminal'}
VM_CLASSIC_TERMINALS = {'mock-mintty': 'mintty', 'mock-putty': 'putty'}
MOCK_RECEIVERS = (MOCK_INSERTION, *MOCK_EXTERNAL)
NORMAL_MATRIX_TARGETS = (MOCK_INSERTION, MOCK_NOTEPAD, 'mock-chrome', 'mock-vscode', 'mock-terminal')
NORMAL_MATRIX_CONDITIONS = ('short', 'mixed', 'multiline', 'long')
NORMAL_MATRIX_PREFIX = '既存文。'
VM_FULL_SCENARIOS = (*VM_SMOKE_SCENARIOS, *VM_EXTRA_GUARDS, *MOCK_EXTERNAL, WORKER_RECOVERY)
CLIPBOARD_GUARDS = ('clipboard-rich', 'clipboard-newer-during-asr')
VM_INFLIGHT_GUARDS = ('ime-composition', 'cancel-processing', *CLIPBOARD_GUARDS)
HELD_WORKER_GUARDS = ('cancel-processing', 'clipboard-newer-during-asr')
VM_MOCK_CASES = (*MOCK_GUARDS, *VM_EXTRA_GUARDS, *MOCK_EXTERNAL, *VM_CLASSIC_TERMINALS,
                 *VM_INFLIGHT_GUARDS)
WORKER_RECOVERY_TRIALS = (WORKER_CRASH, MOCK_INSERTION)
DELIVERY_OUTCOMES = ('pasted', 'paste_unverified')
MOCK_PREFIX = '前の文。'
MOCK_SUFFIX = '後ろの文。'
MOCK_TEXT = '挿入試験です。'


class UnsupportedTarget(EvidenceError):
    pass


class ScenarioEvidenceError(EvidenceError):
    """A fixed diagnostic code that can be saved without receiver text."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def profile_settings(path, device, recording_mode='toggle', *, mock_insertion=False, real_notepad=False,
                     mock_on_real_profile=False, native_model=None):
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
    backends = ('mock',) if mock_insertion and not mock_on_real_profile else (
        'speech-cpp', 'faster-whisper', 'sherpa-onnx', 'transformers-asr')
    if (settings.get('asrBackend') not in backends
            or ((mock_insertion or real_notepad or native_model) and settings.get('offlineOnly') is not True)
            or ((real_notepad or mock_on_real_profile) and (settings.get('asrBackend') != 'faster-whisper'
                                  or settings.get('asrDevice') != 'cpu'
                                  or any(settings.get(key) is not False for key in (
                                      'livePreviewEnabled', 'incrementalFinalAsr', 'statefulStreaming', 'smartDictateEdit'))))
            or (mock_on_real_profile and settings.get('asrEvaluationProfile') != 'vm-real-notepad/1')
            or (native_model and (
                settings.get('asrEvaluationProfile') != 'vm-native-notepad/1'
                or settings.get('asrBackend') != 'speech-cpp' or settings.get('modelId') != native_model
                or settings.get('modelQuantization') != '8bit' or settings.get('asrDevice') != 'cpu'
                or any(settings.get(key) is not False for key in (
                    'livePreviewEnabled', 'incrementalFinalAsr', 'statefulStreaming', 'smartDictateEdit'))))
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


def live_process_start(pid, *, exit_wait_ms=0):
    """Return the Windows creation time of the current live process at pid."""
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.OpenProcess.argtypes = [wt.DWORD, wt.BOOL, wt.DWORD]
    kernel.OpenProcess.restype = wt.HANDLE
    kernel.WaitForSingleObject.argtypes = [wt.HANDLE, wt.DWORD]
    kernel.WaitForSingleObject.restype = wt.DWORD
    kernel.GetProcessTimes.argtypes = [wt.HANDLE] + [ctypes.POINTER(wt.FILETIME)] * 4
    kernel.CloseHandle.argtypes = [wt.HANDLE]
    handle = kernel.OpenProcess(0x101000, False, pid)  # SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION
    if not handle:
        error = ctypes.get_last_error()
        if error == 87:  # No process with this PID.
            return None
        raise ctypes.WinError(error)
    try:
        state = kernel.WaitForSingleObject(handle, exit_wait_ms)
        if state == 0:  # The process has exited.
            return None
        if state != 0x102:  # WAIT_TIMEOUT means the process is still live.
            raise ctypes.WinError(ctypes.get_last_error())
        created, exited, system, user = (wt.FILETIME() for _ in range(4))
        if not kernel.GetProcessTimes(handle, ctypes.byref(created), ctypes.byref(exited),
                                      ctypes.byref(system), ctypes.byref(user)):
            raise ctypes.WinError(ctypes.get_last_error())
        return (created.dwHighDateTime << 32) | created.dwLowDateTime
    finally:
        kernel.CloseHandle(handle)


def worker_identity(proof):
    pid, start = proof.get('worker_pid'), proof.get('worker_start_time')
    if type(pid) is not int or pid <= 0 or type(start) is not int or start <= 0:
        raise EvidenceError('worker process identity was not recorded')
    return pid, start


def worker_exited_before_recovery(directory):
    proof_path = directory / 'trial-01' / 'capture-proof.json'
    proof = json.loads(proof_path.read_text(encoding='utf-8'))
    pid, start = worker_identity(proof)
    requests = (directory / 'transcription-requests.log').read_text(encoding='ascii').splitlines()
    return (requests == ['transcribe'] and not (directory / 'crash-once').exists()
            and live_process_start(pid, exit_wait_ms=2000) != start)


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
                                                 'score_asr.py', 'windows_uia_bridge.ps1', 'windows_acceptance_target.html',
                                                 'windows_acceptance_terminal.py',
                                                 'windows_acceptance_mintty.sh', 'windows_acceptance_putty.py')},
            'os_build': sys.getwindowsversion().build,
            'application_binary_sha256': digest,
            'application_identity_scope': 'loaded executable file; debug frontend is separate'}


class UIABridge:
    def __init__(self, *, owned_notepad=False):
        bridge = Path(__file__).with_name('windows_uia_bridge.ps1')
        env = None
        if owned_notepad:
            # The provisioned guest has Windows PowerShell and does not require PowerShell 7.
            powershell = Path(os.environ['SystemRoot']) / 'System32' / 'WindowsPowerShell' / 'v1.0' / 'powershell.exe'
            env = {**os.environ, 'ASR_EVAL_UIA_SOURCE': str(bridge.resolve())}
            bootstrap = '& ([scriptblock]::Create([IO.File]::ReadAllText($env:ASR_EVAL_UIA_SOURCE)))'
            code = base64.b64encode(bootstrap.encode('utf-16-le')).decode('ascii')
            command = [str(powershell), '-NoProfile', '-NonInteractive', '-EncodedCommand', code]
        else:
            command = ['pwsh.exe', '-NoProfile', '-NonInteractive', '-File', str(bridge)]
        self.owned_notepad = owned_notepad
        self.process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, encoding='utf-8', creationflags=subprocess.CREATE_NO_WINDOW, env=env)
        self.responses = queue.Queue()
        def read():
            for line in self.process.stdout:
                self.responses.put(line)
            self.responses.put(None)
        threading.Thread(target=read, daemon=True).start()

    def call(self, action, target, **fields):
        try:
            self.process.stdin.write(json.dumps({'action': action, 'target': target, **fields}) + '\n')
            self.process.stdin.flush()
            timeout = {'focused': .75, 'open_notepad': 18, 'open_fixture': 35}.get(action, 8)
            line = self.responses.get(timeout=timeout)
            result = json.loads(line) if line else {}
            if result.get('ok') is not True:
                if action in ('open_notepad', 'close_notepad', 'open_fixture', 'close_fixture'):
                    error = result.get('error', 'owned_target_unavailable')
                    if re.fullmatch(r'owned_[a-z_]+', error):
                        raise ScenarioEvidenceError(error, 'owned target could not be prepared or cleaned up')
                raise UnsupportedTarget('UI Automation target unsupported, blocked, or changed')
            return result
        except (OSError, ValueError, queue.Empty) as exc:
            self.close()
            raise UnsupportedTarget('UI Automation unavailable; no fallback input was sent') from exc

    def close(self):
        if self.owned_notepad and self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=4)
            except subprocess.TimeoutExpired:
                pass
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

    def require_matrix_caret(self):
        result = self.bridge.call('matrix_caret', self.name, prefix=NORMAL_MATRIX_PREFIX)
        if not result['at_end']:
            raise UnsupportedTarget('external caret could not be verified after the matrix sentinel')

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

    def change_count(self):
        return len(self.window.snapshot()[1])

    def changed_since(self, start, *permitted):
        return any(change.text not in permitted for change in self.window.snapshot()[1][start:])

    def select_all(self):
        result = ctypes.c_size_t()
        if not self.api.user.SendMessageTimeoutW(self.window.edit, 0xB1, 0, -1, 0x23, 250, ctypes.byref(result)):
            raise UnsupportedTarget('native selection timed out')

    def select_caret(self, offset):
        require_focus(self)
        result = ctypes.c_size_t()
        if not self.api.user.SendMessageTimeoutW(self.window.edit, 0xB1, offset, offset, 0x23, 250, ctypes.byref(result)):
            raise UnsupportedTarget('native caret placement timed out')
        self.require_caret(offset)

    def require_caret(self, offset):
        self.require_selection(offset, offset)

    def require_selection(self, start, end):
        require_focus(self)
        result = ctypes.c_size_t()
        if (not self.api.user.SendMessageTimeoutW(self.window.edit, 0xB0, 0, 0, 0x23, 250, ctypes.byref(result))
                or result.value != (end << 16 | start)):
            raise UnsupportedTarget('native selection does not match the expected positions')

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
        self.api.bind(self.api.user, 'IsIconic', wt.BOOL, wt.HWND)
        if not self.api.user.IsIconic(self.window.hwnd):
            raise UnsupportedTarget('native fixture was not minimized')


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


def matrix_text(condition, nonce):
    if not re.fullmatch(r'[0-9a-f]{16}', nonce):
        raise EvidenceError('invalid matrix trial nonce')
    endings = {
        'short': '確認しました。',
        'mixed': '会議 ABC-123 🗾 確認済み。',
        'multiline': '一行目を確認。\n二行目も確認。\n三行目です。',
        'long': '長文の挿入試験。' * 128,
    }
    if condition not in endings:
        raise EvidenceError('invalid matrix text condition')
    text = endings[condition]
    return (text[:1024 - len(nonce)] if condition == 'long' else text) + nonce


def receiver_text(text):
    return text.replace('\r\n', '\n').replace('\r', '\n')


class ClipboardFixture(Clipboard):
    """Known-format sentinels for the dedicated guest, with independent readback."""

    # BITMAPINFOHEADER and one padded, 24-bit BGR pixel (CF_DIB).
    dib = struct.pack('<IiiHHIIiiII', 40, 1, 1, 1, 24, 0, 4, 0, 0, 0, 0) + b'\x26\x67\xab\x00'

    def __init__(self, api, hwnd, scenario):
        super().__init__(api, hwnd)
        self.scenario = scenario
        self.original = f'ASR fixture original {uuid.uuid4().hex}'
        self.newer = f'ASR fixture newer {uuid.uuid4().hex}'

    def seed(self):
        if self.scenario != 'clipboard-rich':
            self.write(self.original, exclude_from_history=True)
            return
        with self.opened():
            if not self.api.user.EmptyClipboard():
                raise BenchError('could not clear the evaluation clipboard')
            self._set_data(self.exclude_format, b'\0' * 4)
            self._set_data(13, (self.original + '\0').encode('utf-16-le'))
            self._set_data(8, self.dib)
        if not self.preserved():
            raise EvidenceError('could not seed the rich clipboard fixture')

    def copy_newer(self):
        self.write(self.newer, exclude_from_history=True)
        if self.read() != self.newer:
            raise EvidenceError('newer clipboard fixture was not published')

    def preserved(self):
        if self.scenario != 'clipboard-rich':
            return self.read() == self.newer
        with self.opened():
            if self._read_opened() != self.original or not self.api.user.IsClipboardFormatAvailable(8):
                return False
            handle = self.api.user.GetClipboardData(8)
            size = self.api.bind(self.api.kernel, 'GlobalSize', ctypes.c_size_t, wt.HANDLE)
            if not handle or size(handle) < len(self.dib):
                return False
            pointer = self.api.kernel.GlobalLock(handle)
            if not pointer:
                return False
            try:
                return ctypes.string_at(pointer, len(self.dib)) == self.dib
            finally:
                self.api.kernel.GlobalUnlock(handle)


def judge(scenario, primary, secondary, take, reference, max_cer, *, mock_text=MOCK_TEXT,
          normal_matrix=False):
    if scenario in (*MOCK_RECEIVERS, *VM_CLASSIC_TERMINALS, *CLIPBOARD_GUARDS):
        if normal_matrix:
            primary = receiver_text(primary)
            expected = NORMAL_MATRIX_PREFIX + mock_text
        else:
            expected = (MOCK_PREFIX + mock_text + MOCK_SUFFIX
                        if scenario in (MOCK_INSERTION, *CLIPBOARD_GUARDS) else mock_text)
        exact = primary == expected and not secondary
        result = {'status': 'PASS' if exact else 'FAIL', 'exact_receiver_match': exact}
        if exact and take['outcome'] not in DELIVERY_OUTCOMES:
            result.update(status='FAIL', reason='app_outcome_mismatch')
        return result
    if scenario == 'dictate':
        cer = cer_stats(reference, primary)['cer']
        passed = bool(primary.strip()) and not secondary and take['outcome'] == 'pasted' and cer <= max_cer
        return {'status': 'PASS' if passed else 'FAIL', 'normalized_cer': cer}
    expected = ('failed' if scenario == WORKER_CRASH else
                'cancelled' if scenario in ('cancel', 'cancel-processing') else 'clipboard_only')
    passed = primary == SEED and not secondary and take['outcome'] == expected
    return {'status': 'PASS' if passed else 'FAIL', 'normalized_cer': None}


def delivery_digest(text):
    return hashlib.sha256(text.replace('\r\n', '\n').replace('\r', '\n').encode('utf-8')).hexdigest()


def read_real_proof(path):
    try:
        proof = json.loads(path.read_text(encoding='utf-8'))
    except FileNotFoundError:
        return None
    if (not isinstance(proof, dict) or proof.get('backend') != 'faster-whisper'
            or proof.get('device') != 'cpu' or proof.get('compute_type') != 'int8'
            or type(proof.get('captured_frames')) is not int or proof['captured_frames'] <= 0
            or type(proof.get('sample_rate')) is not int or proof['sample_rate'] <= 0
            or not isinstance(proof.get('delivery_sha256'), str)
            or not re.fullmatch('[0-9a-f]{64}', proof['delivery_sha256'])):
        raise EvidenceError('invalid real worker evidence')
    return proof


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
                 recording_mode='toggle', unattended=False, real_proof=None, mock_text=MOCK_TEXT,
                 normal_matrix=False, inflight_proof=None, clipboard_fixture=None):
    arguments = dict(api=api, primary=primary, secondary=secondary, reader=reader,
                     player=player, item=item, hotkey=hotkey, max_cer=max_cer,
                     timeout=timeout, guard_seconds=guard_seconds, clock=clock, ime=ime,
                     unattended=unattended, real_proof=real_proof, mock_text=mock_text,
                     normal_matrix=normal_matrix, inflight_proof=inflight_proof,
                     clipboard_fixture=clipboard_fixture)
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
                  still_held=lambda: None, unattended=False, real_proof=None, mock_text=MOCK_TEXT,
                  normal_matrix=False, inflight_proof=None, clipboard_fixture=None):
    reader.ready()
    if reader.phase != 'idle':
        raise EvidenceError('app not idle; never retry its toggle blindly')
    baseline = reader.sequence
    primary.focus()
    original = primary.read()
    mock_insertion = scenario in (MOCK_INSERTION, *CLIPBOARD_GUARDS)
    native_notepad = scenario == NATIVE_NOTEPAD
    empty_receiver = scenario in ('dictate', NATIVE_NOTEPAD, *REAL_RECEIVERS, *MOCK_EXTERNAL, *VM_CLASSIC_TERMINALS)
    confirms_insertion = empty_receiver or mock_insertion
    expected_initial = (NORMAL_MATRIX_PREFIX if normal_matrix else
                        MOCK_PREFIX + MOCK_SUFFIX if mock_insertion else ('' if empty_receiver else SEED))
    if original != expected_initial or secondary.read():
        raise EvidenceError('unexpected scratch state before recording')
    caret_offset = len(NORMAL_MATRIX_PREFIX if normal_matrix else MOCK_PREFIX)
    if mock_insertion:
        primary.select_caret(caret_offset)
    elif normal_matrix and scenario != 'mock-terminal':
        primary.require_matrix_caret()
    guard_counts = (primary.change_count(), secondary.change_count()) if (
        unattended and scenario in (*MOCK_GUARDS, *VM_EXTRA_GUARDS, *CLIPBOARD_GUARDS,
                                    'cancel-processing', WORKER_CRASH)) else None

    def guarded_receivers_changed(*permitted):
        return guard_counts is not None and (primary.changed_since(guard_counts[0], original, *permitted)
                                             or secondary.changed_since(guard_counts[1], ''))

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
        if primary.read() != original or secondary.read() or guarded_receivers_changed():
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
    if mock_insertion:
        primary.require_caret(caret_offset)
    elif normal_matrix and scenario != 'mock-terminal':
        primary.require_matrix_caret()
    hold_check()
    receiver_violation = False
    secondary_violation = False
    proof = None
    first_confirmed_at = None
    seen_native_text = {}
    permitted_changes = (MOCK_PREFIX + mock_text + MOCK_SUFFIX,) if scenario in CLIPBOARD_GUARDS else ()

    def monitor_receiver():
        nonlocal receiver_violation, secondary_violation, proof, first_confirmed_at
        secondary_violation |= bool(secondary.read())
        if native_notepad:
            text = primary.read()
            if len(text) > 4096 or len(seen_native_text) >= 64 and text not in seen_native_text:
                receiver_violation = True
            elif text:
                seen_native_text.setdefault(text, clock.perf_counter())
        elif real_proof is not None:
            text = primary.read()
            observed_at = clock.perf_counter()
            if proof is None:
                proof = read_real_proof(real_proof)
            if text:
                if proof and delivery_digest(text) == proof['delivery_sha256']:
                    if first_confirmed_at is None:
                        first_confirmed_at = observed_at
                else:
                    receiver_violation = True
        elif scenario in (*MOCK_RECEIVERS, *VM_CLASSIC_TERMINALS, *CLIPBOARD_GUARDS):
            if normal_matrix:
                # Some terminals expose a paste in chunks on their readback pipe.
                # Any transient prefix is permitted; the final read must match exactly.
                observed = receiver_text(primary.read())
                receiver_violation |= not (observed.startswith(original)
                                           and (original + mock_text).startswith(observed))
            else:
                expected = MOCK_PREFIX + mock_text + MOCK_SUFFIX if mock_insertion else mock_text
                receiver_violation |= primary.read() not in (original, expected)
        if secondary_violation or (not confirms_insertion and not ime and primary.read() != original):
            receiver_violation = True
        receiver_violation |= guarded_receivers_changed(*permitted_changes)
    if ime:
        ime.compose()
    if scenario == 'selection-change':
        if stop is None:
            if unattended:
                primary.require_caret(len(SEED))
            tap(api, primary, (0x10, 0x25))  # Shift+Left, observed by product input monitor.
            if unattended:
                primary.require_selection(len(SEED) - 1, len(SEED))
        else:
            primary.change_held_selection()
    target = primary
    if scenario in ('focus-away', 'minimize'):
        secondary.focus()
        target = secondary
    if scenario == 'minimize':
        primary.minimize()
    processing_cancel_attempted = False
    worker_hold_observed = False
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
            stop_at = tap(api, target, hotkey)
        else:
            hold_check()
            still_held()
            require_recording()
            stop(target)
        if scenario in HELD_WORKER_GUARDS:
            def check_held_target():
                require_focus(primary)
                if inflight_proof is not None:
                    before_stop_check()
                    if unattended:
                        primary.require_caret(caret_offset if mock_insertion else len(SEED))

            reader.wait(lambda r: (r.phase == 'processing' and r.operation_id == operation
                                   and (inflight_proof is None or inflight_proof.is_file()))
                        or r.completed(operation), timeout,
                        check=check_held_target)
            if reader.phase == 'processing' and reader.operation_id == operation:
                check_held_target()
                worker_hold_observed = inflight_proof is not None and inflight_proof.is_file()
                if scenario == 'cancel-processing':
                    tap(api, primary, (0x1B,))
                    processing_cancel_attempted = True
                else:
                    clipboard_fixture.copy_newer()
                    check_held_target()
                    # The private hold marker was created by the staged worker.
                    inflight_proof.with_name('hold-transcribe').unlink()
    def check_delivery():
        try:
            monitor_receiver()
            if unattended or mock_insertion or (stop is not None and scenario != 'dictate'):
                require_focus(target)
        except UnsupportedTarget as exc:
            raise ScenarioEvidenceError('target_changed_after_stop',
                                        'receiver or focus could not be verified after the stop action') from exc
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
    try:
        first, second = primary.read(), secondary.read()
        if stop is not None and scenario != 'dictate':
            require_focus(target)
    except UnsupportedTarget as exc:
        raise ScenarioEvidenceError('target_changed_after_stop',
                                    'receiver or focus could not be verified after the stop action') from exc
    if native_notepad:
        first_seen = seen_native_text.get(first)
        cer = cer_stats(item['reference'], first)['cer']
        delivered = bool(first.strip()) and take['outcome'] == 'pasted' and not second
        receiver_violation |= any(not first.startswith(text) for text in seen_native_text)
        result = {'status': 'PASS' if delivered and first_seen is not None and cer <= max_cer else 'FAIL',
                  'reference_normalized_cer': cer,
                  'stop_to_observed_insertion_ms': ((first_seen - stop_at) * 1000
                                                    if delivered and first_seen is not None else None)}
    elif real_proof is None:
        result = judge(scenario, first, second, take, item['reference'], max_cer,
                       mock_text=mock_text, normal_matrix=normal_matrix)
    else:
        proof = proof or read_real_proof(real_proof)
        exact = bool(proof and first and delivery_digest(first) == proof['delivery_sha256'] and not second)
        result = {'status': 'PASS' if exact else 'FAIL', 'exact_receiver_match': exact,
                  'stop_to_observed_insertion_ms': (first_confirmed_at - stop_at) * 1000
                  if exact and first_confirmed_at is not None else None}
        if exact and take['outcome'] not in DELIVERY_OUTCOMES:
            result.update(status='FAIL', reason='app_outcome_mismatch')
        elif exact and first_confirmed_at is None:
            result.update(status='BLOCKED', reason='missing_insertion_observation_time')
        if proof is None and not receiver_violation and not first and not second:
            result.update(status='BLOCKED', reason='real_worker_evidence_missing')
    if receiver_violation or guarded_receivers_changed(*permitted_changes):
        receiver_violation = True
        result['status'] = 'FAIL'
        if unattended:
            result['reason'] = 'unexpected_receiver_mutation'
    if clipboard_fixture is not None:
        result['clipboard_preserved'] = clipboard_fixture.preserved()
        if not result['clipboard_preserved']:
            result.update(status='FAIL', reason='clipboard_fixture_not_preserved')
        if scenario == 'clipboard-newer-during-asr':
            result['worker_hold_observed'] = worker_hold_observed
            if not worker_hold_observed and result['status'] == 'PASS':
                result.update(status='BLOCKED', reason='clipboard_worker_hold_not_observed')
    if not confirms_insertion:
        result['receiver_unchanged'] = first == original and not second and not receiver_violation
    if scenario == 'cancel' and stop is not None:
        result['requested_cancel_method'] = 'operator_overlay'
        result['cancel_source_verified'] = False
        if (result['receiver_unchanged'] and any(
                event['operation_id'] == operation and event['phase'] == 'processing'
                for event in reader.events)):
            result.update(status='BLOCKED', reason='processing_observed_before_cancel_completion')
    if scenario == 'cancel' and unattended:
        saw_processing = any(event['operation_id'] == operation and event['phase'] == 'processing'
                             for event in reader.events)
        result.update(cancel_method='escape', processing_observed=saw_processing)
        if saw_processing:
            result.update(status='FAIL', reason='processing_observed_after_recording_cancel')
    if scenario == 'cancel-processing':
        result['processing_cancel_attempted'] = processing_cancel_attempted
        result['worker_hold_observed'] = worker_hold_observed
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


def require_inactive_ime(api, target):
    require_focus(target)
    get_thread = api.user.GetWindowThreadProcessId
    get_layout = api.bind(api.user, 'GetKeyboardLayout', wt.HANDLE, wt.DWORD)
    imm = ctypes.WinDLL('imm32', use_last_error=True)
    is_ime = api.bind(imm, 'ImmIsIME', wt.BOOL, wt.HANDLE)
    thread = get_thread(target.root, None)
    layout = get_layout(thread) if thread else None
    if not layout:
        raise UnsupportedTarget('normal matrix could not identify the keyboard layout')
    if not is_ime(layout):
        return
    ime_window = api.bind(imm, 'ImmGetDefaultIMEWnd', wt.HWND, wt.HWND)(target.root)
    is_open = ctypes.c_size_t()
    if not ime_window or not api.user.SendMessageTimeoutW(ime_window, 0x0283, 5, 0, 0x23,
                                                          250, ctypes.byref(is_open)):
        raise UnsupportedTarget('normal matrix could not read the IME open status')
    if is_open.value:
        raise UnsupportedTarget('normal matrix requires an inactive IME')
    require_focus(target)


def require_evaluation_guest():
    import winreg

    with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, r'HARDWARE\DESCRIPTION\System\BIOS') as bios:
        product, _ = winreg.QueryValueEx(bios, 'SystemProductName')
    if product != 'Virtual Machine' or socket.gethostname().casefold() != 'asr-eval':
        raise UnsupportedTarget('unattended insertion requires the ASR-Eval guest')


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
    parser.add_argument('--scenario', action='append',
                        choices=SCENARIOS + OPTIONAL_SCENARIOS + tuple(MOCK_EXTERNAL) + tuple(VM_CLASSIC_TERMINALS)
                        + CLIPBOARD_GUARDS)
    parser.add_argument('--recording-mode', choices=('toggle', 'hold'), default='toggle')
    parser.add_argument('--max-cer', type=float, default=.5)
    parser.add_argument('--mock-insertion', action='store_true', help='run one unattended native mock case in ASR-Eval')
    parser.add_argument('--mock-text', help=argparse.SUPPRESS)
    parser.add_argument('--matrix-condition', choices=NORMAL_MATRIX_CONDITIONS, help=argparse.SUPPRESS)
    parser.add_argument('--matrix-nonce', help=argparse.SUPPRESS)
    parser.add_argument('--inflight-proof', type=Path, help=argparse.SUPPRESS)
    parser.add_argument('--worker-recovery', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--mock-on-real-profile', action='store_true', help=argparse.SUPPRESS)
    real_mode = parser.add_mutually_exclusive_group()
    real_mode.add_argument('--real-notepad', dest='real_case', action='store_const', const=REAL_NOTEPAD)
    real_mode.add_argument('--real-case', choices=tuple(REAL_RECEIVERS))
    real_mode.add_argument('--native-notepad', action='store_true')
    parser.add_argument('--native-model', choices=NATIVE_MODELS, help=argparse.SUPPRESS)
    parser.add_argument('--worker-proof', type=Path)
    args = parser.parse_args(argv)
    if args.native_model and not args.native_notepad:
        parser.error('--native-model requires --native-notepad')
    if args.native_notepad:
        args.native_model = args.native_model or NATIVE_MODEL
    expects_hold = (args.mock_insertion and args.scenario is not None and len(args.scenario) == 1
                    and args.scenario[0] in HELD_WORKER_GUARDS)
    if bool(args.inflight_proof) != expects_hold:
        parser.error('VM held-worker checks require a staged proof of active transcription')
    if expects_hold:
        case_dir = Path(__file__).resolve().parents[1] / 'cases' / args.scenario[0]
        if (args.output_dir.resolve() != case_dir / 'acceptance'
                or args.inflight_proof.resolve() != case_dir / 'worker-hold-ready'):
            parser.error('held-worker evidence must belong to its staged case')
    matrix = args.matrix_condition is not None or args.matrix_nonce is not None
    if matrix and (not args.mock_insertion or args.mock_text is not None or args.worker_recovery
                   or args.matrix_condition is None or args.matrix_nonce is None
                   or not re.fullmatch(r'[0-9a-f]{16}', args.matrix_nonce)
                   or (args.scenario or [MOCK_INSERTION])[0] not in NORMAL_MATRIX_TARGETS):
        parser.error('matrix payload requires a normal mock receiver, condition and nonce')
    mock_text = matrix_text(args.matrix_condition, args.matrix_nonce) if matrix else args.mock_text or MOCK_TEXT
    if args.mock_text is not None and (not args.mock_insertion or not re.fullmatch(
            re.escape(MOCK_TEXT) + r'[0-9a-f]{16}', args.mock_text)):
        parser.error('--mock-text requires a nonce-tagged VM mock payload')
    if args.mock_on_real_profile and not args.mock_insertion:
        parser.error('--mock-on-real-profile requires --mock-insertion')
    if args.worker_recovery and (not args.mock_insertion or args.scenario):
        parser.error('--worker-recovery requires --mock-insertion and selects its own scenarios')
    unattended = args.mock_insertion or bool(args.real_case) or args.native_notepad
    if args.real_case and (args.mock_insertion or args.scenario or args.target != 'native'
                              or args.recording_mode != 'toggle' or args.worker_proof is None):
        parser.error('a real case requires toggle mode, an owned target and --worker-proof')
    if not args.real_case and args.worker_proof is not None:
        parser.error('--worker-proof requires a real case')
    if args.native_notepad:
        profile_name, _ = NATIVE_MODELS[args.native_model]
        expected_profile = Path(os.environ.get('APPDATA', '')) / profile_name / 'local-voice-input.sqlite3'
        if (args.mock_insertion or args.scenario or args.target != 'native' or args.recording_mode != 'toggle'
                or args.profile_db.resolve() != expected_profile.resolve()):
            parser.error('native Notepad requires its dedicated evaluation profile and toggle mode')
    if not 0 <= args.max_cer <= 1:
        parser.error('--max-cer must be between 0 and 1')
    if args.mock_insertion and (args.target != 'native' or args.recording_mode != 'toggle'
                                or (args.scenario and (len(args.scenario) != 1
                                                       or args.scenario[0] not in VM_MOCK_CASES))):
        parser.error('--mock-insertion requires toggle mode and one supported VM case')
    if not args.mock_insertion and any(case in (*MOCK_EXTERNAL, *VM_CLASSIC_TERMINALS, *CLIPBOARD_GUARDS)
                                       for case in (args.scenario or ())):
        parser.error('each VM mock case requires --mock-insertion in ASR-Eval')
    scenarios = (args.scenario or (MOCK_INSERTION,)) if args.mock_insertion else (
        args.scenario or (('dictate',) if args.recording_mode == 'hold' else SCENARIOS))
    if args.real_case:
        scenarios = (args.real_case,)
    elif args.native_notepad:
        scenarios = (NATIVE_NOTEPAD,)
    if args.worker_recovery:
        scenarios = WORKER_RECOVERY_TRIALS
    owned_target = ('notepad' if args.native_notepad else REAL_RECEIVERS.get(args.real_case) or MOCK_EXTERNAL.get(scenarios[0])
                    or VM_CLASSIC_TERMINALS.get(scenarios[0]))
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
    def read_profile():
        return profile_settings(args.profile_db, args.microphone_id, args.recording_mode,
                                mock_insertion=args.mock_insertion, real_notepad=bool(args.real_case),
                                mock_on_real_profile=args.mock_on_real_profile, native_model=args.native_model)

    profile = read_profile()
    hotkey = parse_hotkey(profile['hotkey'])
    args.output_dir.mkdir(parents=True, exist_ok=False)
    report = {'schema': 'asr-windows-acceptance/1', 'target': f'owned_{owned_target}' if owned_target else args.target,
              'item_id': item['id'], 'audio_sha256': item['audio_sha256'],
              'max_cer': None if unattended else args.max_cer, 'recording_mode': args.recording_mode,
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
    if not unattended:
        print('評価中は専用の空欄だけを使います。アプリのモデル・録音権限を確認し、停止してから開始してください。')
        input('準備できたら Enter（終了は Ctrl+C）: ')
    active_scenario = 'setup'
    try:
        if unattended:
            require_evaluation_guest()
        with contextlib.ExitStack() as stack:
            if owned_target:
                secondary = NativeTarget(api, stack.enter_context(TargetWindow(api, foreground_mode='legacy')))
                secondary.focus()
                bridge = UIABridge(owned_notepad=True)
                stack.callback(bridge.close)
                if owned_target == 'notepad':
                    fixture = args.output_dir / f'asr-fixture-{uuid.uuid4().hex}.txt'
                    fixture.touch(exist_ok=False)
                    target = bridge.call('open_notepad', 'primary', path=str(fixture.resolve()))
                    stack.callback(bridge.call, 'close_notepad', 'primary')
                else:
                    target = bridge.call('open_fixture', 'primary', kind=owned_target,
                                         bracketed=matrix and owned_target == 'terminal')
                    stack.callback(bridge.call, 'close_fixture', 'primary')
                primary = ExternalTarget(bridge, 'primary', target['window'])
                report['applications'] = [{'process': target['process'], 'version': target['version']},
                                          {'process': 'owned_native_edit', 'version': None}]
            elif args.target == 'native':
                report['applications'] = [{'process': 'owned_native_edit', 'version': None}]
                foreground_mode = 'legacy' if args.mock_insertion else 'plain'
                windows = [stack.enter_context(TargetWindow(api, foreground_mode=foreground_mode)) for _ in range(2)]
                primary, secondary = [NativeTarget(api, window) for window in windows]
                if not args.mock_insertion:
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
                if read_profile() != profile:
                    raise EvidenceError('profile changed')
                reader.ready()
                if reader.phase != 'idle':
                    raise EvidenceError('app not idle before scratch preparation')
                Keyboard(api.send, primary, hotkey, key_down=api.key_down,
                         idle_seconds=api.idle_seconds).before_trial()
                if matrix:
                    primary.focus()
                    require_inactive_ime(api, primary)
                if owned_target:
                    require_focus(primary)
                    if primary.read() or secondary.read():
                        raise EvidenceError('owned receiver and decoy must be empty before recording')
                    if matrix:
                        unicode_text(api, primary, NORMAL_MATRIX_PREFIX)
                        if scenario != 'mock-terminal':
                            tap(api, primary, (0x11, 0x23))  # Ctrl+End in the owned editor.
                        deadline = time.perf_counter() + 3
                        while primary.read() != NORMAL_MATRIX_PREFIX and time.perf_counter() < deadline:
                            time.sleep(.05)
                elif args.mock_insertion:
                    initial = (NORMAL_MATRIX_PREFIX if matrix else
                               MOCK_PREFIX + MOCK_SUFFIX if scenario in (MOCK_INSERTION, *CLIPBOARD_GUARDS) else SEED)
                    prepare(api, primary, previous[0], initial)
                else:
                    prepare(api, secondary, previous[1], '')
                    prepare(api, primary, previous[0], '' if scenario == 'dictate' else SEED)
                print(f'{scenario}: 開始', flush=True)
                with contextlib.ExitStack() as scenario_cleanup:
                    ime = NativeIME(api, primary) if scenario == 'ime-composition' else None
                    if ime:
                        scenario_cleanup.callback(ime.close)
                    clipboard_fixture = None
                    if scenario in CLIPBOARD_GUARDS:
                        clipboard_fixture = ClipboardFixture(api, int(secondary.root), scenario)
                        clipboard_fixture.seed()
                    terminal_clipboard = None
                    if unattended and scenario in ('mock-terminal', *VM_CLASSIC_TERMINALS):
                        terminal_clipboard = Clipboard(api, int(secondary.root))
                        sentinel = f'ASR terminal previous copy {uuid.uuid4().hex}'
                        terminal_clipboard.write(sentinel, exclude_from_history=True)
                    result, previous[0], previous[1] = run_scenario(
                        scenario, api=api, primary=primary, secondary=secondary, reader=reader,
                        player=player, item=item, hotkey=hotkey, max_cer=args.max_cer, ime=ime,
                        recording_mode=args.recording_mode, unattended=unattended,
                        real_proof=args.worker_proof, mock_text=mock_text, normal_matrix=matrix,
                        inflight_proof=args.inflight_proof,
                        clipboard_fixture=clipboard_fixture,
                        timeout=90 if args.real_case or args.native_notepad else (
                            15 if args.mock_insertion else 60))
                    if terminal_clipboard and result['status'] == 'PASS':
                        contents = terminal_clipboard.read()
                        if contents == sentinel:
                            result['terminal_clipboard'] = 'restored'
                        elif contents == mock_text:
                            result['terminal_clipboard'] = 'transcript_retained'
                        else:
                            result.update(status='FAIL', reason='unexpected_terminal_clipboard_contents')
                if read_profile() != profile:
                    raise EvidenceError('profile changed during trial')
                if scenario == WORKER_CRASH and result['status'] == 'PASS':
                    exited = worker_exited_before_recovery(args.output_dir.parent)
                    result['worker_exit_before_recovery'] = exited
                    if not exited:
                        result.update(status='FAIL', reason='worker_exit_not_verified_before_recovery')
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
        status = 'FAIL' if unattended and getattr(exc, 'code', None) == 'receiver_changed_during_playback' else 'BLOCKED'
        report['results'].append({'scenario': active_scenario, 'status': status,
                                  'reason': getattr(exc, 'code', reason) if unattended else reason})
        if unattended:
            print(f'VM insertion {status}: {exc}', file=sys.stderr)
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
