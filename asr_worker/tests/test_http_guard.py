from __future__ import annotations

import asyncio
import importlib.util
import unittest
from unittest.mock import patch

from asr_worker.backends import BackendError, MockBackend
from asr_worker.http_guard import RequestGate
from asr_worker.tests.support import IsolatedWorkerTestCase


class RequestGateTests(IsolatedWorkerTestCase):
    def exercise(self, headers, chunks, *, key=None):
        received = 0
        invoked = False
        sent = []

        async def application(scope, receive, send):
            nonlocal invoked
            invoked = True
            await send({"type": "http.response.start", "status": 204})
            await send({"type": "http.response.body", "body": b""})

        async def receive():
            nonlocal received
            received += 1
            body = chunks.pop(0)
            return {"type": "http.request", "body": body, "more_body": bool(chunks)}

        async def send(message):
            sent.append(message)

        gate = RequestGate(application, api_key=key, allowed_hosts={"localhost"}, max_body_bytes=8)
        scope = {"type": "http", "scheme": "http", "method": "POST", "path": "/v1/audio/transcriptions", "headers": headers}
        asyncio.run(gate(scope, receive, send))
        return sent[0]["status"], received, invoked

    def test_authentication_rejects_before_reading_any_body(self):
        self.assertEqual(self.exercise([(b"host", b"localhost")], [], key="secret"), (401, 0, False))

    def test_actual_bytes_are_limited_with_missing_or_false_content_length(self):
        for extra in [[], [(b"content-length", b"1")]]:
            with self.subTest(headers=extra):
                self.assertEqual(self.exercise([(b"host", b"localhost")] + extra, [b"12345", b"67890"]), (413, 2, False))

    def test_declared_oversize_is_rejected_before_reading(self):
        self.assertEqual(self.exercise([(b"host", b"localhost"), (b"content-length", b"9")], []), (413, 0, False))

    def test_host_and_origin_are_checked_before_body_parsing(self):
        self.assertEqual(self.exercise([(b"host", b"attacker.invalid")], []), (400, 0, False))
        self.assertEqual(self.exercise([(b"host", b"localhost"), (b"origin", b"https://attacker.invalid")], []), (403, 0, False))
        self.assertEqual(self.exercise([(b"host", b"localhost"), (b"origin", b"http://localhost")], [b"ok"]), (204, 1, True))

    def test_nonloopback_bind_requires_a_key(self):
        from asr_worker.api import run_api_server
        with self.assertRaises(BackendError) as caught:
            run_api_server(MockBackend(), host="0.0.0.0", port=8000, quantization="4bit", api_key=None, served_model=None)
        self.assertEqual(caught.exception.code, "serve_auth_required")


@unittest.skipUnless(importlib.util.find_spec("fastapi"), "requires the serve extra")
class MultipartGateTests(IsolatedWorkerTestCase):
    def test_oversized_stream_never_creates_a_temporary_file(self):
        from fastapi.testclient import TestClient
        from asr_worker.api import create_app
        client = TestClient(create_app(MockBackend(), max_upload_bytes=4), base_url="http://localhost")
        for headers in [{}, {"Content-Length": "1"}]:
            with self.subTest(headers=headers), patch("starlette.formparsers.SpooledTemporaryFile") as spool, patch("asr_worker.api.tempfile.NamedTemporaryFile") as temporary:
                response = client.post("/v1/audio/transcriptions", content=iter([b"x" * 40_000, b"x" * 40_000]), headers=headers)
                self.assertEqual(response.status_code, 413)
                spool.assert_not_called()
                temporary.assert_not_called()

    def test_health_requires_auth_and_multipart_accepts_only_one_file(self):
        from fastapi.testclient import TestClient
        from asr_worker.api import create_app
        client = TestClient(create_app(MockBackend(), api_key="test"), base_url="http://localhost")
        self.assertEqual(client.get("/health").status_code, 401)
        self.assertEqual(client.get("/health", headers={"Authorization": "Bearer test"}).status_code, 200)
        response = client.post("/v1/audio/transcriptions", headers={"Authorization": "Bearer test"}, data={"model": "mock"}, files=[("file", ("a.wav", b"a")), ("file", ("b.wav", b"b"))])
        self.assertEqual(response.status_code, 400)


if __name__ == "__main__":
    unittest.main()
