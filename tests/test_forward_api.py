"""forward_api 模块测试：使用假 session，验证请求形状与错误处理（不联网）。"""

from __future__ import annotations

import asyncio
import json
from typing import Any

import pytest

from chatroom_bridge.forward_api import ForwardApi, ForwardApiError


class FakeResponse:
    def __init__(self, status: int, payload: Any = None, text: str = "") -> None:
        self.status = status
        self._payload = payload
        self._text = text if payload is None else json.dumps(payload)

    async def text(self) -> str:
        return self._text

    async def json(self, content_type: str | None = None) -> Any:
        return self._payload

    async def __aenter__(self) -> "FakeResponse":
        return self

    async def __aexit__(self, *exc: object) -> None:
        return None


class FakeSession:
    def __init__(self, response: FakeResponse) -> None:
        self.response = response
        self.calls: list[dict[str, Any]] = []

    def post(self, url: str, **kwargs: Any) -> FakeResponse:
        self.calls.append({"url": url, "kwargs": kwargs})
        return self.response


def make_api(response: FakeResponse, token: str = "tok", channel: int = 1) -> tuple[ForwardApi, FakeSession]:
    session = FakeSession(response)
    api = ForwardApi("https://chatroom.example.com", token, channel, session=session)  # type: ignore[arg-type]
    return api, session


def test_post_message_payload_shape():
    api, session = make_api(FakeResponse(201, {"id": 1}))
    result = asyncio.run(
        api.post_message(
            source="qq",
            content="你好",
            source_message_id="1234567890",
            sender_qq=10001,
            nickname="玩家A",
            reply_source_message_id="987654321",
            reply_nickname="玩家A",
            reply_content="引用内容",
        )
    )
    assert result == {"id": 1}
    call = session.calls[0]
    assert call["url"].endswith("/api/forward/channels/1/messages")
    assert call["kwargs"]["headers"]["Authorization"] == "Bearer tok"
    body = call["kwargs"]["json"]
    assert body["source"] == "qq"
    assert body["sender"] == {"qq": 10001, "nickname": "玩家A"}
    assert body["source_message_id"] == "1234567890"
    assert body["reply"]["source_message_id"] == "987654321"
    assert body["attachment_ids"] == []


def test_post_message_requires_content_or_attachment():
    api, _ = make_api(FakeResponse(201, {"id": 1}))
    with pytest.raises(ForwardApiError):
        asyncio.run(api.post_message(source="qq", content="   "))


def test_post_message_rejects_bad_source():
    api, _ = make_api(FakeResponse(201, {"id": 1}))
    with pytest.raises(ForwardApiError):
        asyncio.run(api.post_message(source="discord", content="x"))


def test_post_message_surfaces_401_without_leaking_token():
    api, _ = make_api(FakeResponse(401, None, text='{"error":"bad token"}'))
    with pytest.raises(ForwardApiError) as excinfo:
        asyncio.run(api.post_message(source="qq", content="x", source_message_id="1"))
    assert excinfo.value.status == 401
    assert "tok" not in str(excinfo.value)


def test_upload_returns_id_and_rejects_oversize():
    api, session = make_api(FakeResponse(200, {"id": 77, "filename": "a.png"}))
    assert asyncio.run(api.upload(b"\x89PNG", "a.png", "image/png")) == 77
    assert session.calls[0]["url"].endswith("/api/forward/channels/1/upload")

    with pytest.raises(ForwardApiError):
        asyncio.run(api.upload(b"x" * (10 * 1024 * 1024 + 1), "big.zip"))


def test_upload_rejects_blocked_extension():
    api, _ = make_api(FakeResponse(200, {"id": 1}))
    with pytest.raises(ForwardApiError):
        asyncio.run(api.upload(b"MZ", "evil.exe"))


def test_unconfigured_token_fails_fast():
    api, _ = make_api(FakeResponse(201, {"id": 1}), token="")
    assert api.configured is False
    with pytest.raises(ForwardApiError):
        asyncio.run(api.post_message(source="qq", content="x"))
