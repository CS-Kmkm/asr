"""Exercise acceptance orchestration without sending Windows input."""
from pathlib import Path
import contextlib
import io
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
    import vm_insertion_smoke as vm_smoke
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
        self.changes = []
    def focus(self): self.focused = True
    def focus_process(self): return None if self.focused else 'other'
    def read(self): return self.content
    def change_count(self): return len(self.changes)
    def changed_since(self, start, *permitted): return any(value not in permitted for value in self.changes[start:])
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
    def test_native_product_timing_requires_independent_receiver_and_reference_quality(self):
        reference = '入力の確認です。'
        for written, outcome, expected in ((reference, 'pasted', 'PASS'),
                                           ('全然違う内容', 'pasted', 'FAIL'),
                                           (reference, 'clipboard_only', 'FAIL'),
                                           ('', 'pasted', 'FAIL')):
            with self.subTest(written=written, outcome=outcome):
                clock = Clock()
                primary, secondary, reader = Target(), Target(), Reader(outcome)

                def tap(api, target, chord):
                    if reader.phase == 'idle':
                        reader.phase, reader.operation_id = 'recording', 7
                        return clock.perf_counter()
                    stopped = clock.perf_counter()
                    clock.sleep(.125)
                    primary.content, reader.phase = written, 'idle'
                    return stopped

                with patch.object(module, 'tap', side_effect=tap), \
                     patch.object(module, 'observed_playback', return_value=(1, 3, 2000)):
                    result, _, _ = module.run_scenario(
                        module.NATIVE_NOTEPAD, api=None, primary=primary, secondary=secondary,
                        reader=reader, player=None, item={'audio_path': 'unused', 'reference': reference},
                        hotkey=(17, 32), clock=clock, guard_seconds=.1, unattended=True)
                self.assertEqual(result['status'], expected)
                if expected == 'PASS':
                    self.assertEqual(result['stop_to_observed_insertion_ms'], 125)
                    self.assertEqual(result['reference_normalized_cer'], 0)

    def test_native_notepad_profile_is_isolated_from_the_real_model_baseline(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(
                'os.environ', {'APPDATA': directory, 'USERNAME': 'ASREvalReal'}):
            old_profile = vm_smoke.prepare_profile(real=True)
            old_bytes = old_profile.read_bytes()
            native_profile = vm_smoke.prepare_profile(real=True, native_model=module.NATIVE_MODEL)
            native_bytes = native_profile.read_bytes()
            haas = 'haas-ja-v1.0'
            haas_profile = vm_smoke.prepare_profile(real=True, native_model=haas)

            self.assertEqual(native_profile.parent.name, module.NATIVE_PROFILE)
            self.assertEqual(haas_profile.parent.name, 'org.localvoice.input.eval-haas')
            self.assertEqual(len({old_profile, native_profile, haas_profile}), 3)
            self.assertEqual(old_profile.read_bytes(), old_bytes)
            self.assertEqual(native_profile.read_bytes(), native_bytes)
            mic = 'name:CABLE Output (VB-Audio Virtual Cable)'
            self.assertEqual(module.profile_settings(native_profile, mic, native_model=module.NATIVE_MODEL)['modelId'],
                             module.NATIVE_MODEL)
            self.assertEqual(module.profile_settings(haas_profile, mic, native_model=haas)['modelId'], haas)
            for profile in (old_profile, native_profile):
                with self.subTest(profile=profile.parent.name), self.assertRaises(module.EvidenceError):
                    module.profile_settings(profile, mic, native_model=haas)

    def test_fixed_matrix_schedule_covers_each_condition_and_excludes_warmup(self):
        cases = []
        nonces = set()
        for target in module.NORMAL_MATRIX_TARGETS:
            warmup, *trials = vm_smoke.matrix_schedule(50)
            self.assertEqual(warmup['index'], 1)
            self.assertEqual(len(trials), 200)
            for row in (warmup, *trials):
                self.assertNotIn(row['nonce'], nonces)
                nonces.add(row['nonce'])
            for row in trials:
                row['status'] = 'PASS'
            cases.append({'scenario': target, 'warmup': warmup, 'trials': trials})
        cases[0]['trials'][-1]['status'] = 'FAIL'
        cases[-1]['trials'].pop()

        result = vm_smoke.matrix_summary(cases, 50)

        self.assertEqual((result['confirmed'], result['reported'], result['planned']), (998, 999, 1000))
        self.assertEqual(result['rate'], .998)
        self.assertLess(result['wilson_95'][0], result['rate'])
        self.assertGreater(result['wilson_95'][1], result['rate'])
        self.assertEqual(result['by_condition']['long']['confirmed'], 248)
        self.assertEqual(result['cells'][module.NORMAL_MATRIX_TARGETS[-1]]['long']['planned'], 50)

    def test_matrix_receiver_rejects_lost_prefix_stale_nonce_and_duplicate(self):
        text = module.matrix_text('multiline', 'a' * 16)
        expected = module.NORMAL_MATRIX_PREFIX + text
        self.assertEqual(len(module.matrix_text('long', 'b' * 16).encode('utf-16-le')) // 2, 1024)
        self.assertGreater(len(module.matrix_text('mixed', 'b' * 16).encode('utf-16-le')) // 2,
                           len(module.matrix_text('mixed', 'b' * 16)))
        for observed, decoy, status in (
                (expected.replace('\n', '\r\n'), '', 'PASS'),
                (expected.replace('\n', '\r'), '', 'PASS'),
                (text, '', 'FAIL'),
                (expected[:-16] + 'b' * 16, '', 'FAIL'),
                (expected + text, '', 'FAIL'),
                (expected, text, 'FAIL')):
            with self.subTest(observed=observed, decoy=decoy):
                result = module.judge('mock-terminal', observed, decoy, {'outcome': 'paste_unverified'},
                                      '', .5, mock_text=text, normal_matrix=True)
                self.assertEqual(result['status'], status)

    def test_real_notepad_matches_worker_output_independently_of_reference(self):
        expected_text = '認識した文章'
        for output, status in ((expected_text, 'PASS'), (expected_text * 2, 'FAIL'), ('', 'FAIL')):
            with self.subTest(status=status, output=output), tempfile.TemporaryDirectory() as directory:
                proof = Path(directory) / 'proof.json'
                clock = Clock()
                primary, secondary = Target(), Target()
                reader = Reader('pasted')

                def tap(api, target, chord):
                    if reader.phase == 'idle':
                        reader.phase, reader.operation_id = 'recording', 7
                        return clock.perf_counter()
                    dispatched = clock.perf_counter()
                    clock.sleep(.125)
                    proof.write_text(json.dumps({
                        'backend': 'faster-whisper', 'device': 'cpu', 'compute_type': 'int8',
                        'captured_frames': 16000, 'sample_rate': 16000,
                        'delivery_sha256': module.delivery_digest(expected_text),
                    }), encoding='utf-8')
                    primary.content, reader.phase = output, 'idle'
                    return dispatched

                with patch.object(module, 'tap', side_effect=tap), \
                     patch.object(module, 'observed_playback', return_value=(1, 3, 2000)):
                    row, _, _ = module.run_scenario(
                        module.REAL_NOTEPAD, api=None, primary=primary, secondary=secondary, reader=reader,
                        player=None, item={'audio_path': 'unused', 'reference': 'unrelated reference'},
                        hotkey=(17, 32), clock=clock, guard_seconds=.1, unattended=True, real_proof=proof)
                self.assertEqual(row['status'], status)
                self.assertEqual(row['stop_to_observed_insertion_ms'], 125 if status == 'PASS' else None)

    def test_owned_notepad_requires_exactly_one_fixture_phrase(self):
        text = module.MOCK_TEXT
        for primary, secondary, expected in ((text, '', 'PASS'), (text + text, '', 'FAIL'),
                                              (text + '\n', '', 'FAIL'), (text, text, 'FAIL')):
            with self.subTest(primary=primary, secondary=secondary):
                row = module.judge(module.MOCK_NOTEPAD, primary, secondary, {'outcome': 'pasted'}, 'unrelated', .5)
                self.assertEqual(row['status'], expected)

    def test_exact_receiver_proof_requires_the_app_to_attempt_delivery(self):
        for outcome, expected in (('pasted', 'PASS'), ('paste_unverified', 'PASS'),
                                  ('clipboard_only', 'FAIL'), ('cancelled', 'FAIL')):
            with self.subTest(outcome=outcome):
                row = module.judge('mock-terminal', module.MOCK_TEXT, '',
                                   {'outcome': outcome}, 'unrelated', .5)
                self.assertEqual(row['status'], expected)
                if expected == 'FAIL':
                    self.assertEqual(row['reason'], 'app_outcome_mismatch')

    def test_focus_loss_after_stop_has_a_distinct_reason(self):
        primary, secondary = Target(), Target()
        reader = Reader('pasted')

        def tap(api, target, chord):
            if reader.phase == 'idle':
                reader.phase, reader.operation_id = 'recording', 7
            else:
                reader.phase = 'idle'
                primary.focused = False

        with patch.object(module, 'tap', side_effect=tap), \
             patch.object(module, 'observed_playback', return_value=(1, 3, 2000)), \
             self.assertRaises(module.ScenarioEvidenceError) as error:
            module.run_scenario(
                'mock-edge', api=None, primary=primary, secondary=secondary, reader=reader,
                player=None, item={'audio_path': 'unused', 'reference': 'public'},
                hotkey=(17, 32), clock=Clock(), unattended=True)
        self.assertEqual(error.exception.code, 'target_changed_after_stop')

    def test_owned_notepad_is_not_available_in_the_legacy_runner(self):
        cli = ['--manifest', 'unused.json', '--item-id', 'unused', '--events', 'unused.jsonl',
               '--profile-db', 'unused.db', '--output-dir', 'unused', '--scenario', module.MOCK_NOTEPAD]
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit):
            module.main(cli)
        self.assertIn('requires --mock-insertion', stderr.getvalue())

    def test_vm_mock_rejects_text_from_a_previous_case(self):
        older = module.MOCK_TEXT + "0" * 16
        current = module.MOCK_TEXT + "1" * 16
        take = {"outcome": "pasted"}
        self.assertEqual(module.judge(module.MOCK_NOTEPAD, older, "", take, "", 0.5,
                                      mock_text=current)["status"], "FAIL")
        self.assertEqual(module.judge(module.MOCK_NOTEPAD, current, "", take, "", 0.5,
                                      mock_text=current)["status"], "PASS")

    def test_vm_guard_cases_detect_transient_edits_and_cancel_processing(self):
        for scenario, defect in (('focus-away', None), ('focus-away', 'transient'),
                                 ('cancel', None), ('cancel', 'processing')):
            with self.subTest(scenario=scenario, defect=defect):
                primary, secondary = Target(module.SEED), Target()
                reader = Reader('cancelled' if scenario == 'cancel' else 'clipboard_only')
                calls = []

                def tap(api, target, chord):
                    calls.append((target, chord))
                    if len(calls) == 1:
                        reader.phase, reader.operation_id = 'recording', 7
                    else:
                        reader.phase = 'idle'
                        if defect == 'transient':
                            secondary.changes.extend(('unexpected insertion', ''))
                        if defect == 'processing':
                            reader.events.append({'operation_id': 7, 'phase': 'processing'})

                with patch.object(module, 'tap', side_effect=tap), \
                     patch.object(module, 'observed_playback', return_value=(1, 3, 2000)):
                    row, _, _ = module.run_scenario(
                        scenario, api=None, primary=primary, secondary=secondary, reader=reader,
                        player=None, item={'audio_path': 'unused', 'reference': 'public'},
                        hotkey=(17, 32), clock=Clock(), guard_seconds=.1, unattended=True)
                self.assertEqual(row['status'], 'FAIL' if defect else 'PASS')
                self.assertEqual(calls[-1], (primary, (27,)) if scenario == 'cancel' else (secondary, (17, 32)))

    def test_vm_capture_proof_is_required_for_focus_away_and_forbidden_for_cancel(self):
        for scenario, initial, proof, requests, expected in (
            ('focus-away', 'PASS', None, 1, 'BLOCKED'),
            ('focus-away', 'PASS', {'passed': True}, 1, 'PASS'),
            ('cancel', 'PASS', None, 1, 'FAIL'),
            ('cancel', 'PASS', {'passed': False}, 0, 'FAIL'),
            ('cancel', 'PASS', None, 0, 'PASS'),
            ('cancel', 'BLOCKED', {'passed': True}, 1, 'BLOCKED'),
        ):
            with self.subTest(scenario=scenario, initial=initial, proof=proof, requests=requests):
                case = {'scenario': scenario, 'status': initial}
                vm_smoke.apply_capture_proof(case, proof, requests)
                self.assertEqual(case['status'], expected)

    def test_vm_request_trace_observes_protocol_input_before_validation(self):
        lines = ['{"command":"load"}', '{bad json', '{"command":"transcribe"}',
                 '{"op":"transcribe","audio_path":"private"}']
        with tempfile.TemporaryDirectory() as tmp:
            trace = Path(tmp) / 'trace.log'
            self.assertEqual(list(vm_smoke.trace_transcriptions(lines, trace)), lines)
            self.assertEqual(trace.read_text(encoding='ascii').splitlines(), ['transcribe', 'transcribe'])

    def test_vm_capture_evidence_distinguishes_the_fixture_from_silence(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, item = vm_smoke.fixture(root)
            self.assertTrue(vm_smoke.capture_evidence(root / f'{item}.wav')['passed'])
            silence = root / 'silence.wav'
            with wave.open(str(silence), 'wb') as output:
                output.setparams((1, 2, 16000, 0, 'NONE', 'not compressed'))
                output.writeframes(bytes(32000))
            self.assertFalse(vm_smoke.capture_evidence(silence)['passed'])

    def test_mock_insertion_requires_exact_text_at_the_caret(self):
        prefix, text, suffix = module.MOCK_PREFIX, module.MOCK_TEXT, module.MOCK_SUFFIX
        cases = (
            (prefix + text + suffix, '', 'PASS'),
            (prefix + text + text + suffix, '', 'FAIL'),
            (prefix + suffix + text, '', 'FAIL'),
            (text + suffix, '', 'FAIL'),
            (prefix + text + suffix, text, 'FAIL'),
        )
        for primary, secondary, expected in cases:
            with self.subTest(primary=primary, secondary=secondary):
                result = module.judge(module.MOCK_INSERTION, primary, secondary, {'outcome': 'pasted'},
                                      'different reference', .5)
                self.assertEqual(result['status'], expected)
                self.assertNotIn('normalized_cer', result)

    def test_mock_profile_requires_explicit_offline_mode(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'profile.sqlite3'
            settings = {'asrBackend': 'mock', 'microphoneId': 'virtual-test',
                        'textCorrectionEnabled': False, 'automaticVoiceMode': False, 'liveTargetInsertion': False,
                        'holdToTalk': {'dictate': False}, 'shortcuts': {'dictate': ['Ctrl+Shift+Space']}}
            with contextlib.closing(sqlite3.connect(path)) as db, db:
                db.execute('CREATE TABLE settings (key TEXT, value TEXT)')
                db.execute('INSERT INTO settings VALUES (?, ?)', ('app_settings', json.dumps(settings)))
            with self.assertRaises(module.EvidenceError):
                module.profile_settings(path, 'virtual-test', mock_insertion=True)
            settings['offlineOnly'] = True
            with contextlib.closing(sqlite3.connect(path)) as db, db:
                db.execute('UPDATE settings SET value=?', (json.dumps(settings),))
            self.assertEqual(module.profile_settings(path, 'virtual-test', mock_insertion=True)['asrBackend'], 'mock')
            with self.assertRaises(module.EvidenceError):
                module.profile_settings(path, 'virtual-test')

    def test_shared_mock_requires_a_pinned_evaluation_profile(self):
        settings = {
            'asrBackend': 'faster-whisper', 'asrDevice': 'cpu', 'offlineOnly': True,
            'microphoneId': 'virtual-test', 'textCorrectionEnabled': False,
            'automaticVoiceMode': False, 'liveTargetInsertion': False, 'livePreviewEnabled': False,
            'incrementalFinalAsr': False, 'statefulStreaming': False, 'smartDictateEdit': False,
            'holdToTalk': {'dictate': False}, 'shortcuts': {'dictate': ['Ctrl+Shift+Space']},
        }
        with tempfile.TemporaryDirectory() as tmp:
            database = Path(tmp) / 'profile.sqlite3'
            with contextlib.closing(sqlite3.connect(database)) as db, db:
                db.execute('CREATE TABLE settings (key TEXT, value TEXT)')
                db.execute('INSERT INTO settings VALUES (?, ?)', ('app_settings', json.dumps(settings)))
            with self.assertRaises(module.EvidenceError):
                module.profile_settings(database, 'virtual-test', mock_insertion=True, mock_on_real_profile=True)
            settings['asrEvaluationProfile'] = 'vm-real-notepad/1'
            with contextlib.closing(sqlite3.connect(database)) as db, db:
                db.execute('UPDATE settings SET value=?', (json.dumps(settings),))
            observed = module.profile_settings(database, 'virtual-test',
                                               mock_insertion=True, mock_on_real_profile=True)
            self.assertEqual(observed['asrBackend'], 'faster-whisper')

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

    def test_worker_crash_oracle_requires_refusal_and_both_receivers_unchanged(self):
        for primary, secondary, outcome, expected in (
            (module.SEED, '', 'failed', 'PASS'),
            (module.SEED + module.MOCK_TEXT, '', 'failed', 'FAIL'),
            (module.SEED, module.MOCK_TEXT, 'failed', 'FAIL'),
            (module.SEED, '', 'clipboard_only', 'FAIL'),
        ):
            with self.subTest(primary=primary, secondary=secondary, outcome=outcome):
                row = module.judge(module.WORKER_CRASH, primary, secondary, {'outcome': outcome}, '', .5)
                self.assertEqual(row['status'], expected)

    def test_worker_recovery_checks_liveness_and_allows_pid_reuse(self):
        def take(operation, created):
            return {'status': 'PASS', 'acceptance': {'results': [{
                        'operation_id': operation, 'worker_exit_before_recovery': True}]},
                    'capture_proof': {'worker_pid': 42, 'worker_start_time': created}}

        reader = SimpleNamespace(pid=123, phase='idle', takes={1: None}, poll=lambda: None)
        process = SimpleNamespace(pid=123, poll=lambda: None)
        first = take(1, 100)
        first['acceptance']['results'][0]['worker_exit_before_recovery'] = False
        vm_smoke.check_worker_recovery_trial(first, [first], reader, process, ['transcribe'])
        self.assertEqual(first['status'], 'FAIL')

        first, second = take(1, 100), take(2, 200)
        vm_smoke.check_worker_recovery_trial(first, [first], reader, process, ['transcribe'])
        self.assertEqual(first['status'], 'PASS')
        reader.takes[2] = None
        with patch.object(vm_smoke, 'live_process_start', return_value=200):
            vm_smoke.check_worker_recovery_trial(second, [first, second], reader, process, ['transcribe'])
        self.assertTrue(second['recovered_worker_live'])
        self.assertEqual(first['application_pid'], second['application_pid'])

    def test_worker_exit_evidence_requires_one_request_marker_consumption_and_worker_exit(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            (root / 'trial-01').mkdir()
            (root / 'trial-01' / 'capture-proof.json').write_text(
                '{"worker_pid": 42, "worker_start_time": 100}', encoding='utf-8')
            requests, marker = root / 'transcription-requests.log', root / 'crash-once'
            requests.write_text('transcribe\n', encoding='ascii')
            with patch.object(module, 'live_process_start', return_value=100):
                self.assertFalse(module.worker_exited_before_recovery(root))
            with patch.object(module, 'live_process_start', return_value=None) as exited:
                self.assertTrue(module.worker_exited_before_recovery(root))
                exited.assert_called_once_with(42, exit_wait_ms=2000)
                marker.touch()
                self.assertFalse(module.worker_exited_before_recovery(root))

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
        proof_exists = False
        wait = reader.wait

        def delayed_proof(predicate, timeout, check=lambda: None):
            nonlocal proof_exists
            if reader.phase == 'processing' and not proof_exists:
                self.assertFalse(predicate(reader))
                self.assertEqual(calls, [(17, 32), (17, 32)])
                proof_exists = True
            return wait(predicate, timeout, check)

        reader.wait = delayed_proof
        def tap(api, target, chord):
            calls.append(chord)
            if len(calls) == 1: reader.phase, reader.operation_id = 'recording', 7
            elif chord == (17, 32): reader.phase = 'processing'
            else:
                self.assertEqual(reader.phase, 'processing')
                self.assertTrue(proof_exists)
                reader.phase = 'idle'
        with patch.object(module, 'tap', side_effect=tap), \
             patch.object(module, 'observed_playback', return_value=(1, 3, 2000)):
            result, _, _ = module.run_scenario('cancel-processing', api=None, primary=primary,
                secondary=secondary, reader=reader, player=None,
                item={'audio_path': 'unused', 'reference': 'public'}, hotkey=(17, 32),
                clock=Clock(), guard_seconds=.1,
                inflight_proof=SimpleNamespace(is_file=lambda: proof_exists))
        self.assertEqual(result['status'], 'PASS')
        self.assertTrue(result['processing_cancel_attempted'])
        self.assertTrue(result['worker_hold_observed'])
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

    def test_held_processing_cancel_sends_no_escape_if_receiver_changes(self):
        primary, secondary, reader = Target(module.SEED), Target(), Reader('cancelled')
        calls = []

        def tap(api, target, chord):
            calls.append(chord)
            if len(calls) == 1:
                reader.phase, reader.operation_id = 'recording', 7
            else:
                reader.phase = 'processing'
                primary.content += 'intervening edit'

        with patch.object(module, 'tap', side_effect=tap), \
             patch.object(module, 'observed_playback', return_value=(1, 3, 2000)), \
             self.assertRaises(module.ScenarioEvidenceError):
            module.run_scenario('cancel-processing', api=None, primary=primary,
                secondary=secondary, reader=reader, player=None,
                item={'audio_path': 'unused', 'reference': 'public'}, hotkey=(17, 32),
                clock=Clock(), inflight_proof=SimpleNamespace(is_file=lambda: True))
        self.assertEqual(calls, [(17, 32), (17, 32)])

    def test_clipboard_guard_requires_exact_receiver_and_independent_clipboard_readback(self):
        initial = module.MOCK_PREFIX + module.MOCK_SUFFIX
        final = module.MOCK_PREFIX + module.MOCK_TEXT + module.MOCK_SUFFIX
        for scenario, preserved in (('clipboard-rich', False), ('clipboard-newer-during-asr', True)):
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                hold, proof = root / 'hold-transcribe', root / 'worker-hold-ready'
                primary, secondary, reader = Target(initial), Target(), Reader('pasted')
                primary.select_caret = primary.require_caret = lambda offset: self.assertEqual(offset, len(module.MOCK_PREFIX))
                calls = []

                def tap(api, target, chord):
                    calls.append(chord)
                    if len(calls) == 1:
                        reader.phase, reader.operation_id = 'recording', 7
                    elif scenario == 'clipboard-rich':
                        primary.content, reader.phase = final, 'idle'
                        primary.changes.append(final)
                    else:
                        hold.touch()
                        proof.touch()
                        reader.phase = 'processing'

                def copy_newer():
                    self.assertTrue(hold.is_file())
                    self.assertEqual(reader.phase, 'processing')
                    self.assertEqual(primary.content, initial)
                    calls.append('copy')

                wait = reader.wait

                def deliver_after_release(predicate, timeout, check=lambda: None):
                    if reader.phase == 'processing' and not hold.is_file():
                        primary.content, reader.phase = final, 'idle'
                        primary.changes.append(final)
                    return wait(predicate, timeout, check)

                reader.wait = deliver_after_release
                with patch.object(module, 'tap', side_effect=tap), \
                     patch.object(module, 'observed_playback', return_value=(1, 3, 2000)):
                    result, _, _ = module.run_scenario(scenario, api=None, primary=primary,
                        secondary=secondary, reader=reader, player=None,
                        item={'audio_path': 'unused', 'reference': 'public'}, hotkey=(17, 32),
                        clock=Clock(), guard_seconds=.1, unattended=True,
                        inflight_proof=proof if scenario in module.HELD_WORKER_GUARDS else None,
                        clipboard_fixture=SimpleNamespace(copy_newer=copy_newer, preserved=lambda: preserved))
                self.assertTrue(result['exact_receiver_match'])
                self.assertEqual(result['clipboard_preserved'], preserved)
                self.assertEqual(result['status'], 'PASS' if preserved else 'FAIL')
                self.assertEqual('copy' in calls, scenario in module.HELD_WORKER_GUARDS)
                self.assertFalse(hold.exists())

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
