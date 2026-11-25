"""Run native product insertion and guard checks in the ASR-Eval guest."""
from __future__ import annotations

import argparse
import array
from contextlib import closing
import ctypes
from ctypes import wintypes
import hashlib
import json
import math
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time
import uuid
import wave

from evaluation_events import EventReader, EvidenceError, SourceUnavailable
from score_asr import Options, _manifest_metrics, load_manifest, parse_args as score_args
from windows_acceptance import (
    MOCK_EXTERNAL, MOCK_INSERTION, MOCK_NOTEPAD, MOCK_TEXT, REAL_EXTERNAL, REAL_EXTERNAL_REPEATS,
    REAL_NOTEPAD, REAL_RECEIVERS, VM_FULL_SCENARIOS, VM_SMOKE_SCENARIOS, WORKER_RECOVERY, WORKER_RECOVERY_TRIALS,
    delivery_digest, live_process_start, read_real_proof, require_evaluation_guest, worker_identity,
)

TONE_HZ = 400
REAL_MODEL = Path(r'C:\ASR-Evaluation\models\faster-whisper-0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf')
REAL_DEPENDENCIES = Path(r'C:\ASR-Evaluation\runtime\real-deps\site-packages')


def contain_guest_processes():
    """Kill the app and acceptance children if their controller exits abnormally."""
    class BasicLimits(ctypes.Structure):
        _fields_ = [('process_time', ctypes.c_int64), ('job_time', ctypes.c_int64),
                    ('flags', wintypes.DWORD), ('min_working', ctypes.c_size_t),
                    ('max_working', ctypes.c_size_t), ('active', wintypes.DWORD),
                    ('affinity', ctypes.c_size_t), ('priority', wintypes.DWORD),
                    ('scheduling', wintypes.DWORD)]

    class ExtendedLimits(ctypes.Structure):
        _fields_ = [('basic', BasicLimits), ('io', ctypes.c_uint64 * 6),
                    ('process_memory', ctypes.c_size_t), ('job_memory', ctypes.c_size_t),
                    ('peak_process_memory', ctypes.c_size_t), ('peak_job_memory', ctypes.c_size_t)]

    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    kernel.CreateJobObjectW.restype = wintypes.HANDLE
    kernel.GetCurrentProcess.restype = wintypes.HANDLE
    kernel.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    handle = kernel.CreateJobObjectW(None, None)
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    limits = ExtendedLimits()
    limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    if not (kernel.SetInformationJobObject(handle, 9, ctypes.byref(limits), ctypes.sizeof(limits))
            and kernel.AssignProcessToJobObject(handle, kernel.GetCurrentProcess())):
        error = ctypes.get_last_error()
        kernel.CloseHandle(handle)
        raise ctypes.WinError(error)
    # The kernel closes this non-inheritable handle when the controller exits.


def fixture(root):
    item_id = 'mock-insertion-tone'
    audio = root / f'{item_id}.wav'
    sample_rate = 16000
    samples = array.array('h', (int(9000 * math.sin(2 * math.pi * TONE_HZ * i / sample_rate))
                                for i in range(2 * sample_rate)))
    with wave.open(str(audio), 'wb') as output:
        output.setparams((1, 2, sample_rate, 0, 'NONE', 'not compressed'))
        output.writeframes(samples.tobytes())
    manifest = root / 'manifest.json'
    manifest.write_text(json.dumps({
        'schema': 'asr-bench-manifest/1', 'name': item_id, 'sources': [],
        'items': [{'id': item_id, 'audio_path': audio.name, 'reference': MOCK_TEXT,
                   'source': 'synthetic', 'language': 'ja-JP',
                   'audio_sha256': hashlib.sha256(audio.read_bytes()).hexdigest()}],
    }), encoding='utf-8')
    return manifest, item_id


def capture_evidence(audio_path):
    with wave.open(str(audio_path), 'rb') as source:
        rate = source.getframerate()
        if source.getsampwidth() != 2 or source.getnchannels() != 1 or rate < 8000:
            raise EvidenceError('unexpected captured PCM format')
        samples = array.array('h', source.readframes(rate * 10))
    size = rate // 50
    phase = [2 * math.pi * TONE_HZ * i / rate for i in range(size)]
    sine, cosine = [math.sin(x) for x in phase], [math.cos(x) for x in phase]
    matching = 0
    for start in range(0, len(samples) - size + 1, size):
        window = samples[start:start + size]
        energy = sum(x * x for x in window)
        real = sum(x * basis for x, basis in zip(window, cosine))
        imag = sum(x * basis for x, basis in zip(window, sine))
        magnitude = math.hypot(real, imag)
        if energy and magnitude > .9 * math.sqrt(energy * size / 2) and 2 * magnitude / size > 400:
            matching += 1
    return {'sample_rate': rate, 'samples': len(samples), 'tone_hz': TONE_HZ,
            'matching_20ms_windows': matching, 'passed': matching >= 25}


