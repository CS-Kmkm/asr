from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
import sys
import tarfile
import tempfile
import unittest
import wave
from collections import Counter
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

import av


_SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "bench_data.py"
_SPEC = importlib.util.spec_from_file_location("bench_data", _SCRIPT)
assert _SPEC is not None and _SPEC.loader is not None
bench_data = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = bench_data
_SPEC.loader.exec_module(bench_data)


def wav_bytes(rate: int = 16000, channels: int = 1, samples: int = 1600, value: int = 0) -> bytes:
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as output:
        output.setnchannels(channels)
        output.setsampwidth(2)
        output.setframerate(rate)
        output.writeframes(value.to_bytes(2, "little", signed=True) * samples * channels)
    return buffer.getvalue()


def flac_bytes(data: bytes) -> bytes:
    buffer = io.BytesIO()
    with av.open(io.BytesIO(data)) as source, av.open(buffer, "w", format="flac") as output:
        stream = output.add_stream("flac", rate=16000)
        stream.layout = "mono"
        for frame in source.decode(audio=0):
            for packet in stream.encode(frame):
                output.mux(packet)
        for packet in stream.encode(None):
            output.mux(packet)
    return buffer.getvalue()


def video_bytes() -> bytes:
    buffer = io.BytesIO()
    with av.open(buffer, "w", format="nut") as output:
        stream = output.add_stream("rawvideo", rate=1)
        stream.width = 2
        stream.height = 2
        stream.pix_fmt = "rgb24"
        frame = av.VideoFrame(2, 2, "rgb24")
        for plane in frame.planes:
            plane.update(bytes(plane.buffer_size))
        for packet in stream.encode(frame):
            output.mux(packet)
        for packet in stream.encode(None):
            output.mux(packet)
    return buffer.getvalue()


class Response(io.BytesIO):
    def __init__(self, body: bytes, size: int | None = None, *, content_length: bool = True) -> None:
        super().__init__(body)
        self.headers = {"Content-Length": str(len(body) if size is None else size)} if content_length else {}


def write_tar(path: Path, members: dict[str, bytes], compressed: bool = True) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(path, "w:gz" if compressed else "w") as archive:
        for name, data in members.items():
            member = tarfile.TarInfo(name)
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))


def record_cache(path: Path, url: str) -> None:
    bench_data.cache_record(path).write_text(
        json.dumps({"url": url, "sha256": bench_data.sha256_file(path), "size_bytes": path.stat().st_size}),
        encoding="utf-8",
    )


def fleurs_fixture(cache: Path, config: str = "ja_jp", groups: int = 24, compressed: bool = True) -> dict[str, bytes]:
    spec = bench_data.SOURCES[f"fleurs:{config}"]
    members = {}
    rows = []
    for group in range(groups):
        for recording in range(2):
            item_id = f"{group:03}{recording}"
            samples = 1600 + (group % 3) * 160
            members[f"test/{item_id}.wav"] = wav_bytes(samples=samples)
            rows.append(f"{group}\t{item_id}.wav\t 生の文 {group} ! \tnormalized\tchars\t{samples}\tfemale")
    write_tar(spec.archive(cache), members, compressed)
    record_cache(spec.archive(cache), spec.url)
    tsv = spec.directory(cache) / "test.tsv"
    tsv.write_text("\n".join(rows) + "\n", encoding="utf-8")
    record_cache(tsv, spec.tsv_url)
    return members


def librispeech_fixture(cache: Path) -> dict[str, bytes]:
    spec = bench_data.SOURCES["librispeech:test-clean"]
    members = {}
    encoded = {index: flac_bytes(wav_bytes(samples=1600 + index * 160)) for index in range(3)}
    for speaker in range(1, 21):
        prefix = f"LibriSpeech/test-clean/{speaker}/10"
        rows = []
        for utterance in range(6):
            item_id = f"{speaker}-10-{utterance:04}"
            members[f"{prefix}/{item_id}.flac"] = encoded[utterance % 3]
            rows.append(f"{item_id} RAW TEXT {speaker} {utterance}")
        members[f"{prefix}/{speaker}-10.trans.txt"] = "\n".join(rows).encode("utf-8")
    write_tar(spec.archive(cache), members)
    record_cache(spec.archive(cache), spec.url)
    return members


