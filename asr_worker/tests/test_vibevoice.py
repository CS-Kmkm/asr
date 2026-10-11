from __future__ import annotations

import contextlib
import json
import os
import sys
import types
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

from asr_worker.backends import BackendError, VibeVoiceBackend, join_segment_texts


class _FakeIds:
    """The slice of a token-id tensor that the backend uses."""

    def __init__(self, rows: list[list[int]]) -> None:
        self.rows = rows

    @property
    def shape(self) -> tuple[int, int]:
        return (len(self.rows), len(self.rows[0]) if self.rows else 0)

    def __getitem__(self, key: Any) -> Any:
        rows, columns = key
        if isinstance(rows, int):
            return self.rows[rows][columns]
        return _FakeIds([row[columns] for row in self.rows[rows]])


class _FakeInputs(dict):
    def to(self, *args: Any) -> "_FakeInputs":
        return self


class _FakeProcessor:
    def __init__(self, parsed: list[dict[str, Any]], prompt_length: int = 3) -> None:
        self.parsed = parsed
        self.prompt_length = prompt_length
        self.decoded: list[_FakeIds] = []

    def apply_transcription_request(self, audio: str, prompt: str | None) -> _FakeInputs:
        return _FakeInputs(input_ids=_FakeIds([[1] * self.prompt_length]))

    def decode(self, generated_ids: _FakeIds, return_format: str) -> list[list[dict[str, Any]]]:
        self.decoded.append(generated_ids)
        return [self.parsed]


class _CutOffProcessor(_FakeProcessor):
    """Fails like the real parser on a transcript that never closed its JSON array."""

    def decode(self, generated_ids: _FakeIds, return_format: str) -> list[list[dict[str, Any]]]:
        self.decoded.append(generated_ids)
        return [json.loads('[{"Start": 0.0, "Content": "cut')]


class _FakeModel:
    device = "cuda:0"
    dtype = "bfloat16"

    def __init__(self, generated: list[int], eos_token_id: Any = None) -> None:
        self.generated = generated
        self.max_new_tokens: int | None = None
        if eos_token_id is not None:
            self.generation_config = types.SimpleNamespace(eos_token_id=eos_token_id)

    def generate(self, input_ids: _FakeIds, max_new_tokens: int) -> _FakeIds:
        self.max_new_tokens = max_new_tokens
        return _FakeIds([input_ids.rows[0] + self.generated[:max_new_tokens]])


def _fake_torch() -> types.ModuleType:
    module = types.ModuleType("torch")
    module.inference_mode = contextlib.nullcontext  # type: ignore[attr-defined]
    return module


def _segments(*texts: str) -> list[dict[str, Any]]:
    return [
        {"Start": float(index), "End": float(index + 1), "Speaker": 0, "Content": text}
        for index, text in enumerate(texts)
    ]


def _transcribe(processor: _FakeProcessor, model: _FakeModel) -> tuple[str, list[dict[str, Any]]]:
    backend = VibeVoiceBackend()
    backend.processor = processor
    backend.model = model
    with patch.dict(sys.modules, {"torch": _fake_torch()}):
        return backend.transcribe(Path("missing.wav"), None)


