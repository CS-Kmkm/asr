from __future__ import annotations

import unittest

from asr_worker.download import byte_progress_tqdm


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


if __name__ == "__main__":
    unittest.main()
