from __future__ import annotations

import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

from asr_worker import download
from asr_worker.backends import BackendError, VibeVoiceBackend
from asr_worker.download import byte_progress_tqdm

COMMIT = "0123456789abcdef0123456789abcdef01234567"


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


class FakeHubApi:
    """Reports the queued mismatches, one verification at a time."""

    def __init__(self, *rounds: list[str]) -> None:
        self.rounds = list(rounds)
        self.calls: list[tuple[str, str]] = []

    def verify_repo_checksums(self, repo_id: str, revision: str):  # noqa: ANN201
        self.calls.append((repo_id, revision))
        paths = self.rounds.pop(0)
        return types.SimpleNamespace(
            mismatches=[{"path": path, "expected": "a", "actual": "b", "algorithm": "sha256"} for path in paths]
        )


class VerificationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.home = tempfile.TemporaryDirectory()
        self.addCleanup(self.home.cleanup)
        constants = types.SimpleNamespace(
            HF_HOME=self.home.name, HF_HUB_CACHE=str(Path(self.home.name) / "hub")
        )
        patcher = patch("asr_worker.download._hub_constants", return_value=constants)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.snapshot = str(Path(self.home.name) / "hub" / "models--org--repo" / "snapshots" / COMMIT)

    def test_pending_mark_survives_until_verification_succeeds(self) -> None:
        self.assertFalse(download.verification_pending("org/repo"))
        download.mark_verification_pending("org/repo")
        self.assertTrue(download.verification_pending("org/repo"))
        self.assertFalse(download.verify_snapshot("org/repo", self.snapshot, FakeHubApi([])))
        self.assertFalse(download.verification_pending("org/repo"))

    def test_partial_blobs_count_as_resumed_bytes(self) -> None:
        blobs = Path(self.home.name) / "hub" / "models--org--repo" / "blobs"
        blobs.mkdir(parents=True)
        (blobs / "aaa.incomplete").write_bytes(b"12345")
        (blobs / "bbb").write_bytes(b"complete files are not partial")
        self.assertEqual(download.partial_download_bytes("org/repo"), 5)
        self.assertEqual(download.partial_download_bytes("org/other"), 0)

    def test_mismatched_files_are_downloaded_again_once(self) -> None:
        api = FakeHubApi(["model.bin"], [])
        download.mark_verification_pending("org/repo")
        with patch("huggingface_hub.hf_hub_download") as fetch:
            self.assertTrue(download.verify_snapshot("org/repo", self.snapshot, api))
        fetch.assert_called_once_with("org/repo", "model.bin", revision=COMMIT, force_download=True)
        self.assertEqual(api.calls, [("org/repo", COMMIT), ("org/repo", COMMIT)])
        self.assertFalse(download.verification_pending("org/repo"))

    def test_persistent_mismatch_fails_and_stays_pending(self) -> None:
        download.mark_verification_pending("org/repo")
        with patch("huggingface_hub.hf_hub_download"):
            with self.assertRaises(BackendError) as raised:
                download.verify_snapshot("org/repo", self.snapshot, FakeHubApi(["model.bin"], ["model.bin"]))
        self.assertEqual(raised.exception.code, "model_checksum_mismatch")
        self.assertIn("model.bin", raised.exception.message)
        self.assertTrue(download.verification_pending("org/repo"))

    def test_unreachable_hub_fails_and_stays_pending(self) -> None:
        class OfflineApi:
            def verify_repo_checksums(self, repo_id, revision):  # noqa: ANN001, ANN201
                raise OSError("network unreachable")

        download.mark_verification_pending("org/repo")
        with self.assertRaises(BackendError) as raised:
            download.verify_snapshot("org/repo", self.snapshot, OfflineApi())
        self.assertEqual(raised.exception.code, "model_verification_failed")
        self.assertTrue(download.verification_pending("org/repo"))

    def test_snapshot_completeness_requires_files_and_every_weight_shard(self) -> None:
        snapshot = Path(self.snapshot)
        snapshot.mkdir(parents=True)
        self.assertFalse(download.snapshot_has_files(self.snapshot, ["config.json", "model.bin"]))
        self.assertFalse(download.snapshot_has_weights(self.snapshot))
        (snapshot / "config.json").write_text("{}", encoding="utf-8")
        (snapshot / "model.bin").write_bytes(b"x")
        self.assertTrue(download.snapshot_has_files(self.snapshot, ["config.json", "model.bin"]))
        (snapshot / "model.safetensors.index.json").write_text(
            '{"weight_map": {"a": "model-1.safetensors", "b": "model-2.safetensors"}}',
            encoding="utf-8",
        )
        (snapshot / "model-1.safetensors").write_bytes(b"x")
        self.assertFalse(download.snapshot_has_weights(self.snapshot))
        (snapshot / "model-2.safetensors").write_bytes(b"x")
        self.assertTrue(download.snapshot_has_weights(self.snapshot))

    def test_local_directories_are_not_hub_snapshots(self) -> None:
        self.assertFalse(download.verify_snapshot("org/repo", "C:/models/custom", FakeHubApi()))


