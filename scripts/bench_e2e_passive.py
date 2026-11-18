"""Operator-controlled replay measurement without keyboard/clipboard monitoring.

Commands use command.json with strictly increasing positive sequence numbers.
Only the owned EDIT is observed. Optional application telemetry supplies internal
take timing; other products report replay-end observations, never stop latency.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import threading
import time

from bench_e2e import WinAPI, TargetWindow, WavePlayer, load_manifest
from evaluation_events import EventReader, EvidenceError
from score_asr import cer_stats


def publish_status(staging, destination, value):
    try:
        staging.write_text(json.dumps(value, ensure_ascii=False), encoding='utf-8')
        os.replace(staging, destination)
        return True
    except OSError:
        return False


def validate_command(command):
    if not isinstance(command, dict) or type(command.get('sequence')) is not int or command['sequence'] < 1:
        raise ValueError('positive integer command sequence required')
    if command.get('action') not in ('arm', 'play', 'finish', 'reset_idle', 'exit'):
        raise ValueError('unknown action')
    if command['action'] == 'arm' and (not isinstance(command.get('item_id'), str) or not command['item_id'] or len(command['item_id']) > 200):
        raise ValueError('arm requires a bounded string item_id')
    return command['sequence'], command['action']


class TrialState:
    def __init__(self):
        self.phase = 'idle'
        self.sequence = None
        self.audio_active = False
        self.audio_completed = False

    def arm(self, sequence):
        if self.phase not in ('idle', 'complete') or self.audio_active:
            raise EvidenceError('verify app idle and reset before rearming')
        self.sequence, self.phase = sequence, 'armed'
        self.audio_completed = False

    def play(self):
        if self.phase != 'armed' or self.audio_active or self.audio_completed:
            raise EvidenceError('play requires a newly armed trial and verified recording')
        self.phase, self.audio_active = 'playing', True

    def audio_done(self, sequence):
        if sequence != self.sequence:
            return False
        self.audio_active, self.audio_completed = False, True
        if self.phase == 'playing':
            self.phase = 'observing'
        return True

    def complete(self):
        if self.phase != 'review' or self.audio_active or not self.audio_completed:
            raise EvidenceError('wait for full playback and verify app idle before finishing')
        self.phase = 'complete'

    def freeze(self):
        if self.phase != 'observing' or self.audio_active or not self.audio_completed:
            raise EvidenceError('receiver observation is not complete')
        self.phase = 'review'

    def reset_idle(self, confirmed):
        if not confirmed or self.audio_active:
            raise EvidenceError('stop/verify app idle and wait for audio before reset')
        self.__init__()


def observation_result(changes, audio_start, audio_end, reference, focused, take=None, *, receiver_changed=False):
    """A receiver change is observable output, not proof of clipboard restore."""
    nonempty = [event for event in changes if event.text.strip()]
    after = [event for event in nonempty if event.at >= audio_end]
    invalid = not focused or any(event.focus_process is not None for event in changes)
    early = any(audio_start <= event.at < audio_end for event in nonempty)
    final = changes[-1].text if changes else ''
    result = {
        'status': 'invalid_focus' if invalid else 'early_output' if early else 'observed_output' if final.strip() else 'no_output',
        'playback_end_to_first_change_ms': (after[0].at - audio_end) * 1000 if after and not invalid and not early else None,
        'receiver_nonempty': bool(final.strip()),
        'normalized_cer': cer_stats(reference, final)['cer'],
        'app_outcome': take['outcome'] if take else None,
        'app_spans_ms': take['spans_ms'] if take else None,
        'app_stop_to_confirmed_delivery_ms': take['spans_ms']['stop_to_delivery'] if take and take['outcome'] == 'pasted' and final.strip() and not invalid and not early else None,
        'clipboard_restoration_verified': None,
    }
    if receiver_changed:
        result.update(status='invalid_late_output', normalized_cer=None,
                      playback_end_to_first_change_ms=None, app_stop_to_confirmed_delivery_ms=None)
    return result


def observation_ready(trial, now, changes, reader):
    settled_since = max(trial['audio_end'], changes[-1].at if changes else trial['audio_end'])
    if reader:
        if not reader.completed(trial['operation']):
            if now - trial['audio_end'] >= 300:
                raise EvidenceError('app completion timed out; verify idle before reset')
            return False
        settled_since = max(settled_since, trial.setdefault('completion_seen_at', now))
        return now - settled_since >= 1
    if not changes or not changes[-1].text.strip():
        return now - trial['audio_end'] >= 60
    return now - settled_since >= 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, required=True)
    parser.add_argument('--output-dir', type=Path, required=True)
    parser.add_argument('--events', type=Path, help='explicit ASR_EVAL_EVENTS file, for this app only')
    parser.add_argument('--playback-device', default='CABLE Input')
    parser.add_argument('--app-label', default='operator-selected product')
    args = parser.parse_args(argv)
    manifest, conditions = load_manifest(args.manifest)
    items = {item['id']: item for item in manifest['items']}
    args.output_dir.mkdir(parents=True, exist_ok=False)
    reader = EventReader(args.events) if args.events else None
    if reader:
        reader.ready()
        if reader.phase != 'idle':
            raise EvidenceError('app is not idle')
    api = WinAPI()
    player = WavePlayer(api, args.playback_device)
    state, last_command, trial, playback_thread = TrialState(), 0, None, None
    audio_events = queue.Queue()
    notice, focus_valid = None, True

    def publish():
        publish_status(args.output_dir / '.status-writing.json', args.output_dir / 'status.json',
                       {'schema': 'asr-passive-status/2', 'phase': state.phase,
                        'trial_sequence': state.sequence, 'notice': notice,
                        'pid': os.getpid(), 'heartbeat_unix': time.time()})

    with TargetWindow(api, foreground_mode='operator') as window:
        publish()
        try:
            while window.thread.is_alive():
                if trial and state.phase in ('playing', 'observing'):
                    focus_valid = focus_valid and window.focus_process() is None
                if reader:
                    try:
                        reader.poll()
                    except EvidenceError:
                        state.phase = 'desynced'
                        notice = ('Invalid telemetry; restart observer and app with a new events file'
                                  if reader.failed else 'Telemetry unavailable; stop/verify app idle before reset')
                while not audio_events.empty():
                    sequence, timing = audio_events.get_nowait()
                    if sequence == state.sequence:
                        state.audio_active = False
                        if timing is None:
                            state.phase, notice = 'desynced', 'Playback failed; verify app idle before reset'
                        elif state.audio_done(sequence):
                            trial['audio_start'], trial['audio_end'], trial['audio_duration_ms'] = timing
                if trial and state.phase == 'observing':
                    try:
                        now, changes = window.snapshot()
                        if observation_ready(trial, now, changes, reader):
                            trial['receiver_snapshot'] = changes
                            trial['take_snapshot'] = reader.takes[trial['operation']] if reader else None
                            state.freeze()
                    except EvidenceError:
                        state.phase, notice = 'desynced', 'Completion evidence invalid; verify app idle before reset'
                command_path = args.output_dir / 'command.json'
                if command_path.exists():
                    try:
                        if command_path.stat().st_size > 16384:
                            raise ValueError('oversized command')
                        command = json.loads(command_path.read_text(encoding='utf-8'))
                        sequence, action = validate_command(command)
                        if sequence > last_command:
                            last_command = sequence
                            if action == 'exit':
                                if state.phase not in ('idle', 'complete') or state.audio_active:
                                    raise EvidenceError('verify app idle and reset before exit')
                                break
                            if action == 'reset_idle':
                                if playback_thread and playback_thread.is_alive():
                                    raise EvidenceError('playback still active')
                                if reader:
                                    reader.ready()
                                    if reader.phase != 'idle':
                                        raise EvidenceError('app is not idle')
                                state.reset_idle(command.get('operator_confirmed_app_idle') is True)
                                trial = None
                            elif action == 'arm':
                                if state.phase not in ('idle', 'complete') or command.get('operator_confirmed_app_idle') is not True:
                                    raise EvidenceError('verify app idle before arm')
                                if window.focus_process() is not None:
                                    raise EvidenceError('focus the owned EDIT before arm')
                                item = items[command['item_id']]
                                if hashlib.sha256(Path(item['audio_path']).read_bytes()).hexdigest() != item['audio_sha256']:
                                    raise EvidenceError('public audio hash mismatch')
                                if reader:
                                    reader.ready()
                                    if reader.phase != 'idle':
                                        raise EvidenceError('app is not idle')
                                window.message(0x8000)
                                state.arm(sequence)
                                focus_valid = True
                                trial = {'item': item, 'baseline': reader.sequence if reader else None, 'operation': None}
                            elif action == 'play':
                                if not trial or command.get('operator_confirmed_app_recording') is not True or window.focus_process() is not None:
                                    raise EvidenceError('verify recording/permissions and target focus before play')
                                if reader:
                                    reader.poll()
                                    trial['operation'] = reader.recording_after(trial['baseline'])
                                    if trial['operation'] is None:
                                        raise EvidenceError('fresh recording not observed')
                                state.play()
                                focus_valid = True
                                def play_audio(seq=state.sequence, path=Path(trial['item']['audio_path'])):
                                    try:
                                        audio_events.put((seq, player.play(path)))
                                    except Exception:
                                        audio_events.put((seq, None))
                                playback_thread = threading.Thread(target=play_audio, daemon=True)
                                playback_thread.start()
                            elif action == 'finish':
                                if state.phase != 'review' or command.get('operator_confirmed_app_idle') is not True:
                                    raise EvidenceError('wait for audio and verify app idle before finish')
                                if reader:
                                    reader.ready()
                                if reader and not reader.completed(trial['operation']):
                                    raise EvidenceError('matching application completion not observed')
                                changes = trial['receiver_snapshot']
                                _, current_changes = window.snapshot()
                                result = observation_result(changes, trial['audio_start'], trial['audio_end'],
                                                            trial['item']['reference'], focus_valid,
                                                            trial['take_snapshot'], receiver_changed=current_changes != changes)
                                result.update(schema='asr-passive/2', item_id=trial['item']['id'],
                                              audio_sha256=trial['item']['audio_sha256'],
                                              audio_duration_ms=trial['audio_duration_ms'],
                                              receiver_settle_ms=1000,
                                              app_label=args.app_label, conditions=conditions,
                                              timing_scope='receiver observation from replay end; app spans are a separate clock')
                                with (args.output_dir / f'trial-{state.sequence}.json').open('x', encoding='utf-8') as output:
                                    json.dump(result, output, ensure_ascii=False, indent=2, allow_nan=False)
                                state.complete()
                            notice = None
                    except (ValueError, OSError, KeyError, EvidenceError):
                        notice = ('Invalid telemetry; restart observer and app with a new events file'
                                  if reader and reader.failed else 'Command rejected; inspect phase and use a new command sequence')
                publish()
                time.sleep(.05)
        finally:
            if playback_thread:
                playback_thread.join(timeout=1)
            if state.phase not in ('idle', 'complete'):
                state.phase, notice = 'desynced', 'Observer ended; stop/verify the app manually'
                publish()


if __name__ == '__main__':
    main()
