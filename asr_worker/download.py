from __future__ import annotations

import re
import time
from pathlib import Path
from typing import Any, Callable

ProgressCallback = Callable[[dict[str, Any]], None]

# The files faster-whisper itself fetches for a CTranslate2 Whisper repository.
FASTER_WHISPER_ALLOW_PATTERNS = [
    "config.json",
    "preprocessor_config.json",
    "model.bin",
    "tokenizer.json",
    "vocabulary.*",
]

# Progress travels over the JSONL protocol, so report often enough for a smooth
# progress bar without flooding stdout.
REPORT_INTERVAL_SECONDS = 0.2


def cached_snapshot_path(
    repo_id: str,
    allow_patterns: list[str] | None = None,
) -> str | None:
    """Return the cached snapshot directory, or None when files must be downloaded.

    The desktop app reports "loading" and "downloading" as different states, so
    the worker has to know which one a load will actually perform.
    """
    try:
        import huggingface_hub

        return huggingface_hub.snapshot_download(
            repo_id,
            allow_patterns=allow_patterns,
            local_files_only=True,
        )
    except BaseException:
        # A failure here only means the cache cannot serve the model. The caller
        # downloads it, and that path reports real download errors.
        return None


def download_snapshot(
    repo_id: str,
    allow_patterns: list[str] | None = None,
    progress: ProgressCallback | None = None,
) -> str:
    """Download a repository snapshot, reporting byte progress when requested."""
    import huggingface_hub

    return huggingface_hub.snapshot_download(
        repo_id,
        allow_patterns=allow_patterns,
        tqdm_class=byte_progress_tqdm(progress) if progress is not None else None,
    )


_COMMIT_HASH = re.compile(r"^[0-9a-f]{40}$")


def _hub_constants() -> Any:
    from huggingface_hub import constants

    return constants


def _repo_cache_folder(repo_id: str) -> Path:
    return Path(_hub_constants().HF_HUB_CACHE) / ("models--" + repo_id.replace("/", "--"))


def _pending_marker(repo_id: str) -> Path:
    # Kept beside, not inside, the hub cache so cache tools never see it.
    return (
        Path(_hub_constants().HF_HOME)
        / "local-voice-input"
        / "pending-verification"
        / repo_id.replace("/", "--")
    )


def partial_download_bytes(repo_id: str) -> int:
    """Bytes already fetched by an interrupted download that will be resumed.

    huggingface_hub keeps unfinished files as ``blobs/*.incomplete`` and
    continues them on the next download.
    """
    try:
        blobs = _repo_cache_folder(repo_id) / "blobs"
        return sum(path.stat().st_size for path in blobs.glob("*.incomplete"))
    except (ImportError, OSError):
        return 0


def mark_verification_pending(repo_id: str) -> None:
    """Remember that downloaded files must be verified before they are trusted.

    The mark survives an interrupted download or an unreachable Hub, so the
    next load verifies the files. Models cached before this mark existed load
    without network access.
    """
    try:
        marker = _pending_marker(repo_id)
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.touch()
    except (ImportError, OSError):
        # Verification still runs right after this download.
        pass


def verification_pending(repo_id: str) -> bool:
    try:
        return _pending_marker(repo_id).is_file()
    except (ImportError, OSError):
        return False


def clear_verification_pending(repo_id: str) -> None:
    try:
        _pending_marker(repo_id).unlink(missing_ok=True)
    except (ImportError, OSError):
        pass


def _mismatched_paths(api: Any, repo_id: str, revision: str) -> list[str]:
    result = api.verify_repo_checksums(repo_id, revision=revision)
    return sorted(mismatch["path"] for mismatch in result.mismatches)


def verify_snapshot(repo_id: str, snapshot_path: str, api: Any = None) -> bool:
    """Check downloaded files against the Hub's SHA-256 / git checksums.

    Files that do not match are downloaded again once. Returns whether files
    were replaced, so a backend that already loaded them can reload.
    """
    from .backends import BackendError

    revision = Path(snapshot_path).name
    if not _COMMIT_HASH.fullmatch(revision):
        # Not a hub cache snapshot, so there is nothing to compare against.
        return False
    try:
        import huggingface_hub

        api = api if api is not None else huggingface_hub.HfApi()
        mismatched = _mismatched_paths(api, repo_id, revision)
        repaired = bool(mismatched)
        for path in mismatched:
            huggingface_hub.hf_hub_download(
                repo_id, path, revision=revision, force_download=True
            )
        if repaired:
            mismatched = _mismatched_paths(api, repo_id, revision)
    except Exception as exc:
        raise BackendError(
            "model_verification_failed",
            "The downloaded model files could not be verified. "
            "Check the network connection and load the model again.",
        ) from exc
    if mismatched:
        raise BackendError(
            "model_checksum_mismatch",
            "Model files failed checksum verification after downloading them again: "
            + ", ".join(mismatched),
        )
    clear_verification_pending(repo_id)
    return repaired


def byte_progress_tqdm(progress: ProgressCallback) -> type:
    """Build a tqdm class that reports aggregated download bytes.

    huggingface_hub aggregates every file of a snapshot into one byte-counting
    bar and creates it through ``tqdm_class``, so subclassing tqdm is the
    supported way to observe download progress. The bar stays disabled because
    the worker's stderr is a diagnostic log, not a terminal; a disabled tqdm
    does not track ``n`` either, so the byte count is accumulated here.
    """
    from tqdm.auto import tqdm

    last_report = 0.0
    last_reported: tuple[int, int | None] | None = None

    class ByteProgressTqdm(tqdm):  # type: ignore[misc]
        def __init__(self, *args: Any, **kwargs: Any) -> None:
            # The same class also builds the "fetched files" bar, which counts
            # files instead of bytes.
            self.reports_bytes = kwargs.get("unit") == "B"
            self.completed = float(kwargs.get("initial") or 0)
            kwargs["disable"] = True
            super().__init__(*args, **kwargs)

        def update(self, n: float | None = 1) -> Any:
            nonlocal last_report, last_reported
            self.completed += float(n or 0)
            if self.reports_bytes:
                # The total grows while huggingface_hub resolves file metadata.
                total = float(self.total or 0)
                finished = total > 0 and self.completed >= total
                now = time.monotonic()
                if finished or now - last_report >= REPORT_INTERVAL_SECONDS:
                    update = (
                        int(self.completed),
                        int(total) if total > 0 else None,
                    )
                    # The hub also calls update() without new bytes; repeating
                    # an unchanged count would only add protocol traffic.
                    if update != last_reported:
                        last_report = now
                        last_reported = update
                        progress(
                            {"completed_bytes": update[0], "total_bytes": update[1]}
                        )
            return super().update(n)

    return ByteProgressTqdm
