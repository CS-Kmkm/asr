"""Receiver-only observation is not clipboard or stop/paste confirmation."""
from pathlib import Path
import json
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'scripts'))
try:
    import bench_e2e_passive as module
    from send_evaluation_command import send_command
finally:
    sys.path.pop(0)


class PassiveTests(unittest.TestCase):
    def test_no_output_and_no_stop_evidence_remain_missing(self):
        result = module.observation_result([], 1, 3, 'public', True)
        self.assertEqual(result['status'], 'no_output')
        self.assertIsNone(result['playback_end_to_first_change_ms'])
        self.assertIsNone(result['app_stop_to_confirmed_delivery_ms'])
        self.assertIsNone(result['clipboard_restoration_verified'])

    def test_receiver_change_only_has_replay_end_timing(self):
        event = SimpleNamespace(at=3.2, text='public', focus_process=None)
        result = module.observation_result([event], 1, 3, 'public', True)
        self.assertEqual(result['status'], 'observed_output')
        self.assertAlmostEqual(result['playback_end_to_first_change_ms'], 200)
        self.assertEqual(result['normalized_cer'], 0)
        self.assertNotIn('text', result)

    def test_output_after_freeze_invalidates_both_score_and_timing(self):
        event = SimpleNamespace(at=3.2, text='partial', focus_process=None)
        result = module.observation_result([event], 1, 3, 'public', True, receiver_changed=True)
        self.assertEqual(result['status'], 'invalid_late_output')
        self.assertIsNone(result['normalized_cer'])
        self.assertIsNone(result['playback_end_to_first_change_ms'])

    def test_early_output_or_focus_loss_never_enters_latency_results(self):
        for at, focus, focused in [(2, None, True), (3.2, 'other', True), (3.2, None, False)]:
            with self.subTest(at=at, focus=focus):
                event = SimpleNamespace(at=at, text='public', focus_process=focus)
                result = module.observation_result([event], 1, 3, 'public', focused)
                self.assertIsNone(result['playback_end_to_first_change_ms'])

    def test_unverified_delivery_is_never_confirmed(self):
        event = SimpleNamespace(at=3.2, text='public', focus_process=None)
        take = {'outcome': 'paste_unverified', 'spans_ms': {'stop_to_delivery': 100}}
        result = module.observation_result([event], 1, 3, 'public', True, take)
        self.assertIsNone(result['app_stop_to_confirmed_delivery_ms'])

    def test_receiver_never_freezes_before_app_completion_and_stability(self):
        trial = {'audio_end': 0, 'operation': 7}
        reader = SimpleNamespace(completed=lambda _: False)
        change = SimpleNamespace(at=20, text='preview')
        self.assertFalse(module.observation_ready(trial, 61, [change], reader))
        self.assertNotIn('completion_seen_at', trial)
        with self.assertRaises(module.EvidenceError): module.observation_ready(trial, 301, [change], reader)
        reader.completed = lambda _: True
        self.assertFalse(module.observation_ready(trial, 302, [change], reader))
        self.assertTrue(module.observation_ready(trial, 303, [change], reader))

    def test_duplicate_play_and_inflight_reset_are_rejected(self):
        state = module.TrialState(); state.arm(1); state.play()
        for action in (state.play, lambda: state.reset_idle(True), state.complete, lambda: state.arm(2)):
            with self.assertRaises(module.EvidenceError): action()
        self.assertFalse(state.audio_done(999))
        self.assertTrue(state.audio_active)
        state.audio_done(1); state.freeze(); state.complete(); state.arm(2)
        self.assertFalse(state.audio_done(1))

    def test_malformed_commands_do_not_become_actions(self):
        for command in ([], {}, {'sequence': True, 'action': 'arm'},
                        {'sequence': 1, 'action': 'send_keys'}, {'sequence': -1, 'action': 'play'},
                        {'sequence': 1, 'action': 'arm', 'item_id': []}):
            with self.assertRaises(ValueError): module.validate_command(command)

    def test_status_contention_does_not_terminate_observation(self):
        stage = SimpleNamespace(write_text=lambda *a, **kw: None)
        with patch.object(module.os, 'replace', side_effect=PermissionError):
            self.assertFalse(module.publish_status(stage, 'unused', {'phase': 'armed'}))

    def test_delayed_controller_preserves_sequence_and_waits_before_send(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            (directory / 'status.json').write_text(json.dumps({'schema': 'asr-passive-status/2'}), encoding='utf-8')
            clock = SimpleNamespace(sleep=lambda delay: self.assertFalse((directory / 'command.json').exists()))
            send_command(directory, {'sequence': 2, 'action': 'exit'}, 5, clock)
            before = (directory / 'command.json').read_text(encoding='utf-8')
            with self.assertRaises(ValueError):
                send_command(directory, {'sequence': 1, 'action': 'exit'}, 0, SimpleNamespace(sleep=lambda _: None))
            self.assertEqual((directory / 'command.json').read_text(encoding='utf-8'), before)

    def test_native_ui_timeout_cancels_late_callback(self):
        from bench_e2e import TargetWindow, AbortRun
        window = TargetWindow(SimpleNamespace(), foreground_mode='operator')
        window.message = lambda *args, **kwargs: (_ for _ in ()).throw(AbortRun('timeout'))
        called = []
        with self.assertRaises(AbortRun): window.call_on_ui(lambda: called.append(True))
        future, callback = window.ui_jobs.get_nowait()
        if future.set_running_or_notify_cancel(): callback()
        self.assertEqual(called, [])


if __name__ == '__main__': unittest.main()
