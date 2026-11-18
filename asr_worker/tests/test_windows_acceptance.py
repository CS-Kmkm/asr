"""Exercise acceptance orchestration without sending Windows input."""
from pathlib import Path
import contextlib
import json
import sqlite3
import sys
import tempfile
import threading
import wave
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'scripts'))
try:
    import windows_acceptance as module
finally:
    sys.path.pop(0)


class Clock:
    now = 0
    def perf_counter(self): return self.now
    def sleep(self, seconds): self.now += seconds


class Target:
    def __init__(self, content=''):
        self.content, self.focused, self.minimized = content, False, False
    def focus(self): self.focused = True
    def focus_process(self): return None if self.focused else 'other'
    def read(self): return self.content
    def minimize(self): self.minimized = True


class Reader:
    def __init__(self, outcome):
        self.sequence, self.phase, self.operation_id = 0, 'idle', None
        self.takes = {7: {'outcome': outcome, 'spans_ms': {}}}
        self.events = []
    def ready(self): pass
    def poll(self): pass
    def recording_after(self, sequence): return 7 if self.phase == 'recording' else None
    def completed(self, operation): return self.phase == 'idle' and operation == 7
    def wait(self, predicate, timeout, check=lambda: None):
        check()
        if not predicate(self): raise module.EvidenceError('state timeout')


