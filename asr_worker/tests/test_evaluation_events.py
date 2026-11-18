"""Evaluation evidence must fail closed, independent of Windows UI availability."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

scripts = Path(__file__).resolve().parents[2] / 'scripts'
sys.path.insert(0, str(scripts))
try:
    from evaluation_events import EventReader, EvidenceError
finally:
    sys.path.pop(0)


def frame(sequence=0, phase='idle', operation=None, **extra):
    return dict(schema='asr-evaluation/1', pid=123, sequence=sequence,
                elapsed_ms=sequence * 10, healthy=True, heartbeat=False,
                kind='lifecycle', phase=phase, operation_id=operation,
                mode=None if operation is None else 'dictate', outcome=None,
                spans_ms=None, **extra)


class Clock:
    now = 0
    def perf_counter(self): return self.now
    def sleep(self, duration): self.now += duration


class EventReaderTests(unittest.TestCase):
    def reader(self):
        return EventReader(Path('unused'), Clock())

    def test_missing_recording_or_second_operation_cannot_claim_start(self):
        reader = self.reader()
        reader.accept(frame())
        reader.accept(frame(1, 'starting', 1))
        self.assertIsNone(reader.recording_after(0))
        reader.accept(frame(2, 'recording', 1))
        self.assertEqual(reader.recording_after(0), 1)
        self.assertIsNone(reader.recording_after(2))
        reader.accept(frame(3, 'starting', 2))
        with self.assertRaises(EvidenceError): reader.recording_after(0)

    def test_stale_file_cannot_become_ready_from_old_contents(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'events.jsonl'
            path.write_text(json.dumps(frame()) + '\n', encoding='utf-8')
            reader = EventReader(path, Clock())
            with self.assertRaises(EvidenceError): reader.ready(timeout=.2)

    def test_partial_write_is_not_a_frame_and_truncation_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'events.jsonl'
            first = json.dumps(frame()) + '\n'
            second = json.dumps(frame(1, 'starting', 1))
            path.write_text(first + second[:20], encoding='utf-8')
            reader = EventReader(path, Clock()); reader.poll()
            self.assertEqual(reader.sequence, 0)
            with path.open('a', encoding='utf-8') as output: output.write(second[20:] + '\n')
            reader.poll(); self.assertEqual(reader.phase, 'starting')
            path.write_text('', encoding='utf-8')
            with self.assertRaises(EvidenceError): reader.poll()

    def test_lost_events_wrong_process_unknown_codes_or_negative_duration_reject(self):
        for mutation in ({'sequence': 3}, {'pid': 456}, {'healthy': False},
                         {'phase': 'private phase'}, {'operation_id': True},
                         {'sequence': True}, {'elapsed_ms': -1}, {'extra': 'private'}):
            with self.subTest(mutation=mutation):
                reader = self.reader(); reader.accept(frame())
                value = frame(1, 'recording', 1); value.update(mutation)
                with self.assertRaises(EvidenceError): reader.accept(value)

    def test_heartbeat_does_not_duplicate_events(self):
        reader = self.reader(); value = frame()
        reader.accept(value)
        value.update(heartbeat=True, elapsed_ms=250)
        reader.accept(value)
        self.assertEqual(len(reader.events), 1)

    def test_shared_rust_fixture_supports_success_cancellation_and_interruption(self):
        path = scripts.parent / 'docs/reference/evaluation-events-v1.jsonl'
        frames = [json.loads(line) for line in path.read_text(encoding='utf-8').splitlines()]
        reader = self.reader()
        for value in frames[:6]: reader.accept(value)
        self.assertTrue(reader.completed(7))
        self.assertEqual(reader.takes[7]['spans_ms']['stop_to_delivery'], 20)
        reader.accept(frames[6])
        with self.assertRaises(EvidenceError): reader.completed(7)
        for value in frames[7:]: reader.accept(value)
        self.assertTrue(reader.completed(8))
        self.assertEqual(reader.takes[8]['outcome'], 'cancelled')

    def test_long_operator_pause_backlog_is_drained_in_chunks(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'events.jsonl'
            value = frame(); value['heartbeat'] = True
            path.write_text((json.dumps(value) + '\n') * 6000, encoding='utf-8')
            self.assertGreater(path.stat().st_size, 1024*1024)
            reader = EventReader(path, Clock()); reader.poll()
            self.assertEqual(len(reader.events), 1)
            with path.open('a', encoding='utf-8') as output:
                output.write(json.dumps(frame(1, 'starting', 7)) + '\n')
            reader.poll()
            self.assertEqual(reader.phase, 'starting')

    def test_ready_waits_for_first_complete_frame(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'events.jsonl'; path.touch()
            clock = Clock()
            def sleep(duration):
                clock.now += duration
                path.write_text(json.dumps(frame()) + '\n', encoding='utf-8')
            clock.sleep = sleep
            reader = EventReader(path, clock); reader.ready()
            self.assertEqual(reader.phase, 'idle')

    def test_invalid_frame_poison_cannot_be_skipped_by_reset(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'events.jsonl'
            path.write_text(json.dumps(frame()) + '\n' + json.dumps(frame(3, 'starting', 7)) + '\n', encoding='utf-8')
            reader = EventReader(path, Clock())
            with self.assertRaises(EvidenceError): reader.poll()
            self.assertTrue(reader.failed)
            with path.open('a', encoding='utf-8') as output:
                output.write(json.dumps(frame(1, 'starting', 7)) + '\n')
            with self.assertRaises(EvidenceError): reader.ready()


if __name__ == '__main__': unittest.main()
