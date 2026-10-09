from __future__ import annotations

import sys
import types
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

from asr_worker.download import (
    FASTER_WHISPER_ALLOW_PATTERNS,
    FASTER_WHISPER_REQUIRED_FILES,
    byte_progress_tqdm,
    cached_snapshot_path,
)

COMPLETE_SNAPSHOT = ["config.json", "model.bin", "tokenizer.json", "vocabulary.json"]


def fake_hub(snapshot: Path, calls: list[dict]) -> types.ModuleType:
    """A huggingface_hub stand-in whose cache always holds ``snapshot``.

    Like the real hub with ``local_files_only=True``, it returns the snapshot
    folder without checking which files are in it.
    """
    module = types.ModuleType("huggingface_hub")

    def snapshot_download(repo_id, **kwargs):  # noqa: ANN001, ANN202
        calls.append({"repo_id": repo_id, **kwargs})
        return str(snapshot)

    module.snapshot_download = snapshot_download  # type: ignore[attr-defined]
    return module


class ByteProgressTqdmTests(unittest.TestCase):
    def test_reports_bytes_while_the_download_total_grows(self) -> None:
        events: list[dict] = []
        # huggingface_hub creates the aggregate bar with a zero total and raises
        # it as file metadata arrives.
        bar = byte_progress_tqdm(events.append)(
            total=0,
            initial=0,
            unit="B",
            unit_scale=True,
            desc="Downloading",
        )
        bar.total += 100
        bar.update(40)
        # Reports are throttled, but reaching the total always reports.
        bar.update(60)

        self.assertEqual(
            events,
            [
                {"completed_bytes": 40, "total_bytes": 100},
                {"completed_bytes": 100, "total_bytes": 100},
            ],
        )

    def test_unknown_total_is_reported_as_missing(self) -> None:
        events: list[dict] = []
        bar = byte_progress_tqdm(events.append)(total=0, initial=0, unit="B")
        bar.update(7)

        self.assertEqual(events, [{"completed_bytes": 7, "total_bytes": None}])

    def test_file_count_bar_reports_nothing(self) -> None:
        events: list[dict] = []
        bar = byte_progress_tqdm(events.append)(total=3, desc="Fetching 3 files")
        bar.update(1)

        self.assertEqual(events, [])


class CachedSnapshotPathTests(unittest.TestCase):
    def resolve(self, files: list[str], required: list[str] | None) -> tuple[str | None, str, list[dict]]:
        with TemporaryDirectory() as tmp:
            snapshot = Path(tmp) / "snapshots" / "abc123"
            snapshot.mkdir(parents=True)
            for name in files:
                (snapshot / name).write_bytes(b"x")
            calls: list[dict] = []
            with patch.dict(sys.modules, {"huggingface_hub": fake_hub(snapshot, calls)}):
                result = cached_snapshot_path("org/repo", FASTER_WHISPER_ALLOW_PATTERNS, required)
        return result, str(snapshot), calls

    def test_complete_snapshot_is_cached(self) -> None:
        result, snapshot, calls = self.resolve(COMPLETE_SNAPSHOT, FASTER_WHISPER_REQUIRED_FILES)

        self.assertEqual(result, snapshot)
        # The cache lookup itself must never reach the network.
        self.assertEqual(calls[0]["local_files_only"], True)

    def test_older_vocabulary_format_counts_as_complete(self) -> None:
        files = ["config.json", "model.bin", "tokenizer.json", "vocabulary.txt"]
        result, snapshot, _ = self.resolve(files, FASTER_WHISPER_REQUIRED_FILES)

        self.assertEqual(result, snapshot)

    def test_interrupted_snapshot_without_model_bin_is_not_cached(self) -> None:
        # huggingface_hub creates the snapshot folder before fetching files, and
        # the small files finish first, so an interrupted download leaves this.
        files = ["config.json", "tokenizer.json", "vocabulary.json"]
        result, _, _ = self.resolve(files, FASTER_WHISPER_REQUIRED_FILES)

        self.assertIsNone(result)

    def test_snapshot_without_any_vocabulary_is_not_cached(self) -> None:
        result, _, _ = self.resolve(["config.json", "model.bin", "tokenizer.json"], FASTER_WHISPER_REQUIRED_FILES)

        self.assertIsNone(result)

    def test_optional_preprocessor_config_is_not_required(self) -> None:
        self.assertNotIn("preprocessor_config.json", FASTER_WHISPER_REQUIRED_FILES)

    def test_without_required_files_any_snapshot_folder_counts(self) -> None:
        result, snapshot, _ = self.resolve([], None)

        self.assertEqual(result, snapshot)

    def test_cache_miss_is_not_cached(self) -> None:
        module = types.ModuleType("huggingface_hub")

        def snapshot_download(repo_id, **kwargs):  # noqa: ANN001, ANN202
            raise FileNotFoundError(repo_id)

        module.snapshot_download = snapshot_download  # type: ignore[attr-defined]
        with patch.dict(sys.modules, {"huggingface_hub": module}):
            self.assertIsNone(cached_snapshot_path("org/repo", None, FASTER_WHISPER_REQUIRED_FILES))


if __name__ == "__main__":
    unittest.main()