class JoinSegmentTextsTests(unittest.TestCase):
    def test_japanese_segments_are_joined_without_spaces(self) -> None:
        self.assertEqual(join_segment_texts(["今日は会議です。", "明日は休みです"]), "今日は会議です。明日は休みです")
        self.assertEqual(join_segment_texts(["コーヒー", "ください"]), "コーヒーください")

    def test_latin_segments_are_joined_with_one_space(self) -> None:
        self.assertEqual(join_segment_texts(["Hello there.", " How are you? "]), "Hello there. How are you?")

    def test_no_space_when_either_side_of_a_boundary_is_cjk(self) -> None:
        self.assertEqual(join_segment_texts(["Tauri", "で作ります"]), "Tauriで作ります")
        self.assertEqual(join_segment_texts(["設定は", "VibeVoice"]), "設定はVibeVoice")
        self.assertEqual(join_segment_texts(["中文", "测试"]), "中文测试")

    def test_full_width_punctuation_and_half_width_katakana_join_without_spaces(self) -> None:
        self.assertEqual(join_segment_texts(["OK", "（了解）"]), "OK（了解）")
        self.assertEqual(join_segment_texts(["ok", "ｶﾅ"]), "okｶﾅ")

    def test_korean_keeps_word_spacing(self) -> None:
        self.assertEqual(join_segment_texts(["안녕하세요", "반갑습니다"]), "안녕하세요 반갑습니다")

    def test_empty_segments_are_skipped(self) -> None:
        self.assertEqual(join_segment_texts(["", "  ", "hello", "", "world"]), "hello world")
        self.assertEqual(join_segment_texts([]), "")


class VibeVoiceTranscribeTests(unittest.TestCase):
    def test_japanese_segments_reach_the_transcript_without_spaces(self) -> None:
        processor = _FakeProcessor(_segments("今日は会議です", "明日は休みです"))
        text, segments = _transcribe(processor, _FakeModel([7, 8, 2]))

        self.assertEqual(text, "今日は会議です明日は休みです")
        self.assertEqual([segment["text"] for segment in segments], ["今日は会議です", "明日は休みです"])

    def test_english_segments_are_joined_with_one_space(self) -> None:
        processor = _FakeProcessor(_segments("Hello there.", "See you tomorrow."))
        text, _ = _transcribe(processor, _FakeModel([7, 8, 2]))

        self.assertEqual(text, "Hello there. See you tomorrow.")



class VibeVoiceTruncationTests(unittest.TestCase):
    def transcribe_with_limit(self, processor: _FakeProcessor, model: _FakeModel) -> tuple[str, list[dict[str, Any]]]:
        with patch.dict(os.environ, {"ASR_MAX_NEW_TOKENS": "4"}):
            return _transcribe(processor, model)

    def test_output_cut_off_at_the_token_limit_is_a_clear_error(self) -> None:
        processor = _CutOffProcessor([])
        model = _FakeModel([7, 8, 9, 10, 11, 12], eos_token_id=2)

        with self.assertRaises(BackendError) as raised:
            self.transcribe_with_limit(processor, model)

        self.assertEqual(raised.exception.code, "transcript_truncated")
        self.assertIn("4-token output limit", raised.exception.message)
        self.assertIn("ASR_MAX_NEW_TOKENS", raised.exception.message)
        self.assertEqual(model.max_new_tokens, 4)
        self.assertEqual(processor.decoded, [])

    def test_limit_is_detected_without_a_known_end_token(self) -> None:
        with self.assertRaises(BackendError) as raised:
            self.transcribe_with_limit(_CutOffProcessor([]), _FakeModel([7, 8, 9, 10]))

        self.assertEqual(raised.exception.code, "transcript_truncated")

    def test_transcript_ending_exactly_at_the_limit_is_complete(self) -> None:
        processor = _FakeProcessor(_segments("Hello"))
        text, _ = self.transcribe_with_limit(processor, _FakeModel([7, 8, 9, 2], eos_token_id=[2, 3]))

        self.assertEqual(text, "Hello")

    def test_transcript_below_the_limit_is_decoded(self) -> None:
        processor = _FakeProcessor(_segments("Hello"))
        text, _ = self.transcribe_with_limit(processor, _FakeModel([7, 8, 2], eos_token_id=2))

        self.assertEqual(text, "Hello")
        self.assertEqual(processor.decoded[0].rows, [[7, 8, 2]])

    def test_other_parse_failures_stay_transcription_failed(self) -> None:
        with self.assertRaises(BackendError) as raised:
            self.transcribe_with_limit(_CutOffProcessor([]), _FakeModel([7, 2], eos_token_id=2))

        self.assertEqual(raised.exception.code, "transcription_failed")


if __name__ == "__main__":
    unittest.main()