class CoreTests(unittest.TestCase):
    def test_tsv_preserves_raw_text_and_rejects_bad_rows(self) -> None:
        text = "123\t456.wav\t Raw, 文 ! \tnormal\tchars\t32000\tfemale\n"
        item = bench_data.parse_fleurs_tsv(text, "ja_jp")[0]
        self.assertEqual((item.item_id, item.group, item.reference, item.duration), ("456", "123", " Raw, 文 ! ", 2.0))
        self.assertEqual(item.id, "fleurs-ja_jp-test-456")
        for invalid in ("header\tbad\n", text + text, text.replace("32000", "oops"), text.replace("32000", "0"), text.replace("456.wav", "../456.wav")):
            with self.subTest(invalid=invalid), self.assertRaises(bench_data.DataError):
                bench_data.parse_fleurs_tsv(invalid, "ja_jp")

    def test_transcripts_preserve_case_and_punctuation(self) -> None:
        self.assertEqual(bench_data.parse_transcripts("1-2-3 RAW, TEXT!\n\n4-5-6 OTHER\n"), {"1-2-3": "RAW, TEXT!", "4-5-6": "OTHER"})
        for text in ("bad row", "1-2-3 \n", "1-2-3 A\n1-2-3 B"):
            with self.assertRaises(bench_data.DataError):
                bench_data.parse_transcripts(text)

    def test_pool_hash_and_group_deduplication_are_seed_independent(self) -> None:
        candidates = [bench_data.Candidate("fleurs", "ja_jp", f"{group}-{variant}", "unused", "raw", group + 1.0, str(group))
                      for group in range(60) for variant in (1, 0)]
        expected = "holdout" if int(hashlib.sha256(b"fleurs:17").hexdigest(), 16) % 100 < 50 else "dev"
        self.assertEqual(bench_data.assign_pool("fleurs", "17"), expected)
        deduped = bench_data.dedupe_groups(candidates)
        self.assertEqual(len(deduped), 60)
        self.assertTrue(all(item.item_id.endswith("-0") for item in deduped))
        dev = bench_data.select_items(candidates, 10, 3, "dev")
        holdout = bench_data.select_items(candidates, 10, 99, "holdout")
        self.assertFalse({item.group for item in dev} & {item.group for item in holdout})
        self.assertEqual(dev, bench_data.select_items(list(reversed(candidates)), 10, 3, "dev"))
        self.assertNotEqual(dev, bench_data.select_items(candidates, 10, 4, "dev"))
        self.assertEqual([item.id for item in dev], sorted(item.id for item in dev))
        with self.assertRaisesRegex(bench_data.DataError, "only"):
            bench_data.select_items(candidates, 999, 3, "dev")

    def test_duration_tertiles_and_speaker_balance(self) -> None:
        candidates = [bench_data.Candidate("fleurs", "ja_jp", str(index), "unused", "raw", index + 1.0, str(index)) for index in range(30)]
        ordered = bench_data.stratified_order(candidates, 9)
        self.assertEqual(Counter(int(item.duration - 1) // 10 for item in ordered[:12]), {0: 4, 1: 4, 2: 4})
        speakers = [bench_data.Candidate("librispeech", "test-clean", f"{speaker}-1-{index}", "unused", "raw", index + 1.0, str(speaker), str(speaker))
                    for speaker in range(4) for index in range(12)]
        selected = bench_data.select_items(speakers, 14, 10, "dev", 0, kind="librispeech")
        counts = Counter(item.speaker for item in selected)
        self.assertEqual(max(counts.values()) - min(counts.values()), 1)
        strata = Counter(int(item.duration - 1) // 4 for item in selected)
        self.assertLessEqual(max(strata.values()) - min(strata.values()), 1)
        self.assertEqual(selected, bench_data.select_items(list(reversed(speakers)), 14, 10, "dev", 0, kind="librispeech"))
        self.assertEqual(bench_data.stratified_order([], 1), [])

    def test_pool_percent_bounds_and_empty_reference_skip(self) -> None:
        for percent in (-1, 101):
            with self.assertRaisesRegex(bench_data.DataError, "holdout_percent"):
                bench_data.assign_pool("fleurs", "17", percent)
        self.assertEqual(bench_data.assign_pool("fleurs", "17", 0), "dev")
        self.assertEqual(bench_data.assign_pool("fleurs", "17", 100), "holdout")
        rows = ("1\tfirst.wav\t\tnormal\tchars\t1600\tfemale\n"
                "2\tsecond.wav\t  \tnormal\tchars\t1600\tfemale\n"
                "3\tthird.wav\t RAW \tnormal\tchars\t1600\tfemale\n")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            items = bench_data.parse_fleurs_tsv(rows, "ja_jp")
        self.assertEqual([item.item_id for item in items], ["third"])
        self.assertEqual(items[0].reference, " RAW ")
        self.assertIn("skipped 2 row(s) with empty raw_transcription", stderr.getvalue())
        with self.assertRaisesRegex(bench_data.DataError, "duplicate"):
            bench_data.parse_fleurs_tsv(rows + rows.splitlines()[0], "ja_jp")
        with self.assertRaisesRegex(bench_data.DataError, "7 columns"):
            bench_data.parse_fleurs_tsv("1\tfirst.wav\t\n", "ja_jp")

    def test_hash_order_matches_specification_and_ignores_input_order(self) -> None:
        items = [bench_data.Candidate("fleurs", "ja_jp", str(index), "unused", "raw", 1.0, str(index))
                 for index in range(9)]
        ordered = bench_data.stratified_order(items, 11)
        for offset in range(3):
            actual = ordered[offset::3]
            expected = sorted(items[offset * 3:(offset + 1) * 3],
                              key=lambda item: hashlib.sha256(f"11:{item.id}".encode("utf-8")).hexdigest())
            self.assertEqual(actual, expected)
        self.assertEqual(ordered, bench_data.stratified_order(reversed(items), 11))
        with self.assertRaisesRegex(bench_data.DataError, "seed"):
            bench_data.stratified_order(items, -11)

    def test_conversion_resamples_and_downmixes_wav_and_flac(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            for index, data in enumerate((wav_bytes(48000, 2, 4800, 1000), flac_bytes(wav_bytes(value=1000)))):
                target = Path(tmp) / f"{index}.wav"
                duration = bench_data.convert_audio(data, target)
                self.assertAlmostEqual(duration, 0.1, places=4)
                with wave.open(str(target)) as wav:
                    self.assertEqual((wav.getframerate(), wav.getnchannels(), wav.getsampwidth(), wav.getnframes()), (16000, 1, 2, 1600))
                    self.assertNotEqual(wav.readframes(1600), b"\0" * 3200)
            self.assertAlmostEqual(bench_data.flac_duration(flac_bytes(wav_bytes())), 0.1)
            with self.assertRaises(bench_data.DataError):
                bench_data.flac_duration(b"bad")


class CliTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.cache = Path(self.temporary.name) / "cache"
        # Every test is offline, including accidental network calls.
        self.network = patch.object(bench_data.urllib.request, "urlopen", side_effect=AssertionError("network forbidden"))
        self.urlopen = self.network.start()
        self.addCleanup(self.network.stop)
        self.stdout = io.StringIO()
        self.stderr = io.StringIO()

    def run_cli(self, *args: str) -> int:
        with contextlib.redirect_stdout(self.stdout), contextlib.redirect_stderr(self.stderr):
            return bench_data.main(["--cache-dir", str(self.cache), *args])

    def build_fleurs(self, name: str = "first", *extra: str) -> dict:
        self.assertEqual(self.run_cli("build", "--source", "fleurs:ja_jp", "--out", name, "--count-per-source", "3", *extra), 0, self.stderr.getvalue())
        return json.loads((self.cache / "manifests" / f"{name}.json").read_text(encoding="utf-8"))

    def test_build_is_stable_relative_and_only_converts_selected(self) -> None:
        fleurs_fixture(self.cache, compressed=False)
        with patch.object(bench_data, "convert_audio", wraps=bench_data.convert_audio) as convert:
            first = self.build_fleurs()
            self.assertEqual(convert.call_count, 3)
        second = self.build_fleurs("second")
        self.assertEqual(first["schema"], "asr-bench-manifest/1")
        self.assertEqual(first["selection"], {"profile": "smoke", "seed": 20261103, "count_per_source": 3, "pool": "dev", "holdout_percent": 50, "strata": "duration-tertile"})
        self.assertEqual([{k: v for k, v in item.items() if k != "audio_path"} for item in first["items"]],
                         [{k: v for k, v in item.items() if k != "audio_path"} for item in second["items"]])
        manifest = self.cache / "manifests" / "first.json"
        for item in first["items"]:
            audio = manifest.parent / item["audio_path"]
            self.assertTrue(audio.is_file())
            self.assertEqual(audio.name, item["id"] + ".wav")
            self.assertEqual(bench_data.sha256_file(audio), item["audio_sha256"])
            self.assertEqual(item["language"], "ja-JP")
            self.assertTrue(item["reference"].startswith(" 生の文"))
        snapshot = manifest.read_bytes()
        with patch.object(bench_data, "convert_audio") as convert:
            self.assertEqual(self.run_cli("build", "--out", "first"), 1)
            convert.assert_not_called()
        self.assertEqual(manifest.read_bytes(), snapshot)
        self.urlopen.assert_not_called()

    def test_build_multiple_sources_and_librispeech_pools(self) -> None:
        fleurs_fixture(self.cache, "en_us")
        librispeech_fixture(self.cache)
        for profile in ("dev", "holdout"):
            self.assertEqual(self.run_cli("build", "--source", "fleurs:en_us", "--source", "librispeech:test-clean", "--profile", profile,
                                         "--count-per-source", "8", "--seed", "27", "--out", profile), 0, self.stderr.getvalue())
        dev = bench_data.read_json(self.cache / "manifests/dev.json")
        holdout = bench_data.read_json(self.cache / "manifests/holdout.json")
        self.assertEqual(len(dev["items"]), 16)
        self.assertEqual({item["language"] for item in dev["items"]}, {"en-US"})
        self.assertFalse({(item["source"], item["group"]) for item in dev["items"]} & {(item["source"], item["group"]) for item in holdout["items"]})
        for document in (dev, holdout):
            counts = Counter(item["speaker"] for item in document["items"] if item["source"] == "librispeech")
            eligible_speakers = sum(bench_data.assign_pool("librispeech", str(speaker)) == document["selection"]["pool"]
                                    for speaker in range(1, 21))
            self.assertLessEqual(max(counts.values()), (8 + eligible_speakers - 1) // eligible_speakers)
            self.assertEqual({item["locale"] for item in document["items"]}, {"en_us"})
            self.assertTrue(all(item["id"] == f"librispeech-test-clean-{item['item_id']}"
                                for item in document["items"] if item["source"] == "librispeech"))
        lock = Path(self.temporary.name) / "mixed.lock.json"
        self.assertEqual(self.run_cli("lock", "--manifest", str(self.cache / "manifests/dev.json"), "--output", str(lock)), 0)
        self.assertEqual(self.run_cli("build", "--lock", str(lock), "--out", "mixed-rebuilt"), 0, self.stderr.getvalue())
        rebuilt = bench_data.read_json(self.cache / "manifests/mixed-rebuilt.json")
        self.assertEqual([item["id"] for item in dev["items"]], [item["id"] for item in rebuilt["items"]])

    def test_lock_rebuild_and_no_text_or_paths_leak(self) -> None:
        fleurs_fixture(self.cache)
        first = self.build_fleurs()
        manifest = self.cache / "manifests/first.json"
        first["selection"]["reference"] = "private selection text"
        first["sources"][0]["reference"] = "private source text"
        manifest.write_text(json.dumps(first), encoding="utf-8")
        lock = Path(self.temporary.name) / "tracked.lock.json"
        self.assertEqual(self.run_cli("lock", "--manifest", str(manifest), "--output", str(lock)), 0, self.stderr.getvalue())
        content = lock.read_text(encoding="utf-8")
        for forbidden in ("reference", "audio_path", "生の文", "private selection", "private source", "provenance"):
            self.assertNotIn(forbidden, content)
        snapshot = lock.read_bytes()
        self.assertEqual(self.run_cli("lock", "--manifest", str(manifest), "--output", str(lock)), 1)
        self.assertEqual(lock.read_bytes(), snapshot)
        self.assertEqual(self.run_cli("build", "--lock", str(lock), "--out", "rebuilt"), 0, self.stderr.getvalue())
        rebuilt = bench_data.read_json(self.cache / "manifests/rebuilt.json")
        self.assertEqual([item["id"] for item in rebuilt["items"]], [item["id"] for item in first["items"]])
        self.assertEqual([item["source_audio_sha256"] for item in rebuilt["items"]], [item["source_audio_sha256"] for item in first["items"]])
        self.assertEqual(self.run_cli("build", "--lock", str(lock), "--source", "fleurs:ja_jp", "--out", "bad"), 1)

    def test_lock_checks_missing_items_and_original_audio_hash(self) -> None:
        members = fleurs_fixture(self.cache)
        original = self.build_fleurs()
        lock = Path(self.temporary.name) / "subset.lock.json"
        self.assertEqual(self.run_cli("lock", "--manifest", str(self.cache / "manifests/first.json"), "--output", str(lock)), 0)
        spec = bench_data.SOURCES["fleurs:ja_jp"]
        member = original["items"][0]["provenance"].split("#")[1]
        members[member] = wav_bytes(samples=1600, value=42)
        write_tar(spec.archive(self.cache), members)
        record_cache(spec.archive(self.cache), spec.url)
        self.assertEqual(self.run_cli("build", "--lock", str(lock), "--out", "mismatch"), 1)
        self.assertIn("source_audio_sha256 mismatch", self.stderr.getvalue())
        self.assertFalse((self.cache / "audio/mismatch").exists())
        del members[member]
        write_tar(spec.archive(self.cache), members)
        record_cache(spec.archive(self.cache), spec.url)
        self.assertEqual(self.run_cli("build", "--lock", str(lock), "--out", "missing"), 1)
        self.assertIn("missing selected archive item", self.stderr.getvalue())
        tsv = spec.directory(self.cache) / "test.tsv"
        missing_id = original["items"][0]["item_id"]
        tsv.write_text("\n".join(row for row in tsv.read_text(encoding="utf-8").splitlines() if f"\t{missing_id}.wav\t" not in row), encoding="utf-8")
        record_cache(tsv, spec.tsv_url)
        self.assertEqual(self.run_cli("build", "--lock", str(lock), "--out", "missing-tsv"), 1)
        self.assertIn("missing locked item", self.stderr.getvalue())

    def test_duration_cache_corruption_and_malformed_documents_fail_cleanly(self) -> None:
        members = fleurs_fixture(self.cache)
        first = self.build_fleurs()
        spec = bench_data.SOURCES["fleurs:ja_jp"]
        member = first["items"][0]["provenance"].split("#")[1]
        members[member] = wav_bytes(samples=160)
        write_tar(spec.archive(self.cache), members)
        self.assertEqual(self.run_cli("build", "--source", spec.key, "--out", "corrupt", "--count-per-source", "3"), 1)
        self.assertIn("cache SHA256/metadata mismatch", self.stderr.getvalue())
        record_cache(spec.archive(self.cache), spec.url)
        self.assertEqual(self.run_cli("build", "--source", spec.key, "--out", "duration", "--count-per-source", "3"), 1)
        self.assertIn("duration mismatch", self.stderr.getvalue())
        manifest = self.cache / "manifests/first.json"
        for mutation, message in ((lambda doc: doc.update(schema="unknown"), "unsupported schema"),
                                  (lambda doc: doc["items"].append(doc["items"][0]), "duplicate item id")):
            document = json.loads(json.dumps(first))
            mutation(document)
            manifest.write_text(json.dumps(document), encoding="utf-8")
            self.assertEqual(self.run_cli("lock", "--manifest", str(manifest), "--output", str(self.cache / "invalid.lock.json")), 1)
            self.assertIn(message, self.stderr.getvalue())

    def test_fetch_license_gate_progress_atomicity_and_cached_verification(self) -> None:
        self.assertEqual(self.run_cli("fetch", "--source", "fleurs:ja_jp"), 1)
        self.urlopen.assert_not_called()
        self.assertIn("CC-BY-4.0", self.stderr.getvalue())
        self.assertIn("size:", self.stderr.getvalue())
        self.assertFalse(self.cache.exists())
        spec = bench_data.SOURCES["fleurs:ja_jp"]

        self.urlopen.side_effect = [Response(b"archive"), Response(b"tsv")]
        self.assertEqual(self.run_cli("fetch", "--source", spec.key, "--accept-license"), 0, self.stderr.getvalue())
        self.assertEqual(spec.archive(self.cache).read_bytes(), b"archive")
        self.assertEqual(bench_data.read_json(bench_data.cache_record(spec.archive(self.cache)))["sha256"], hashlib.sha256(b"archive").hexdigest())
        self.assertIn("complete:", self.stderr.getvalue())
        self.urlopen.reset_mock(side_effect=True)
        self.urlopen.side_effect = AssertionError("should reuse cache")
        self.assertEqual(self.run_cli("fetch", "--source", spec.key, "--accept-license"), 0)
        self.urlopen.assert_not_called()
        spec.archive(self.cache).write_bytes(b"tampered")
        self.assertEqual(self.run_cli("fetch", "--source", spec.key, "--accept-license"), 1)
        self.assertEqual(spec.archive(self.cache).read_bytes(), b"tampered")
        self.urlopen.side_effect = [Response(b"short", 100)]
        other = bench_data.SOURCES["librispeech:test-other"]
        self.assertEqual(self.run_cli("fetch", "--source", other.key, "--accept-license"), 1)
        self.assertFalse(other.archive(self.cache).exists())
        self.assertFalse(bench_data.cache_record(other.archive(self.cache)).exists())

    def test_invalid_names_counts_and_duplicate_sources(self) -> None:
        for args in (("--out", "../unsafe"), ("--out", "CON"), ("--out", "x", "--count-per-source", "0"),
                     ("--out", "x", "--holdout-percent", "101"),
                     ("--out", "x", "--source", "fleurs:ja_jp", "--source", "fleurs:ja_jp")):
            self.assertEqual(self.run_cli("build", *args), 1)
        self.assertEqual(bench_data.parse_args(["build", "--cache-dir", "other", "--out", "x"]).cache_dir, Path("other"))

    def test_invalid_audio_and_locked_source_revision_fail_cleanly(self) -> None:
        members = fleurs_fixture(self.cache)
        first = self.build_fleurs()
        lock = Path(self.temporary.name) / "subset.lock.json"
        self.assertEqual(self.run_cli("lock", "--manifest", str(self.cache / "manifests/first.json"), "--output", str(lock)), 0)
        content = bench_data.read_json(lock)
        content["sources"][0]["revision"] = "wrong-revision"
        lock.write_text(json.dumps(content), encoding="utf-8")
        self.assertEqual(self.run_cli("build", "--lock", str(lock), "--out", "revision"), 1)
        self.assertIn("source metadata/revision mismatch", self.stderr.getvalue())
        spec = bench_data.SOURCES["fleurs:ja_jp"]
        member = first["items"][0]["provenance"].split("#")[1]
        members[member] = b"not an audio file"
        write_tar(spec.archive(self.cache), members)
        record_cache(spec.archive(self.cache), spec.url)
        self.assertEqual(self.run_cli("build", "--source", spec.key, "--out", "invalid-audio", "--count-per-source", "3"), 1)
        self.assertIn("cannot decode/resample selected audio", self.stderr.getvalue())
        self.assertFalse((self.cache / "manifests/invalid-audio.json").exists())

    def test_parallel_fleurs_sentences_share_pool_across_languages(self) -> None:
        fleurs_fixture(self.cache, "ja_jp")
        fleurs_fixture(self.cache, "en_us")
        pools = {}
        for profile in ("dev", "holdout"):
            count = sum(bench_data.assign_pool("fleurs", str(group)) == profile for group in range(24))
            self.assertEqual(self.run_cli("build", "--source", "fleurs:ja_jp", "--source", "fleurs:en_us",
                                         "--profile", profile, "--count-per-source", str(count), "--out", profile), 0,
                             self.stderr.getvalue())
            document = bench_data.read_json(self.cache / "manifests" / f"{profile}.json")
            for item in document["items"]:
                pools[item["locale"], item["group"]] = item["pool"]
        for group in range(24):
            self.assertEqual(pools["ja_jp", str(group)], pools["en_us", str(group)])

    def test_fetch_without_content_length(self) -> None:
        spec = bench_data.SOURCES["librispeech:test-other"]
        self.urlopen.side_effect = [Response(b"synthetic archive", content_length=False)]
        self.assertEqual(self.run_cli("fetch", "--source", spec.key, "--accept-license"), 0, self.stderr.getvalue())
        record = bench_data.read_json(bench_data.cache_record(spec.archive(self.cache)))
        self.assertEqual(record["size_bytes"], len(b"synthetic archive"))
        self.assertIn("size: unknown bytes", self.stderr.getvalue())

    def test_incomplete_cache_pairs_name_both_recovery_paths(self) -> None:
        spec = bench_data.SOURCES["librispeech:test-other"]
        archive = spec.archive(self.cache)
        archive.parent.mkdir(parents=True)
        record = bench_data.cache_record(archive)
        for existing, missing in ((archive, record), (record, archive)):
            with self.subTest(existing=existing):
                existing.write_bytes(b"preserve this file")
                self.stderr.seek(0)
                self.stderr.truncate()
                self.assertEqual(self.run_cli("fetch", "--source", spec.key, "--accept-license"), 1)
                self.assertIn("delete these exact files", self.stderr.getvalue())
                self.assertIn(str(archive.resolve()), self.stderr.getvalue())
                self.assertIn(str(record.resolve()), self.stderr.getvalue())
                self.assertEqual(existing.read_bytes(), b"preserve this file")
                self.assertFalse(missing.exists())
                existing.unlink()
        self.urlopen.assert_not_called()

    def test_fetch_prepares_record_before_publication_and_rolls_back_failure(self) -> None:
        spec = bench_data.SOURCES["librispeech:test-other"]
        archive = spec.archive(self.cache)
        publish_new = bench_data._publish_new
        self.urlopen.side_effect = [Response(b"synthetic archive")]

        def publish(temporary: Path, destination: Path) -> None:
            self.assertTrue((temporary.parent / "record.json").is_file())
            if destination == bench_data.cache_record(archive):
                raise OSError("record publication failed")
            publish_new(temporary, destination)

        with patch.object(bench_data, "_publish_new", side_effect=publish):
            self.assertEqual(self.run_cli("fetch", "--source", spec.key, "--accept-license"), 1)
        self.assertFalse(archive.exists())
        self.assertFalse(bench_data.cache_record(archive).exists())
        self.assertEqual(list(archive.parent.glob(".download-*")), [])
        self.urlopen.side_effect = [Response(b"synthetic archive")]
        with patch.object(bench_data, "write_json_temporary", side_effect=OSError("record preparation failed")), \
                patch.object(bench_data, "_publish_new") as publish_mock:
            self.assertEqual(self.run_cli("fetch", "--source", spec.key, "--accept-license"), 1)
            publish_mock.assert_not_called()
        self.assertFalse(archive.exists())

    def test_build_prepares_manifest_and_cleans_only_its_published_audio(self) -> None:
        fleurs_fixture(self.cache)
        command = ("build", "--source", "fleurs:ja_jp", "--count-per-source", "3")
        with patch.object(bench_data, "write_json_temporary", side_effect=OSError("manifest preparation failed")):
            self.assertEqual(self.run_cli(*command, "--out", "prepare-failure"), 1)
        self.assertFalse((self.cache / "audio/prepare-failure").exists())

        def fail_publication(temporary: Path, destination: Path) -> None:
            document = bench_data.read_json(temporary)
            self.assertEqual(document["name"], "publish-failure")
            self.assertTrue((self.cache / "audio/publish-failure").is_dir())
            raise OSError("manifest publication failed")

        with patch.object(bench_data, "_publish_new", side_effect=fail_publication):
            self.assertEqual(self.run_cli(*command, "--out", "publish-failure"), 1)
        self.assertFalse((self.cache / "audio/publish-failure").exists())
        self.assertFalse((self.cache / "manifests/publish-failure.json").exists())
        self.assertEqual(list((self.cache / "audio").glob(".build-*")), [])
        self.assertEqual(self.run_cli(*command, "--out", "publish-failure"), 0, self.stderr.getvalue())
        publish_new = bench_data._publish_new

        def competing_manifest(temporary: Path, destination: Path) -> None:
            with destination.open("xb") as output:
                output.write(b"another run's manifest")
            publish_new(temporary, destination)

        with patch.object(bench_data, "_publish_new", side_effect=competing_manifest):
            self.assertEqual(self.run_cli(*command, "--out", "collision"), 1)
        self.assertEqual((self.cache / "manifests/collision.json").read_bytes(), b"another run's manifest")
        self.assertFalse((self.cache / "audio/collision").exists())

    def test_archive_hash_notice_and_locked_metadata_or_extra_items(self) -> None:
        members = fleurs_fixture(self.cache)
        first = self.build_fleurs()
        manifest = self.cache / "manifests/first.json"
        lock = Path(self.temporary.name) / "subset.lock.json"
        self.assertEqual(self.run_cli("lock", "--manifest", str(manifest), "--output", str(lock)), 0)
        original = bench_data.read_json(lock)
        spec = bench_data.SOURCES["fleurs:ja_jp"]
        write_tar(spec.archive(self.cache), members, compressed=False)
        record_cache(spec.archive(self.cache), spec.url)
        self.assertEqual(self.run_cli("build", "--lock", str(lock), "--out", "repacked"), 0, self.stderr.getvalue())
        self.assertIn("notice: archive_sha256 differs", self.stderr.getvalue())
        self.assertIn(original["sources"][0]["archive_sha256"], self.stderr.getvalue())
        self.assertIn(bench_data.sha256_file(spec.archive(self.cache)), self.stderr.getvalue())
        rebuilt = bench_data.read_json(self.cache / "manifests/repacked.json")
        self.assertEqual([item["source_audio_sha256"] for item in first["items"]],
                         [item["source_audio_sha256"] for item in rebuilt["items"]])
        for field, value in (("pool", "holdout"), ("group", "different"), ("locale", "en_us")):
            document = json.loads(json.dumps(original))
            document["items"][0][field] = value
            lock.write_text(json.dumps(document), encoding="utf-8")
            self.assertEqual(self.run_cli("build", "--lock", str(lock), "--out", f"bad-{field}"), 1)
            self.assertIn("locked item metadata/pool mismatch", self.stderr.getvalue())
        document = json.loads(json.dumps(original))
        extra = dict(document["items"][0], id="extra-locked-item", locale="not-a-config")
        document["items"].append(extra)
        lock.write_text(json.dumps(document), encoding="utf-8")
        self.assertEqual(self.run_cli("build", "--lock", str(lock), "--out", "extra"), 1)
        self.assertIn("missing locked item: extra-locked-item", self.stderr.getvalue())

    def test_required_document_and_record_fields_are_validated(self) -> None:
        fleurs_fixture(self.cache)
        first = self.build_fleurs()
        manifest = self.cache / "manifests/first.json"
        output = Path(self.temporary.name) / "invalid.lock.json"
        mutations = (("name", lambda doc: doc.pop("name")),
                     ("created_by", lambda doc: doc.pop("created_by")),
                     ("speaker", lambda doc: doc["items"][0].pop("speaker")),
                     ("license", lambda doc: doc["sources"][0].pop("license")),
                     ("seed", lambda doc: doc["selection"].pop("seed")),
                     ("profile", lambda doc: doc["selection"].update(profile=[])),
                     ("pool", lambda doc: doc["items"][0].update(pool=None)))
        for field, mutate in mutations:
            with self.subTest(field=field):
                document = json.loads(json.dumps(first))
                mutate(document)
                manifest.write_text(json.dumps(document), encoding="utf-8")
                self.stderr.seek(0)
                self.stderr.truncate()
                self.assertEqual(self.run_cli("lock", "--manifest", str(manifest), "--output", str(output)), 1)
                self.assertIn(field, self.stderr.getvalue())
                self.assertFalse(output.exists())
        spec = bench_data.SOURCES["fleurs:ja_jp"]
        record_path = bench_data.cache_record(spec.archive(self.cache))
        original = bench_data.read_json(record_path)
        for field in ("url", "sha256", "size_bytes"):
            record = dict(original)
            del record[field]
            record_path.write_text(json.dumps(record), encoding="utf-8")
            self.stderr.seek(0)
            self.stderr.truncate()
            self.assertEqual(self.run_cli("fetch", "--source", spec.key, "--accept-license"), 1)
            self.assertIn(f"missing required field '{field}'", self.stderr.getvalue())

    def test_librispeech_missing_audio_and_transcript(self) -> None:
        original = librispeech_fixture(self.cache)
        spec = bench_data.SOURCES["librispeech:test-clean"]
        for suffix, message in ((".flac", "missing LibriSpeech audio item"), (".trans.txt", "missing LibriSpeech transcript item")):
            members = dict(original)
            del members[next(name for name in members if name.endswith(suffix))]
            write_tar(spec.archive(self.cache), members)
            record_cache(spec.archive(self.cache), spec.url)
            self.assertEqual(self.run_cli("build", "--source", spec.key, "--out", "missing", "--count-per-source", "1"), 1)
            self.assertIn(message, self.stderr.getvalue())

    def test_no_audio_stream_member_is_a_clean_error(self) -> None:
        members = fleurs_fixture(self.cache)
        first = self.build_fleurs()
        video = video_bytes()
        target = Path(self.temporary.name) / "no-audio.wav"
        with self.assertRaisesRegex(bench_data.DataError, "no audio stream"):
            bench_data.convert_audio(video, target)
        self.assertFalse(target.exists())
        spec = bench_data.SOURCES["fleurs:ja_jp"]
        members[first["items"][0]["provenance"].split("#")[1]] = video
        write_tar(spec.archive(self.cache), members)
        record_cache(spec.archive(self.cache), spec.url)
        self.assertEqual(self.run_cli("build", "--source", spec.key, "--out", "no-audio", "--count-per-source", "3"), 1)
        self.assertIn("no audio stream", self.stderr.getvalue())

    def test_fleurs_duration_tolerance_uses_decoded_length(self) -> None:
        spec = bench_data.SOURCES["fleurs:ja_jp"]
        spec.directory(self.cache).mkdir(parents=True)
        tsv = spec.directory(self.cache) / "test.tsv"
        tsv.write_text("1\tone.wav\tRAW\tnormal\tchars\t1600\tfemale\n", encoding="utf-8")
        record_cache(tsv, spec.tsv_url)
        for index, delta in enumerate((800, -800, 801, -801)):
            write_tar(spec.archive(self.cache), {"test/one.wav": wav_bytes(samples=1600 + delta)})
            record_cache(spec.archive(self.cache), spec.url)
            name = f"tolerance-{index}"
            result = self.run_cli("build", "--source", spec.key, "--out", name,
                                  "--count-per-source", "1", "--holdout-percent", "0")
            if abs(delta) == 800:
                self.assertEqual(result, 0, self.stderr.getvalue())
                document = bench_data.read_json(self.cache / "manifests" / f"{name}.json")
                self.assertEqual(document["items"][0]["duration"], (1600 + delta) / 16000)
            else:
                self.assertEqual(result, 1)
                self.assertIn("duration mismatch: fleurs-ja_jp-test-one", self.stderr.getvalue())
                self.assertIn("metadata 0.100000s", self.stderr.getvalue())
                self.assertIn(f"decoded {(1600 + delta) / 16000:.6f}s", self.stderr.getvalue())

    def test_negative_seed_and_default_cache_independent_of_cwd(self) -> None:
        self.assertEqual(self.run_cli("build", "--out", "negative", "--seed", "-1"), 1)
        self.assertIn("--seed must be nonnegative", self.stderr.getvalue())
        self.assertFalse(self.cache.exists())
        fleurs_fixture(self.cache)
        self.build_fleurs()
        lock = Path(self.temporary.name) / "negative.lock.json"
        self.assertEqual(self.run_cli("lock", "--manifest", str(self.cache / "manifests/first.json"), "--output", str(lock)), 0)
        document = bench_data.read_json(lock)
        document["selection"]["seed"] = -1
        lock.write_text(json.dumps(document), encoding="utf-8")
        self.assertEqual(self.run_cli("build", "--lock", str(lock), "--out", "negative-lock"), 1)
        self.assertIn("selection seed must be nonnegative", self.stderr.getvalue())
        previous = Path.cwd()
        try:
            os.chdir(self.temporary.name)
            args = bench_data.parse_args(["build", "--out", "default"])
            self.assertEqual(args.cache_dir, _SCRIPT.parents[1] / "bench_data")
        finally:
            os.chdir(previous)

    def test_source_kind_controls_dispatch(self) -> None:
        librispeech_fixture(self.cache)
        spec = replace(bench_data.SOURCES["librispeech:test-clean"], tsv_url="https://example.invalid/unused.tsv")
        self.assertEqual(len(bench_data.load_candidates(self.cache, spec)), 120)
        self.assertEqual(len(bench_data.source_downloads(spec)), 1)
        with patch.dict(bench_data.SOURCES, {spec.key: spec}):
            self.assertEqual(self.run_cli("fetch", "--source", spec.key, "--accept-license"), 0, self.stderr.getvalue())
        candidates = bench_data.load_candidates(self.cache, spec)
        deduped = bench_data.select_items(candidates, 20, 1, "dev", 0, kind="fleurs")
        self.assertEqual(len({item.group for item in deduped}), 20)
        unknown = replace(spec, kind="unsupported")
        with self.assertRaisesRegex(bench_data.DataError, "unsupported source kind"):
            bench_data.load_candidates(self.cache, unknown)
        with self.assertRaisesRegex(bench_data.DataError, "unsupported source kind"):
            bench_data.source_downloads(unknown)
        alias = replace(bench_data.SOURCES["fleurs:ja_jp"], source="alias")
        tsv = alias.directory(self.cache) / "test.tsv"
        tsv.parent.mkdir(parents=True)
        tsv.write_text("1\tone.wav\tRAW\tnormal\tchars\t1600\tfemale\n", encoding="utf-8")
        record_cache(tsv, alias.tsv_url)
        self.assertEqual(bench_data.load_candidates(self.cache, alias)[0].source, "alias")


if __name__ == "__main__":
    unittest.main()
