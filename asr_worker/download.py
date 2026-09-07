from __future__ import annotations

import time
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
