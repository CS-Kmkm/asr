"""Request gates that run before multipart parsing or temporary-file spooling."""
from __future__ import annotations

import json
import secrets
from urllib.parse import urlsplit


class RequestGate:
    def __init__(self, app, *, api_key: str | None, allowed_hosts: set[str], max_body_bytes: int):
        self.app = app
        self.api_key = api_key
        self.allowed_hosts = {host.lower() for host in allowed_hosts}
        self.max_body_bytes = max_body_bytes

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            return await self.app(scope, receive, send)
        headers: dict[bytes, list[bytes]] = {}
        for key, value in scope.get("headers", []):
            headers.setdefault(key.lower(), []).append(value)

        async def reject(status: int, message: str, code: str = "invalid_request"):
            body = json.dumps({"error": {"message": message, "type": "invalid_request_error", "param": None, "code": code}}).encode()
            await send({"type": "http.response.start", "status": status, "headers": [(b"content-type", b"application/json"), (b"content-length", str(len(body)).encode())]})
            await send({"type": "http.response.body", "body": body})

        if len(headers.get(b"host", [])) != 1:
            return await reject(400, "Invalid Host header")
        authority = headers[b"host"][0].decode("latin-1").lower()
        try:
            address = urlsplit("http://" + authority)
            _ = address.port  # Reject malformed ports as well as hostnames.
            valid_host = address.hostname in self.allowed_hosts and not address.username and not address.password and not address.path and not address.query and not address.fragment
        except ValueError:
            valid_host = False
        if not valid_host:
            return await reject(400, "Host is not allowed")
        origins = headers.get(b"origin", [])
        if origins and (len(origins) != 1 or origins[0].decode("latin-1").lower() != f'{scope.get("scheme", "http")}://{authority}'):
            return await reject(403, "Origin is not allowed")
        if self.api_key is not None:
            values = headers.get(b"authorization", [])
            expected = f"Bearer {self.api_key}".encode()
            if len(values) != 1 or not secrets.compare_digest(values[0], expected):
                return await reject(401, "Invalid API key", "invalid_api_key")
        if scope.get("method") != "POST" or scope.get("path") != "/v1/audio/transcriptions":
            return await self.app(scope, receive, send)
        lengths = headers.get(b"content-length", [])
        if lengths:
            try:
                if len(lengths) != 1 or not lengths[0].isdigit():
                    raise ValueError()
                declared = int(lengths[0])
            except ValueError:
                return await reject(400, "Invalid Content-Length")
            if declared > self.max_body_bytes:
                return await reject(413, "Audio request is too large")

        # Bounded memory, no disk writes. Count the actual stream even if the
        # client omits or lies about Content-Length. Multipart parsing starts
        # only after the complete request has passed this gate.
        chunks: list[bytes] = []
        size = 0
        while True:
            message = await receive()
            if message["type"] == "http.disconnect":
                return
            chunk = message.get("body", b"")
            size += len(chunk)
            if size > self.max_body_bytes:
                return await reject(413, "Audio request is too large")
            chunks.append(chunk)
            if not message.get("more_body", False):
                break
        body = b"".join(chunks)
        chunks.clear()
        delivered = False

        async def replay():
            nonlocal delivered
            if not delivered:
                delivered = True
                return {"type": "http.request", "body": body, "more_body": False}
            return await receive()

        return await self.app(scope, replay, send)