def _fake_vibevoice_modules(loads: list[str]) -> dict[str, types.ModuleType]:
    torch = types.ModuleType("torch")
    torch.cuda = types.SimpleNamespace(is_available=lambda: True)  # type: ignore[attr-defined]
    torch.bfloat16 = "bf16"  # type: ignore[attr-defined]

    class Parameter:
        device = "cuda:0"

    class Model:
        def parameters(self):  # noqa: ANN201
            return iter([Parameter()])

    class Loader:
        @staticmethod
        def from_pretrained(model_id, **kwargs):  # noqa: ANN001, ANN003, ANN205
            loads.append(model_id)
            return Model()

    transformers = types.ModuleType("transformers")
    transformers.AutoProcessor = Loader  # type: ignore[attr-defined]
    transformers.VibeVoiceAsrForConditionalGeneration = Loader  # type: ignore[attr-defined]
    return {"torch": torch, "transformers": transformers}


class VibeVoiceVerificationTests(unittest.TestCase):
    def load(
        self, *, cached: list[str | None], pending: bool, repaired: bool, complete: bool = True
    ) -> tuple[list[str], list[dict], list]:
        loads: list[str] = []
        events: list[dict] = []
        backend = VibeVoiceBackend()
        backend.model_id = "org/vibe"
        backend.progress = events.append
        with patch.dict(sys.modules, _fake_vibevoice_modules(loads)), patch(
            "asr_worker.backends.cached_snapshot_path", side_effect=cached
        ), patch("asr_worker.backends.verification_pending", return_value=pending), patch(
            "asr_worker.backends.partial_download_bytes", return_value=0
        ), patch("asr_worker.backends.mark_verification_pending"), patch(
            "asr_worker.backends.snapshot_has_weights", return_value=complete
        ), patch(
            "asr_worker.backends.verify_snapshot", return_value=repaired
        ) as verify:
            backend.load("bf16")
        return loads, events, verify.call_args_list

    def test_fresh_download_is_verified_and_reloaded_after_a_repair(self) -> None:
        loads, events, verified = self.load(cached=[None, "/snap"], pending=False, repaired=True)
        self.assertEqual(len(loads), 4)  # processor and model, twice
        self.assertEqual([event["stage"] for event in events], ["download", "verify"])
        self.assertEqual(len(verified), 1)

    def test_incomplete_cached_snapshot_is_downloaded_and_verified_after_loading(self) -> None:
        loads, events, verified = self.load(
            cached=["/snap", "/snap"], pending=True, repaired=False, complete=False
        )
        self.assertEqual(len(loads), 2)
        self.assertEqual([event["stage"] for event in events], ["download", "verify"])
        self.assertEqual(len(verified), 1)

    def test_unverified_cached_files_are_verified_before_loading(self) -> None:
        loads, events, verified = self.load(cached=["/snap"], pending=True, repaired=False)
        self.assertEqual(len(loads), 2)
        self.assertEqual(events, [{"stage": "verify", "model": "org/vibe"}])
        self.assertEqual(len(verified), 1)


if __name__ == "__main__":
    unittest.main()