def trace_transcriptions(lines, path):
    for line in lines:
        try:
            request = json.loads(line)
        except ValueError:
            request = None  # The product's protocol reader still handles the malformed line.
        if isinstance(request, dict) and request.get('command', request.get('op')) == 'transcribe':
            with path.open('a', encoding='ascii') as log:
                log.write('transcribe\n')
        yield line


def speech_capture_evidence(input_path, recorded_path):
    import numpy as np

    def envelope(path):
        with wave.open(str(path), 'rb') as source:
            rate = source.getframerate()
            frames = source.getnframes()
            if source.getsampwidth() != 2 or source.getnchannels() != 1 or rate < 8000 or frames > rate * 30:
                raise EvidenceError('unexpected speech capture PCM format or duration')
            samples = np.frombuffer(source.readframes(frames), dtype='<i2').astype(float)
        window = round(rate / 100)
        samples = samples[:len(samples) // window * window].reshape(-1, window)
        return np.sqrt(np.mean(samples * samples, axis=1)), rate, frames

    reference, _, _ = envelope(input_path)
    recorded, rate, frames = envelope(recorded_path)
    # Allow audio buffering and resampling while rejecting unrelated or silent captures.
    crop = 25  # Ignore edges where playback and recording callbacks can meet.
    reference = reference[crop:-crop]
    if not len(reference):
        raise EvidenceError('speech fixture is too short for capture verification')
    reference -= reference.mean()
    ref_norm = np.linalg.norm(reference)
    best = 0.0
    if ref_norm:
        for offset in range(min(len(recorded) - len(reference) + 1, 300)):
            sample = recorded[offset:offset + len(reference)]
            sample = sample - sample.mean()
            sample_norm = np.linalg.norm(sample)
            if sample_norm:
                best = max(best, float(np.dot(reference, sample) / (ref_norm * sample_norm)))
    return {'sample_rate': rate, 'captured_frames': frames,
            'rms_envelope_correlation': best, 'capture_matches_fixture': best >= .8}


def probed_worker():
    """Record aggregate signal evidence from the actual WAV sent to the fixture worker."""
    from asr_worker.backends import FasterWhisperBackend, MockBackend
    from asr_worker import worker

    real_manifest = os.environ.get('ASR_EVAL_REAL_REFERENCE')
    backend = FasterWhisperBackend if real_manifest else MockBackend
    original = backend.transcribe
    read_lines = worker._stdin_lines
    proof_path = Path(os.environ['ASR_EVAL_CAPTURE_PROOF'])
    repeats = int(os.environ.get('ASR_EVAL_REAL_REPEATS', '1'))
    if real_manifest:
        original_load = FasterWhisperBackend.load
        load_count = 0
        transcription_count = 0

        def load(self, quantization):
            nonlocal load_count
            load_count += 1
            original_load(self, quantization)
            self.evaluation_load_id = uuid.uuid4().hex

        FasterWhisperBackend.load = load

    def real_transcribe(self, audio_path, *args, **kwargs):
        from importlib.metadata import version

        nonlocal transcription_count
        transcription_count += 1
        text, segments = original(self, audio_path, *args, **kwargs)
        manifest = load_manifest(Path(real_manifest))
        item, = manifest['items']
        capture = speech_capture_evidence(item['audio_path'], audio_path)
        options = score_args([])
        options._terms = []
        scores = _manifest_metrics(item['reference'], text, item['language'], Options(options))
        destination = trial_directory(proof_path.parent, transcription_count, repeats) / proof_path.name
        temporary = destination.with_suffix('.tmp')
        with temporary.open('x', encoding='utf-8') as report:
            json.dump({
                'backend': 'faster-whisper', 'device': self.device, 'compute_type': self.compute_type,
                'faster_whisper_version': version('faster-whisper'),
                'ctranslate2_version': version('ctranslate2'),
                'worker_pid': os.getpid(), 'model_load_id': self.evaluation_load_id,
                'model_load_count': load_count, 'transcription_index': transcription_count,
                'delivery_sha256': delivery_digest(text), **capture,
                'cer_raw': scores['raw']['cer'], 'cer_normalized': scores['normalized']['cer'],
            }, report, allow_nan=False)
        temporary.rename(destination)
        return text, segments

    def transcribe(self, audio_path, *args, **kwargs):
        marker = os.environ.get('ASR_WORKER_MOCK_CRASH_FILE')
        destination = proof_path
        identity = {}
        if marker:
            index = 1 if Path(marker).is_file() else 2
            destination = trial_directory(proof_path.parent, index, 2) / proof_path.name
            identity = {'worker_pid': os.getpid(), 'worker_start_time': live_process_start(os.getpid())}
        with destination.open('x', encoding='utf-8') as report:
            try:
                json.dump({'effective_backend': 'mock', **identity, **capture_evidence(audio_path)}, report)
            except (EvidenceError, OSError, wave.Error):
                json.dump({'passed': False, 'reason': 'captured_audio_unreadable'}, report)
                raise
        return original(self, audio_path, *args, **kwargs)

    backend.transcribe = real_transcribe if real_manifest else transcribe
    request_log = Path(os.environ['ASR_EVAL_REQUEST_LOG'])
    worker._stdin_lines = lambda: trace_transcriptions(read_lines(), request_log)
    if os.environ.get('ASR_EVAL_MOCK_WORKER') == '1':
        require_evaluation_guest()
        if real_manifest or os.environ.get('USERNAME', '').casefold() != 'asrevalreal':
            raise EvidenceError('unexpected staged mock worker mode')
        return worker.main(['--backend', 'mock'])
    return worker.main()


def stage_capture_probe(root):
    entry = root / 'asr_worker' / '__main__.py'
    entry.write_text(
        "from pathlib import Path\n"
        "import sys\n"
        "sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))\n"
        "from vm_insertion_smoke import probed_worker\n"
        "raise SystemExit(probed_worker())\n",
        encoding='utf-8',
    )


def prepare_profile(*, real=False):
    if os.environ.get('USERNAME', '').casefold() != ('asrevalreal' if real else 'asreval'):
        raise EvidenceError('expected the dedicated evaluation user')
    profile = Path(os.environ['APPDATA']) / 'org.localvoice.input' / 'local-voice-input.sqlite3'
    config = {
        'asrEvaluationProfile': 'vm-insertion-smoke/1', 'setupComplete': True,
        'asrBackend': 'mock', 'offlineOnly': True, 'modelId': None,
        'microphoneId': 'name:CABLE Output (VB-Audio Virtual Cable)',
        'shortcuts': {'dictate': ['Ctrl+Shift+Space'], 'translate': [], 'ask': [], 'edit': []},
        'holdToTalk': {'dictate': False}, 'automaticVoiceMode': False,
        'textCorrectionEnabled': False, 'liveTargetInsertion': False,
        'livePreviewEnabled': False, 'incrementalFinalAsr': False, 'statefulStreaming': False,
        'smartDictateEdit': False, 'historyRetention': 'never', 'keepFailedTakes': False,
        'deleteAudioAfterProcessing': True, 'autoStart': False, 'clipboardRestore': True,
        'noiseSuppression': 'off', 'automaticGain': False, 'inputGainPercent': 100,
    }
    if real:
        config.update(asrEvaluationProfile='vm-real-notepad/1', asrBackend='faster-whisper',
                      asrDevice='cpu', speechLocale='ja-JP', modelQuantization='4bit',
                      modelId=str(REAL_MODEL))
    if profile.exists():
        with closing(sqlite3.connect(profile.as_uri() + '?mode=ro', uri=True)) as db:
            row = db.execute("SELECT value FROM settings WHERE key='app_settings'").fetchone()
        stored = json.loads(row[0]) if row else {}
        if not isinstance(stored, dict) or any(stored.get(key) != value for key, value in config.items()):
            raise EvidenceError('existing profile is not the expected evaluation profile')
        return profile
    profile.parent.mkdir(parents=True, exist_ok=True)
    profile.touch(exist_ok=False)
    with closing(sqlite3.connect(profile)) as db, db:
        db.execute('CREATE TABLE settings (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL, updated_at TEXT NOT NULL)')
        db.execute("INSERT INTO settings VALUES('app_settings', ?, 'evaluation')", (json.dumps(config),))
    return profile


def wait_for_app(process, events):
    reader = EventReader(events)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise EvidenceError(f'evaluation app exited during startup ({process.returncode})')
        try:
            reader.poll(require_fresh=False)
        except SourceUnavailable:
            time.sleep(.1)
            continue
        if reader.phase == 'idle':
            if reader.pid != process.pid:
                raise EvidenceError('evaluation telemetry came from an unexpected process')
            return reader
        time.sleep(.1)
    raise EvidenceError('evaluation app did not become ready')


def apply_capture_proof(case, proof, requests):
    case['transcription_requests'] = requests
    if proof is not None:
        case['capture_proof'] = proof
    if case['status'] != 'PASS':
        return
    if case['scenario'] == 'cancel':
        if requests or proof is not None:
            case.update(status='FAIL', reason='transcription_observed_after_recording_cancel')
    elif not requests or proof is None:
        case.update(status='BLOCKED', reason='no_independent_evidence_of_captured_audio')
    elif proof.get('passed') is not True:
        case.update(status='FAIL', reason='fixture_tone_missing_from_captured_audio')


def trial_directory(case_dir, index, repeats):
    return case_dir / f'trial-{index:02}' if repeats > 1 else case_dir


def run_acceptance(root, scenario, profile, manifest, item, exe_hash, events, directory, index, *, mock_on_real_profile=False):
    real = scenario in REAL_RECEIVERS
    recovery = scenario == WORKER_RECOVERY
    command = [
        sys.executable, '-X', 'utf8', str(root / 'scripts' / 'windows_acceptance.py'),
        '--manifest', str(manifest), '--item-id', item, '--events', str(events),
        '--profile-db', str(profile), '--output-dir', str(directory / 'acceptance'),
    ]
    if real:
        command.extend(('--real-case', scenario, '--worker-proof', str(directory / 'capture-proof.json')))
    else:
        command.append('--mock-insertion')
        if mock_on_real_profile:
            command.append('--mock-on-real-profile')
        if recovery:
            command.append('--worker-recovery')
        elif scenario != MOCK_INSERTION:
            command.extend(('--scenario', scenario))
    with (directory / 'acceptance.log').open('xb') as log:
        timeout = 160 if real else 100 if recovery else 65 if scenario in MOCK_EXTERNAL else 50
        code = subprocess.run(command, timeout=timeout, check=False,
                              stdout=log, stderr=subprocess.STDOUT).returncode
    report_path = directory / 'acceptance' / 'acceptance.json'
    if not report_path.is_file():
        raise EvidenceError(f'runner exited without an acceptance report ({code})')
    acceptance = json.loads(report_path.read_text(encoding='utf-8'))
    rows = acceptance['results']
    requested = list(WORKER_RECOVERY_TRIALS) if recovery else [scenario]
    if (acceptance['requested_scenarios'] != requested or not 1 <= len(rows) <= len(requested)
            or (code == 0) != (len(rows) == len(requested) and all(row['status'] == 'PASS' for row in rows))):
        raise EvidenceError('inconsistent acceptance results')
    if acceptance['runtime']['application_binary_sha256'] != exe_hash:
        raise EvidenceError('the running app does not match the staged executable')
    trials = []
    for offset, row in enumerate(rows):
        case_name = requested[offset]
        if row['scenario'] not in (case_name, 'setup'):
            raise EvidenceError('acceptance reported an unexpected scenario')
        trial = {'scenario': case_name, 'index': index + offset, 'status': row['status'],
                 'acceptance': {**acceptance, 'results': [row]}}
        if 'reason' in row:
            trial['reason'] = row['reason']
        trials.append(trial)
    return trials


def check_real_trial(trial, reader, process, proof, requests, index, previous):
    trial.update(transcription_requests=len(requests), real_worker_proof=proof,
                 application_pid=reader.pid)
    if trial['status'] != 'PASS':
        return
    operation = trial['acceptance']['results'][0]['operation_id']
    if (process.poll() is not None or reader.pid != process.pid or reader.phase != 'idle'
            or list(reader.takes) != [*previous, operation] or operation in previous):
        raise EvidenceError('application operations did not match the sequential trials')
    if (len(requests) != 1 or not proof or proof.get('transcription_index') != index
            or proof.get('model_load_count') != 1 or not proof.get('model_load_id')
            or not proof.get('worker_pid')):
        trial.update(status='FAIL', reason='unexpected_real_worker_request_or_model_load')
    elif proof.get('capture_matches_fixture') is not True:
        trial.update(status='FAIL', reason='recorded_signal_does_not_match_fixture')


def check_worker_recovery_trial(trial, trials, reader, process, requests):
    if trial['status'] != 'PASS':
        return
    reader.poll()
    operations = [take['acceptance']['results'][0]['operation_id'] for take in trials]
    if (process.poll() is not None or reader.pid != process.pid or reader.phase != 'idle'
            or list(reader.takes)[:len(operations)] != operations or len(set(operations)) != len(operations)):
        raise EvidenceError('worker recovery trial lost application operation continuity')
    pid, start = worker_identity(trial['capture_proof'])
    if len(trials) == 1:
        verified = trial['acceptance']['results'][0].get('worker_exit_before_recovery') is True
    else:
        verified = (live_process_start(pid) == start
                    and (pid, start) != worker_identity(trials[0]['capture_proof']))
        trial['recovered_worker_live'] = verified
    if len(requests) != 1 or not verified:
        trial.update(status='FAIL', reason='worker_exit_or_restart_not_verified')
    else:
        trial['application_pid'] = process.pid


def run_case(root, scenario, profile, manifest, item, exe_hash, *, repeats=1, mock_on_real_profile=False):
    real = scenario in REAL_RECEIVERS
    case_dir = root / 'cases' / scenario
    case_dir.mkdir(parents=True)
    events = case_dir / 'events.jsonl'
    capture = case_dir / 'capture-proof.json'
    request_log = case_dir / 'transcription-requests.log'
    request_log.touch(exist_ok=False)
    case = {'scenario': scenario, 'status': 'BLOCKED'}
    app = root / 'local-voice-input.exe'
    env = {key: value for key, value in os.environ.items() if not key.upper().startswith('ASR_')}
    env.update(ASR_EVAL_EVENTS=str(events), ASR_PYTHON=sys.executable,
               ASR_WORKER_MOCK_TEXT=MOCK_TEXT, ASR_EVAL_CAPTURE_PROOF=str(capture),
               ASR_EVAL_REQUEST_LOG=str(request_log))
    if mock_on_real_profile:
        env['ASR_EVAL_MOCK_WORKER'] = '1'
    recovery = scenario == WORKER_RECOVERY
    if recovery:
        repeats = len(WORKER_RECOVERY_TRIALS)
        marker = case_dir / 'crash-once'
        marker.touch(exist_ok=False)
        env['ASR_WORKER_MOCK_CRASH_FILE'] = str(marker)
    if real:
        if not REAL_DEPENDENCIES.is_dir() or not REAL_MODEL.is_dir():
            raise EvidenceError('provisioned real dependencies or model missing')
        env.update(PYTHONPATH=str(REAL_DEPENDENCIES),
                   ASR_EVAL_REAL_REFERENCE=str(manifest), HF_HUB_OFFLINE='1',
                   ASR_EVAL_REAL_REPEATS=str(repeats))
    trials = []
    operations = []
    worker_identity = None
    request_offset = 0
    if repeats > 1:
        case.update(requested_trials=repeats, trials=trials)
    with (case_dir / 'app.log').open('xb') as log:
        process = subprocess.Popen([str(app)], cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            reader = wait_for_app(process, events)
            if recovery:
                for index in range(1, repeats + 1):
                    trial_directory(case_dir, index, repeats).mkdir()
                recovery_trials = run_acceptance(root, scenario, profile, manifest, item, exe_hash, events,
                                                 case_dir, 1, mock_on_real_profile=mock_on_real_profile)
            for index in range(1, (len(recovery_trials) if recovery else repeats) + 1):
                directory = trial_directory(case_dir, index, repeats)
                if repeats > 1 and not recovery:
                    directory.mkdir()
                trial = recovery_trials[index - 1] if recovery else run_acceptance(
                    root, scenario, profile, manifest, item, exe_hash, events, directory, index,
                    mock_on_real_profile=mock_on_real_profile)[0]
                trials.append(trial)
                all_requests = request_log.read_text(encoding='ascii').splitlines()
                if any(request != 'transcribe' for request in all_requests):
                    raise EvidenceError('unexpected worker request trace')
                requests = all_requests[request_offset:index] if recovery else all_requests[request_offset:]
                request_offset = index if recovery else len(all_requests)
                if real:
                    reader.poll()
                    proof = read_real_proof(directory / capture.name)
                    check_real_trial(trial, reader, process, proof, requests, index, operations)
                    if trial['status'] == 'PASS':
                        identity = (proof['worker_pid'], proof['model_load_id'])
                        if worker_identity not in (None, identity):
                            trial.update(status='FAIL', reason='worker_or_loaded_model_changed')
                        worker_identity = identity
                        operations.append(trial['acceptance']['results'][0]['operation_id'])
                else:
                    proof_path = directory / capture.name
                    proof = json.loads(proof_path.read_text(encoding='utf-8')) if proof_path.is_file() else None
                    apply_capture_proof(trial, proof, len(requests))
                    if (trial['status'] == 'PASS' and scenario != 'cancel'
                            and proof.get('effective_backend') != 'mock'):
                        trial.update(status='BLOCKED', reason='mock_backend_evidence_missing')
                    if recovery:
                        check_worker_recovery_trial(trial, trials, reader, process, requests)
                        if (trial['status'] == 'PASS' and index == repeats
                                and (len(all_requests) != repeats or len(reader.takes) != repeats)):
                            trial.update(status='FAIL', reason='unexpected_requests_during_worker_recovery')
                if repeats == 1:
                    case.update(trial)
                else:
                    case['status'] = trial['status']
                    if 'reason' in trial:
                        case['reason'] = trial['reason']
                if trial['status'] != 'PASS':
                    break
        except (EvidenceError, OSError, sqlite3.Error, ValueError, subprocess.SubprocessError) as exc:
            case.update(status='BLOCKED', reason=str(exc))
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
    return case


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--revision', required=True)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument('--include-notepad', action='store_true')
    modes.add_argument('--expanded-matrix', action='store_true')
    modes.add_argument('--real-notepad', action='store_true')
    modes.add_argument('--real-apps', action='store_true')
    parser.add_argument('--real-repeats', type=int, default=1)
    parser.add_argument('--mock-on-real-profile', action='store_true')
    args = parser.parse_args()
    real = args.real_notepad or args.real_apps
    if not 1 <= args.real_repeats <= 80 or (args.real_repeats != 1 and not real):
        parser.error('--real-repeats accepts 1–80 and requires a real mode for multiple takes')
    if args.mock_on_real_profile and real:
        parser.error('--mock-on-real-profile is a mock-only mode')
    scenarios = (*VM_SMOKE_SCENARIOS, MOCK_NOTEPAD) if args.include_notepad else VM_SMOKE_SCENARIOS
    if args.expanded_matrix:
        scenarios = VM_FULL_SCENARIOS
    elif args.real_notepad:
        scenarios = (REAL_NOTEPAD,)
    elif args.real_apps:
        scenarios = tuple(REAL_RECEIVERS)
    root = Path(__file__).resolve().parents[1]
    output = root / 'result.json'
    if output.exists():
        raise FileExistsError('evaluation run already has a report')
    app = root / 'local-voice-input.exe'
    result = {'schema': 'asr-vm-insertion-smoke/2', 'app_source_revision': args.revision,
              'status': 'BLOCKED', 'cases': [], 'mock_on_real_profile': args.mock_on_real_profile}
    try:
        require_evaluation_guest()
        contain_guest_processes()
        result['exe_sha256'] = hashlib.sha256(app.read_bytes()).hexdigest()
        profile = prepare_profile(real=real or args.mock_on_real_profile)
        if real:
            manifest = root / 'real-fixture' / 'manifest.json'
            staged_item, = load_manifest(manifest)['items']
            item = staged_item['id']
        else:
            manifest, item = fixture(root)
        stage_capture_probe(root)
        result['entry_sha256'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
        result['worker_sha256'] = {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                                   for path in sorted((root / 'asr_worker').glob('*.py'))}
        for scenario in scenarios:
            repeats = REAL_EXTERNAL_REPEATS if scenario in REAL_EXTERNAL else args.real_repeats
            result['cases'].append(run_case(root, scenario, profile, manifest, item, result['exe_sha256'],
                                            repeats=repeats, mock_on_real_profile=args.mock_on_real_profile))
    except (EvidenceError, OSError, sqlite3.Error, ValueError, subprocess.SubprocessError) as exc:
        result['reason'] = str(exc)
        for scenario in scenarios[len(result['cases']):]:
            result['cases'].append({'scenario': scenario, 'status': 'BLOCKED', 'reason': 'suite_interrupted'})
    statuses = {case['status'] for case in result['cases']}
    result['status'] = 'FAIL' if 'FAIL' in statuses else 'BLOCKED' if 'BLOCKED' in statuses else 'PASS'
    with output.open('x', encoding='utf-8') as report:
        json.dump(result, report, ensure_ascii=False, indent=2)
    return {'PASS': 0, 'FAIL': 1}.get(result['status'], 2)


if __name__ == '__main__':
    raise SystemExit(main())
