from __future__ import annotations

import contextlib
import io
import json
import os
import subprocess
import sys

from asr_worker.backends import map_backend_exception
from asr_worker.diagnostics import PREFIX, log_exception
from asr_worker.tests.support import IsolatedWorkerTestCase


class DiagnosticTests(IsolatedWorkerTestCase):
    def test_redacted_stack_and_known_hint_exclude_values_and_source_lines(self):
        secret = "private transcript hf_credential C:/Users/private/audio.wav https://secret.invalid/key"
        output = io.StringIO()
        with contextlib.redirect_stderr(output):
            try:
                try:
                    raise ValueError(secret)
                except ValueError as cause:
                    raise RuntimeError("cublas64_12.dll " + secret) from cause
            except RuntimeError as exc:
                mapped = map_backend_exception(exc, "transcribe", "faster-whisper")
        self.assertEqual(mapped.code, "transcription_failed")
        rendered = output.getvalue()
        for value in ["private", "hf_credential", "secret.invalid", "audio.wav", "raise RuntimeError"]:
            self.assertNotIn(value, rendered)
        record = json.loads(rendered.removeprefix(PREFIX))
        self.assertEqual(record["chain"][0]["hint"], "missing_cublas12")
        self.assertEqual(record["chain"][1]["type"], "ValueError")
        self.assertTrue(record["chain"][0]["frames"])

    def test_startup_configuration_failure_prints_code_and_safe_guidance(self):
        environment = dict(os.environ, ASR_API_TIMEOUT_SECONDS="private-invalid-value", ASR_API_KEY="private-key")
        result = subprocess.run([sys.executable, "-m", "asr_worker", "--backend", "openai-compatible"], env=environment, capture_output=True, text=True, encoding="utf-8", timeout=10)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, "")
        self.assertIn("invalid_api_config", result.stderr)
        self.assertIn("ASR_API_TIMEOUT_SECONDS", result.stderr)
        self.assertNotIn("private-invalid-value", result.stderr)
        self.assertNotIn("private-key", result.stderr)

    def test_an_exception_with_a_broken_string_method_still_logs(self):
        class Broken(Exception):
            def __str__(self):
                raise RuntimeError("unprintable")
        with contextlib.redirect_stderr(io.StringIO()) as output:
            log_exception(Broken(), "internal")
        self.assertIn("details_redacted", output.getvalue())
