from __future__ import annotations

import json
import sys
import tempfile
import time
import unittest
import wave
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[2] / "scripts"
sys.path.insert(0, str(SCRIPTS))
import bench_streaming as streaming
import score_asr
sys.path.pop(0)


class StreamingTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.audio = self.root / "sample.wav"
        with wave.open(str(self.audio), "wb") as output:
            output.setparams((1, 2, 16000, 0, "NONE", "not compressed"))
            output.writeframes(b"\0\0" * 4000)  # 250 ms; short final chunk.
        self.command = [sys.executable, str(SCRIPTS / "bench_streaming.py"), "--mock-adapter"]

    def test_realtime_chunks_events_flush_and_final_scoring(self):
        with streaming.StreamSession(self.command, timeout_s=5) as session:
            record = streaming.replay(session, self.audio, "ja-JP", "test", 100)
            # The same adapter can reset for a second utterance.
            second = streaming.replay(session, self.audio, "ja-JP", "next", 125)
        self.assertEqual([c["frames"] for c in record["chunks"]], [1600, 1600, 800])
        self.assertEqual([c["offset_frames"] for c in record["chunks"]], [0, 1600, 3200])
        self.assertTrue(all(c["sent_ms"] >= c["source_end_ms"] - 0.5 for c in record["chunks"]))
        self.assertGreaterEqual(record["first_partial_ms"], 99)
        self.assertGreaterEqual(record["first_stable_ms"], 99)
        self.assertGreaterEqual(record["replay_rtf"], 1)
        self.assertGreaterEqual(record["flush_to_final_ms"], 0)
        self.assertEqual(record["stable_retractions"], 0)
        self.assertEqual(second["text"], "mock")
        opts = score_asr.parse_args([])
        opts._terms = []
        scored = score_asr._manifest_metrics("mock", record["text"], "ja-JP", score_asr.Options(opts))
        self.assertEqual(scored["raw"]["cer"]["cer"], 0)
        self.assertIsNone(scored["raw"]["wer"])

    def adapter(self, body):
        path = self.root / "adapter.py"
        path.write_text(body, encoding="utf-8")
        return [sys.executable, str(path)]

    def test_adapter_timeout_is_bounded(self):
        command = self.adapter("import time\ntime.sleep(30)\n")
        started = time.perf_counter()
        with self.assertRaisesRegex(streaming.BenchError, "timed out"):
            with streaming.StreamSession(command, timeout_s=0.1) as session:
                streaming.replay(session, self.audio, "ja-JP", "test", 100)
        self.assertLess(time.perf_counter() - started, 5)

    def test_bad_stream_identity_or_event_is_rejected(self):
        for event in ({"event": "ready", "stream_id": "wrong"}, ["invalid"]):
            command = self.adapter("import json,sys\nsys.stdin.readline()\nprint(" + repr(json.dumps(event)) + ",flush=True)\n")
            with self.assertRaises(streaming.BenchError):
                with streaming.StreamSession(command, timeout_s=2) as session:
                    streaming.replay(session, self.audio, "ja-JP", "test", 100)

    def test_slow_adapter_preserves_source_clock_and_reports_delay(self):
        command = self.adapter('''import json,sys,time
for line in sys.stdin:
 r=json.loads(line); e={'stream_id':r['stream_id']}
 if r['command']=='start':
  e.update(event='ready', **{k:'test' for k in ('backend','runtime','model','model_revision','quantization','device','compute_type')})
 elif r['command']=='audio':
  time.sleep(0.15); e.update(event='partial',text='test')
 else: e.update(event='final',text='test')
 print(json.dumps(e),flush=True)
''')
        with streaming.StreamSession(command, timeout_s=5) as session:
            record = streaming.replay(session, self.audio, "ja-JP", "test", 100)
        self.assertGreater(record["flush_to_final_ms"], 200)
        self.assertEqual([c["source_end_ms"] for c in record["chunks"]], [100, 200, 250])

    def test_cli_manifest_and_existing_output(self):
        path = self.root / "manifest.json"
        path.write_text(json.dumps({"schema":"asr-bench-manifest/1", "name":"synthetic", "selection":{}, "sources":[],
                                    "items":[{"id":"sample", "audio_path":"sample.wav", "reference":"mock", "language":"ja-JP",
                                              "source":"synthetic", "split":"test", "duration":0.25}]}), encoding="utf-8")
        output = self.root / "result.json"
        flags = ["--manifest", str(path), "--output", str(output), "--adapter-cmd", *self.command]
        self.assertEqual(streaming.main(flags), 0)
        original = output.read_bytes()
        self.assertEqual(json.loads(original)["config"]["mode"], "streaming-realtime")
        opts = score_asr.parse_args([])
        opts._terms = []
        report = score_asr.score_manifest(json.loads(original), score_asr.load_manifest(path), path, score_asr.Options(opts))
        self.assertEqual(report["overall"]["raw"]["cer"]["cer"], 0)
        self.assertEqual(report["streaming"]["first_partial_ms"]["count"], 1)
        markdown = score_asr.render_manifest_markdown(report)
        self.assertIn("Real-time streaming", markdown)
        self.assertIn("first_partial_ms", markdown)
        self.assertEqual(streaming.main(flags), 1)
        self.assertEqual(output.read_bytes(), original)

    def test_partial_receipt_is_timestamped_during_replay_not_at_flush(self):
        with wave.open(str(self.audio), "wb") as output:
            output.setparams((1, 2, 16000, 0, "NONE", "not compressed"))
            output.writeframes(b"\0\0" * 16000)
        with streaming.StreamSession(self.command, timeout_s=5) as session:
            record = streaming.replay(session, self.audio, "ja-JP", "test", 100)
        self.assertLess(record["first_partial_ms"], 700)
        self.assertGreaterEqual(record["wall_ms"], 1000)

    def test_failed_replay_does_not_publish_result_and_can_be_retried(self):
        manifest = self.root / "manifest.json"
        manifest.write_text(json.dumps({"schema":"asr-bench-manifest/1", "name":"synthetic", "sources":[],
                                       "items":[{"id":"sample", "audio_path":"sample.wav", "reference":"mock",
                                                 "language":"ja-JP", "source":"synthetic"}]}), encoding="utf-8")
        output = self.root / "retry.json"
        bad = self.adapter("import sys\nsys.stdin.readline()\nprint('bad json',flush=True)\n")
        flags = ["--manifest", str(manifest), "--output", str(output), "--adapter-cmd"]
        self.assertEqual(streaming.main([*flags, *bad]), 1)
        self.assertFalse(output.exists())
        self.assertEqual(streaming.main([*flags, *self.command]), 0)
        self.assertEqual(json.loads(output.read_text(encoding="utf-8"))["config"]["mode"], "streaming-realtime")


if __name__ == "__main__":
    unittest.main()