class AcceptanceTests(unittest.TestCase):
    def test_hold_playback_abort_does_not_wait_for_audio_worker(self):
        release, entered, finished = threading.Event(), threading.Event(), threading.Event()
        def play(path):
            entered.set()
            release.wait(2)
            finished.set()
            return 1, 2, 1000
        reader = Reader('pasted')
        reader.poll = lambda: self.assertTrue(entered.wait(1))
        primary = Target()
        primary.focus()
        def changed(): raise module.EvidenceError('operator input')
        with tempfile.TemporaryDirectory() as directory:
            audio = Path(directory) / 'silence.wav'
            with wave.open(str(audio), 'wb') as output:
                output.setnchannels(1)
                output.setsampwidth(2)
                output.setframerate(16000)
                output.writeframes(b'\0\0' * 16)
            try:
                with self.assertRaises(module.EvidenceError):
                    module.observed_playback(SimpleNamespace(play=play), audio, reader, primary,
                                             changed, abort_on_failure=True)
                self.assertFalse(finished.is_set())
            finally:
                release.set()
                self.assertTrue(finished.wait(1))

    def test_receiver_text_alone_or_unverified_paste_is_not_acceptance(self):
        for outcome in ('clipboard_only', 'paste_unverified', 'failed'):
            self.assertEqual(module.judge('dictate', 'public', '', {'outcome': outcome}, 'public', .1)['status'], 'FAIL')

    def test_wrong_target_late_output_or_damaged_source_fails(self):
        for first, second in [(module.SEED+'public', ''), (module.SEED, 'public'), ('', '')]:
            self.assertEqual(module.judge('focus-away', first, second, {'outcome': 'clipboard_only'}, 'public', .5)['status'], 'FAIL')
        self.assertEqual(module.judge('cancel', module.SEED, '', {'outcome': 'pasted'}, 'public', .5)['status'], 'FAIL')

    def test_failed_start_never_plays_audio_or_sends_stop_toggle(self):
        reader, primary, secondary = Reader('pasted'), Target(), Target()
        calls = []
        with patch.object(module, 'tap', side_effect=lambda *args: calls.append(args[-1])), \
             patch.object(module, 'observed_playback') as playback:
            with self.assertRaises(module.EvidenceError):
                module.run_scenario('dictate', api=None, primary=primary, secondary=secondary,
                                    reader=reader, player=None, item={'audio_path': 'unused', 'reference': 'public'},
                                    hotkey=(17, 32), clock=Clock())
            playback.assert_not_called()
        self.assertEqual(calls, [(17, 32)])

    def test_unavailable_ime_is_detected_before_starting_recording(self):
        reader, primary, secondary = Reader('clipboard_only'), Target(module.SEED), Target()
        def unsupported(): raise module.UnsupportedTarget('no IME')
        ime = SimpleNamespace(compose=unsupported)
        with patch.object(module, 'tap') as tap, patch.object(module, 'observed_playback') as playback:
            with self.assertRaises(module.UnsupportedTarget):
                module.run_scenario('ime-composition', api=None, primary=primary, secondary=secondary,
                    reader=reader, player=None, item={'audio_path': 'unused', 'reference': 'public'},
                    hotkey=(17, 32), clock=Clock(), ime=ime)
            tap.assert_not_called()
            playback.assert_not_called()

    def test_ime_cancel_checks_until_composition_is_gone_and_is_bounded(self):
        ime = module.NativeIME.__new__(module.NativeIME)
        ime.api, ime.target = None, Target()
        ime.target.focus()
        states = iter((True, True, True, False))
        ime.active = lambda: next(states)
        ime.wait_active = lambda expected: not ime.active()
        with patch.object(module, 'tap') as tap:
            ime.cancel_composition()
            self.assertEqual(tap.call_count, 2)
        ime.active = lambda: True
        with patch.object(module, 'tap') as tap:
            with self.assertRaises(module.UnsupportedTarget): ime.cancel_composition()
            self.assertEqual(tap.call_count, 3)

    def test_each_scenario_uses_real_start_then_expected_target_operation(self):
        for scenario in module.SCENARIOS:
            with self.subTest(scenario=scenario):
                primary = Target('' if scenario == 'dictate' else module.SEED)
                secondary = Target()
                reader = Reader('pasted' if scenario == 'dictate' else 'cancelled' if scenario == 'cancel' else 'clipboard_only')
                calls = []
                def tap(api, target, chord):
                    calls.append((target, chord))
                    if len(calls) == 1:
                        reader.phase, reader.operation_id = 'recording', 7
                    elif chord == (17, 32) or chord == (27,):
                        reader.phase = 'idle'
                        if scenario == 'dictate': primary.content = 'public'
                with patch.object(module, 'tap', side_effect=tap), \
                     patch.object(module, 'observed_playback', return_value=(1, 3, 2000)):
                    result, _, _ = module.run_scenario(
                        scenario, api=None, primary=primary, secondary=secondary,
                        reader=reader, player=None, item={'audio_path': 'unused', 'reference': 'public'},
                        hotkey=(17, 32), clock=Clock(), guard_seconds=.1)
                self.assertEqual(result['status'], 'PASS')
                self.assertIs(calls[-1][0], secondary if scenario in ('focus-away', 'minimize') else primary)
                self.assertEqual(primary.minimized, scenario == 'minimize')

    def test_delayed_output_is_checked_after_idle(self):
        primary, secondary, reader = Target(module.SEED), Target(), Reader('cancelled')
        def tap(api, target, chord):
            reader.phase, reader.operation_id = ('recording', 7) if chord == (17, 32) else ('idle', None)
        clock = Clock()
        def sleep(seconds):
            clock.now += seconds
            primary.content += 'late'
        clock.sleep = sleep
        with patch.object(module, 'tap', side_effect=tap), \
             patch.object(module, 'observed_playback', return_value=(1, 3, 2000)):
            result, _, _ = module.run_scenario('cancel', api=None, primary=primary,
                secondary=secondary, reader=reader, player=None,
                item={'audio_path': 'unused', 'reference': 'public'}, hotkey=(17, 32),
                clock=clock, guard_seconds=.1)
        self.assertEqual(result['status'], 'FAIL')

    def test_hold_keeps_chord_down_through_audio_and_releases_once(self):
        primary, secondary, reader = Target(), Target(), Reader('pasted')
        pressed, batches = set(), []
        def send(events):
            batches.append(events)
            for key, up in events:
                if up: pressed.discard(key)
                else: pressed.add(key)
            if pressed:
                reader.phase, reader.operation_id = 'recording', 7
            else:
                reader.phase, primary.content = 'idle', 'public'
        api = SimpleNamespace(send=send, key_down=lambda key: key in pressed, idle_seconds=lambda: 2)
        def playback(*args, **kwargs):
            self.assertEqual(pressed, {17, 32})
            return 1, 3, 2000
        with patch.object(module, 'ConsoleKeyGuard', return_value=contextlib.nullcontext()), \
             patch.object(module, 'observed_playback', side_effect=playback):
            result, _, _ = module.run_scenario('dictate', api=api, primary=primary, secondary=secondary,
                reader=reader, player=None, item={'audio_path': 'unused', 'reference': 'public'},
                hotkey=(17, 32), clock=Clock(), guard_seconds=.1, recording_mode='hold')
        self.assertEqual(result['status'], 'PASS')
        self.assertEqual(batches, [[(17, False), (32, False)], [(32, True), (17, True)]])
        self.assertFalse(pressed)

    def test_hold_failure_still_releases_injected_keys(self):
        primary, secondary, reader = Target(), Target(), Reader('pasted')
        pressed = set()
        def send(events):
            for key, up in events:
                if up: pressed.discard(key)
                else: pressed.add(key)
            reader.phase, reader.operation_id = ('recording', 7) if pressed else ('idle', None)
        api = SimpleNamespace(send=send, key_down=lambda key: key in pressed, idle_seconds=lambda: 2)
        with patch.object(module, 'ConsoleKeyGuard', return_value=contextlib.nullcontext()), \
             patch.object(module, 'observed_playback', side_effect=module.EvidenceError('playback failed')):
            with self.assertRaises(module.EvidenceError) as raised:
                module.run_scenario('dictate', api=api, primary=primary, secondary=secondary,
                    reader=reader, player=None, item={'audio_path': 'unused', 'reference': 'public'},
                    hotkey=(17, 32), clock=Clock(), recording_mode='hold')
        self.assertFalse(pressed)
        self.assertTrue(raised.exception.hold_abort['hold_released_on_abort'])
        self.assertEqual(raised.exception.hold_abort['original_exception'], 'EvidenceError')

    def test_hold_guards_keep_chord_until_focus_selection_or_overlay_cancel(self):
        class NativeScratch(Target, module.NativeTarget):
            def change_held_selection(self):
                self.selection_changed = True

        chord = (17, 16, 0x78)
        for scenario, cancel_state in [('focus-away', None), ('focus-away', 'lost-focus'), ('selection-change', None),
                                       ('cancel', 'cancelled'), ('cancel', 'processing'),
                                       ('cancel', 'timeout')]:
            with self.subTest(scenario=scenario, cancel_state=cancel_state):
                primary, secondary = NativeScratch(module.SEED), Target()
                primary.selection_changed = False
                reader = Reader('cancelled' if scenario == 'cancel' else 'clipboard_only')
                pressed, batches = set(), []
                observed_cancel = False
                def focus_secondary():
                    primary.focused, secondary.focused = False, True
                secondary.focus = focus_secondary

                def send(events):
                    batches.append(events)
                    for key, up in events:
                        (pressed.discard if up else pressed.add)(key)
                    if pressed:
                        reader.phase, reader.operation_id = 'recording', 7
                    else:
                        if scenario == 'focus-away':
                            self.assertTrue(secondary.focused and not primary.focused)
                            if cancel_state == 'lost-focus':
                                secondary.focused = False
                        if scenario == 'selection-change':
                            self.assertTrue(primary.selection_changed)
                        if scenario == 'cancel' and cancel_state != 'timeout':
                            self.assertTrue(observed_cancel)
                        reader.phase, reader.operation_id = 'idle', None

                original_wait = reader.wait
                def wait(predicate, timeout, check=lambda: None):
                    nonlocal observed_cancel
                    if timeout == 8:
                        self.assertEqual(pressed, set(chord))
                        if cancel_state == 'timeout':
                            raise module.EvidenceError('timed out')
                        if cancel_state == 'processing':
                            reader.events.append({'operation_id': 7, 'phase': 'processing'})
                        reader.phase, reader.operation_id = 'idle', None
                        observed_cancel = True
                    original_wait(predicate, timeout, check)
                reader.wait = wait
                api = SimpleNamespace(send=send, key_down=pressed.__contains__, idle_seconds=lambda: 2)
                with patch.object(module, 'ConsoleKeyGuard', return_value=contextlib.nullcontext()), \
                     patch.object(module, 'observed_playback', return_value=(1, 3, 2000)), \
                     patch.object(module, 'tap') as tap:
                    def run():
                        return module.run_scenario(scenario, api=api, primary=primary, secondary=secondary,
                            reader=reader, player=None, item={'audio_path': 'unused', 'reference': 'public'},
                            hotkey=chord, clock=Clock(), guard_seconds=.1, recording_mode='hold')
                    if cancel_state in ('timeout', 'lost-focus'):
                        with self.assertRaises(module.EvidenceError) as raised:
                            run()
                        self.assertEqual(raised.exception.hold_abort['hold_released_on_abort'], cancel_state == 'timeout')
                    else:
                        result, _, _ = run()
                        self.assertEqual(result['status'], 'BLOCKED' if cancel_state == 'processing' else 'PASS')
                        if cancel_state:
                            self.assertEqual(result['requested_cancel_method'], 'operator_overlay')
                            self.assertFalse(result['cancel_source_verified'])
                    tap.assert_not_called()
                self.assertEqual(batches, [[(key, False) for key in chord],
                                           [(key, True) for key in reversed(chord)]])
                self.assertFalse(pressed)

    def test_hold_selection_requires_an_observed_native_range_change(self):
        for initial, final, accepted in [(9 | (9 << 16), len(module.SEED) << 16, True),
                                         (len(module.SEED) << 16, len(module.SEED) << 16, False),
                                         (9 | (9 << 16), 9 | (9 << 16), False)]:
            with self.subTest(initial=initial, final=final):
                selections = iter((initial, final))
                calls = []
                def message(hwnd, command, start, end, flags, timeout, result):
                    calls.append((hwnd, command, start, end))
                    if command == 0xB0:
                        result._obj.value = next(selections)
                    return 1
                target = module.NativeTarget.__new__(module.NativeTarget)
                target.api = SimpleNamespace(user=SimpleNamespace(SendMessageTimeoutW=message))
                target.window = SimpleNamespace(edit=42)
                target.focus_process = lambda: None
                if accepted:
                    target.change_held_selection()
                else:
                    with self.assertRaises(module.UnsupportedTarget):
                        target.change_held_selection()
                self.assertEqual(calls, [(42, 0xB0, 0, 0), (42, 0xB1, 0, -1), (42, 0xB0, 0, 0)])

    def test_processing_cancel_classification_does_not_hide_missing_escape_or_wrong_receiver(self):
        cases = [(False, 'cancelled', '', 'BLOCKED'),
                 (True, 'cancelled', 'wrong receiver', 'FAIL'),
                 (True, 'clipboard_only', '', 'FAIL'),
                 (True, 'failed', '', 'FAIL'),
                 (False, 'pasted', 'wrong receiver', 'FAIL')]
        for attempted, outcome, secondary_text, expected in cases:
            with self.subTest(attempted=attempted, outcome=outcome, secondary=secondary_text):
                primary, secondary, reader = Target(module.SEED), Target(), Reader(outcome)
                calls = []
                def tap(api, target, chord):
                    calls.append(chord)
                    if len(calls) == 1:
                        reader.phase, reader.operation_id = 'recording', 7
                    elif len(calls) == 2:
                        reader.phase = 'processing' if attempted else 'idle'
                    else:
                        reader.phase = 'idle'
                    if reader.phase == 'idle': secondary.content = secondary_text
                with patch.object(module, 'tap', side_effect=tap), \
                     patch.object(module, 'observed_playback', return_value=(1, 3, 2000)):
                    result, _, _ = module.run_scenario('cancel-processing', api=None, primary=primary,
                        secondary=secondary, reader=reader, player=None,
                        item={'audio_path': 'unused', 'reference': 'public'}, hotkey=(17, 32),
                        clock=Clock(), guard_seconds=.1)
                self.assertEqual(result['status'], expected)

    def test_hold_operator_input_aborts_and_releases(self):
        primary, secondary, reader, clock = Target(), Target(), Reader('pasted'), Clock()
        pressed = set()
        def send(events):
            for key, up in events:
                if up: pressed.discard(key)
                else: pressed.add(key)
            reader.phase, reader.operation_id = ('recording', 7) if pressed else ('idle', None)
        api = SimpleNamespace(send=send, key_down=lambda key: key in pressed, idle_seconds=lambda: 0)
        def playback(*args, **kwargs):
            clock.now += 1
            args[-1]()
            self.fail('physical input must abort')
        with patch.object(module, 'ConsoleKeyGuard', return_value=contextlib.nullcontext()), \
             patch.object(module, 'observed_playback', side_effect=playback):
            with self.assertRaises(module.EvidenceError) as rejected:
                module.run_scenario('dictate', api=api, primary=primary, secondary=secondary,
                    reader=reader, player=None, item={'audio_path': 'unused', 'reference': 'public'},
                    hotkey=(17, 32), clock=clock, recording_mode='hold')
        self.assertEqual(rejected.exception.hold_abort['hold_abort_cause'], 'input_activity_during_hold')
        self.assertFalse(pressed)

    def test_processing_cancel_waits_for_processing_before_escape(self):
        primary, secondary, reader = Target(module.SEED), Target(), Reader('cancelled')
        calls = []
        def tap(api, target, chord):
            calls.append(chord)
            if len(calls) == 1: reader.phase, reader.operation_id = 'recording', 7
            elif chord == (17, 32): reader.phase = 'processing'
            else:
                self.assertEqual(reader.phase, 'processing')
                reader.phase = 'idle'
        with patch.object(module, 'tap', side_effect=tap), \
             patch.object(module, 'observed_playback', return_value=(1, 3, 2000)):
            result, _, _ = module.run_scenario('cancel-processing', api=None, primary=primary,
                secondary=secondary, reader=reader, player=None,
                item={'audio_path': 'unused', 'reference': 'public'}, hotkey=(17, 32),
                clock=Clock(), guard_seconds=.1)
        self.assertEqual(result['status'], 'PASS')
        self.assertTrue(result['processing_cancel_attempted'])
        self.assertEqual(calls, [(17, 32), (17, 32), (27,)])

    def test_missed_processing_window_is_blocked_without_late_escape(self):
        primary, secondary, reader = Target(module.SEED), Target(), Reader('pasted')
        calls = []
        def tap(api, target, chord):
            calls.append(chord)
            reader.phase, reader.operation_id = ('recording', 7) if len(calls) == 1 else ('idle', None)
            if len(calls) == 2: primary.content += 'public'
        with patch.object(module, 'tap', side_effect=tap), \
             patch.object(module, 'observed_playback', return_value=(1, 3, 2000)):
            result, _, _ = module.run_scenario('cancel-processing', api=None, primary=primary,
                secondary=secondary, reader=reader, player=None,
                item={'audio_path': 'unused', 'reference': 'public'}, hotkey=(17, 32),
                clock=Clock(), guard_seconds=.1)
        self.assertEqual(result['status'], 'BLOCKED')
        self.assertEqual(result['reason'], 'processing_window_missed')
        self.assertFalse(result['processing_cancel_attempted'])
        self.assertEqual(calls, [(17, 32), (17, 32)])

    def test_external_bridge_stops_on_error_without_fallback_input(self):
        bridge = SimpleNamespace(call=lambda *args: {'focused': False})
        target = module.ExternalTarget(bridge, 'primary')
        with self.assertRaises(module.UnsupportedTarget): target.focus()
        self.assertFalse(target.refocus())
        def unavailable(*args): raise module.UnsupportedTarget('fixture closed')
        bridge.call = unavailable
        self.assertEqual(target.focus_process(), 'unavailable')

    def test_external_trailing_newline_is_not_hidden(self):
        bridge = SimpleNamespace(call=lambda *args: {'text': module.SEED + '\n'})
        target = module.ExternalTarget(bridge, 'primary')
        self.assertEqual(module.judge('cancel', target.read(), '', {'outcome': 'cancelled'}, '', .5)['status'], 'FAIL')

    def test_setup_does_not_replace_unknown_text(self):
        target = Target('private unsaved work')
        with patch.object(module, 'tap') as tap:
            with self.assertRaises(module.EvidenceError): module.prepare(None, target, '', '')
            tap.assert_not_called()

    def test_partial_unicode_input_releases_without_inserting_more_characters(self):
        calls = []
        def send(count, inputs, size):
            calls.append([inputs[index].ki.dwFlags for index in range(count)])
            return count - 1 if len(calls) == 1 else count
        target = Target(); target.focus()
        with self.assertRaises(module.EvidenceError):
            module.unicode_text(SimpleNamespace(user=SimpleNamespace(SendInput=send)), target, 'ab')
        self.assertEqual(calls, [[4, 6, 4, 6], [6, 6]])

    def test_profile_is_read_only_and_rejects_cloud_or_wrong_microphone(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'profile.sqlite3'
            settings = {'asrBackend': 'transformers-asr', 'microphoneId': 'virtual-test',
                        'textCorrectionEnabled': False, 'automaticVoiceMode': False, 'liveTargetInsertion': False,
                        'holdToTalk': {'dictate': False}, 'shortcuts': {'dictate': ['Ctrl+Shift+Space']},
                        'apiKeyEnvVar': 'PRIVATE_SENTINEL'}
            with contextlib.closing(sqlite3.connect(path)) as db, db:
                db.execute('CREATE TABLE settings (key TEXT, value TEXT)')
                db.execute("INSERT INTO settings VALUES ('app_settings', ?)", (json.dumps(settings),))
            result = module.profile_settings(path, 'virtual-test')
            self.assertEqual(result['asrBackend'], 'transformers-asr')
            self.assertNotIn('apiKeyEnvVar', result)
            with self.assertRaises(module.EvidenceError): module.profile_settings(path, 'virtual-test', 'hold')
            settings['holdToTalk']['dictate'] = True
            with contextlib.closing(sqlite3.connect(path)) as db, db:
                db.execute('UPDATE settings SET value=?', (json.dumps(settings),))
            self.assertTrue(module.profile_settings(path, 'virtual-test', 'hold')['holdToTalkDictate'])
            with self.assertRaises(module.EvidenceError): module.profile_settings(path, 'virtual-test')
            settings['holdToTalk']['dictate'] = False
            settings.pop('liveTargetInsertion')
            with contextlib.closing(sqlite3.connect(path)) as db, db:
                db.execute('UPDATE settings SET value=?', (json.dumps(settings),))
            with self.assertRaises(module.EvidenceError): module.profile_settings(path, 'virtual-test')
            settings['liveTargetInsertion'] = False
            with self.assertRaises(module.EvidenceError): module.profile_settings(path, 'other-microphone')
            settings['asrBackend'] = 'openai-compatible'
            with contextlib.closing(sqlite3.connect(path)) as db, db:
                db.execute('UPDATE settings SET value=?', (json.dumps(settings),))
            with self.assertRaises(module.EvidenceError): module.profile_settings(path, 'virtual-test')


if __name__ == '__main__': unittest.main()
