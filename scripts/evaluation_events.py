"""Read explicit content-free application evaluation telemetry; never control UI."""
from __future__ import annotations

import json
from pathlib import Path
import time


class EvidenceError(RuntimeError):
    pass


class SourceUnavailable(EvidenceError):
    """A temporarily unreadable source, without invalidating parsed evidence."""


class EventReader:
    def __init__(self, path: Path, clock=time):
        self.path, self.clock = path, clock
        self.offset = 0
        self.pending = b''
        self.sequence = -1
        self.pid = None
        self.phase = None
        self.operation_id = None
        self.events = []
        self.takes = {}
        self.last_received = None
        self.elapsed_ms = -1
        self.frames_received = 0
        self.failed = False

    def poll(self, *, require_fresh=True):
        if self.failed:
            raise EvidenceError('invalid telemetry; restart observer and app with a new events file')
        try:
            self._drain()
        except SourceUnavailable:
            raise
        except EvidenceError:
            self.failed = True
            raise
        if require_fresh and (self.last_received is None or self.clock.perf_counter() - self.last_received > 3):
            raise EvidenceError('evaluation heartbeat stale')

    def _drain(self):
        try:
            with self.path.open('rb') as source:
                snapshot_end = source.seek(0, 2)
                if snapshot_end < self.offset:
                    raise EvidenceError('evaluation file truncated')
                source.seek(self.offset)
                while self.offset < snapshot_end:
                    chunk = source.read(min(65536, snapshot_end - self.offset))
                    if not chunk:
                        raise EvidenceError('evaluation file changed during read')
                    self.offset += len(chunk)
                    lines = (self.pending + chunk).split(b'\n')
                    self.pending = lines.pop()
                    for line in lines:
                        if len(line) > 65536:
                            raise EvidenceError('oversized evaluation frame')
                        try:
                            frame = json.loads(line)
                        except (ValueError, UnicodeError) as exc:
                            raise EvidenceError('invalid evaluation frame') from exc
                        self.accept(frame)
                    if len(self.pending) > 65536:
                        raise EvidenceError('oversized evaluation frame')
        except OSError as exc:
            raise SourceUnavailable('evaluation file unavailable; start the instrumented app first') from exc

    def accept(self, frame):
        fields = {'schema', 'pid', 'sequence', 'elapsed_ms', 'healthy', 'heartbeat',
                  'kind', 'phase', 'operation_id', 'mode', 'outcome', 'spans_ms'}
        if not isinstance(frame, dict) or set(frame) != fields or frame['schema'] != 'asr-evaluation/1':
            raise EvidenceError('unsupported evaluation schema')
        if frame['healthy'] is not True or not isinstance(frame['heartbeat'], bool):
            raise EvidenceError('evaluation writer lost evidence')
        for field in ('pid', 'sequence', 'elapsed_ms'):
            if type(frame[field]) is not int or frame[field] < 0:
                raise EvidenceError('invalid evaluation counter')
        if frame['pid'] == 0 or self.pid not in (None, frame['pid']):
            raise EvidenceError('evaluation process changed')
        seq = frame['sequence']
        maximum = 1 if self.sequence == -1 else self.sequence + 1
        if frame['elapsed_ms'] < self.elapsed_ms or seq < self.sequence or seq > maximum:
            raise EvidenceError('evaluation sequence gap or clock reversal')
        if seq == self.sequence and not frame['heartbeat']:
            raise EvidenceError('duplicate evaluation event')
        op = frame['operation_id']
        if op is not None and (type(op) is not int or op < 1):
            raise EvidenceError('invalid operation identity')
        if frame['mode'] not in (None, 'dictate', 'translate', 'edit', 'ask', 'selected_text'):
            raise EvidenceError('unknown evaluation mode')
        if frame['kind'] == 'lifecycle':
            if frame['phase'] not in ('idle', 'starting', 'recording', 'processing'):
                raise EvidenceError('unknown lifecycle phase')
            if (frame['phase'] == 'idle') != (op is None):
                raise EvidenceError('lifecycle identity mismatch')
        elif frame['kind'] == 'take':
            if frame['outcome'] not in ('pasted', 'paste_unverified', 'clipboard_only',
                                        'insertion_failed', 'panel', 'search', 'failed', 'cancelled'):
                raise EvidenceError('unknown take outcome')
            spans = frame['spans_ms']
            allowed = {'record_ready', 'model_ensure', 'audio_stop', 'live_drain', 'asr',
                       'shortcut_release', 'route', 'plan', 'ai', 'first_output', 'settle',
                       'paste', 'helper_exit', 'history_save', 'completion_job',
                       'stop_to_final', 'stop_to_delivery', 'stop_to_next_take'}
            if not isinstance(spans, dict) or set(spans) != allowed or any(
                value is not None and (type(value) is not int or value < 0)
                for value in spans.values()
            ):
                raise EvidenceError('invalid take spans')
        else:
            raise EvidenceError('unknown evaluation event')
        if seq > self.sequence:
            self.events.append(frame)
            if len(self.events) > 4096:
                raise EvidenceError('evaluation session too long; restart with a new file')
            if frame['kind'] == 'lifecycle':
                self.phase, self.operation_id = frame['phase'], op
            elif op is not None:
                if op in self.takes:
                    raise EvidenceError('multiple summaries for one operation')
                self.takes[op] = frame
        self.sequence, self.pid = seq, frame['pid']
        self.elapsed_ms = frame['elapsed_ms']
        self.last_received = self.clock.perf_counter()
        self.frames_received += 1

    def ready(self, timeout=4):
        # Require an actual new heartbeat after attaching; old file contents do
        # not prove that this process is alive or still idle.
        self.poll(require_fresh=False)
        attached_frames = self.frames_received
        deadline = self.clock.perf_counter() + timeout
        while self.clock.perf_counter() < deadline:
            self.clock.sleep(.05)
            self.poll(require_fresh=False)
            if self.frames_received > attached_frames:
                return
        raise EvidenceError('no fresh application heartbeat')

    def wait(self, predicate, timeout=30, check=lambda: None):
        deadline = self.clock.perf_counter() + timeout
        while self.clock.perf_counter() < deadline:
            self.poll()
            check()
            if predicate(self):
                return
            self.clock.sleep(.02)
        raise EvidenceError('application state timed out; verify idle before restarting')

    def recording_after(self, sequence):
        starts = [e for e in self.events if e['sequence'] > sequence and
                  e['kind'] == 'lifecycle' and e['phase'] == 'starting']
        if len(starts) > 1:
            raise EvidenceError('multiple operations observed')
        if starts and self.phase == 'recording' and self.operation_id == starts[0]['operation_id']:
            return self.operation_id
        return None

    def completed(self, operation):
        started = next((event['sequence'] for event in self.events
                        if event['operation_id'] == operation), None)
        if started is None:
            return False
        if any(event['sequence'] > started and event['kind'] == 'lifecycle'
               and event['operation_id'] not in (None, operation) for event in self.events):
            raise EvidenceError('another operation interrupted the trial')
        return self.phase == 'idle' and operation in self.takes
