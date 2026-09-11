"""chatroom 官方 Forward Bot API 客户端（写方向，Bearer token）。

文档: backend-go/docs/forward-bot-api.md
  POST /api/forward/channels/{id}/upload   -> {id, filename, content_type, size, url}
  POST /api/forward/channels/{id}/messages -> 201 完整消息对象

服务端不去重：同一 source_message_id 重复提交会产生新消息，去重由调用方负责。
"""

from __future__ import annotations

import os
from typing import Any, Iterable

import aiohttp

MAX_FILE_SIZE = 10 * 1024 * 1024
BLOCKED_EXTENSIONS = {
    ".exe", ".bat", ".cmd", ".com", ".cpl", ".dll", ".scr", ".msi", ".jar",
    ".sh", ".bash", ".ps1", ".vbs", ".js", ".wsf", ".apk", ".app", ".deb", ".rpm",
}

_MIME_BY_EXT = {
    ".png": "image/png", ".jpg": "image/jpeg", ".jpeg": "image/jpeg",
    ".gif": "image/gif", ".webp": "image/webp", ".bmp": "image/bmp",
    ".mp4": "video/mp4", ".webm": "video/webm", ".mov": "video/quicktime",
    ".mp3": "audio/mpeg", ".wav": "audio/wav", ".ogg": "audio/ogg",
    ".pdf": "application/pdf", ".txt": "text/plain", ".zip": "application/zip",
}


class ForwardApiError(RuntimeError):
    def __init__(self, message: str, *, status: int | None = None, body: str = "") -> None:
        super().__init__(message)
        self.status = status
        self.body = body


def guess_content_type(filename: str) -> str:
    return _MIME_BY_EXT.get(os.path.splitext(filename)[1].lower(), "application/octet-stream")


def is_blocked(filename: str) -> bool:
    return os.path.splitext(filename)[1].lower() in BLOCKED_EXTENSIONS


class ForwardApi:
    """官方 forward 接口封装。token 只放在请求头，不写日志。"""

    def __init__(
        self,
        base_url: str,
        token: str,
        channel_id: int,
        *,
        session: aiohttp.ClientSession | None = None,
        timeout: float = 30.0,
    ) -> None:
        self._base_url = base_url.rstrip("/")
        self._token = token
        self._channel_id = int(channel_id)
        self._session = session
        self._owns_session = session is None
        self._timeout = timeout

    async def start(self) -> None:
        if self._session is None:
            self._session = aiohttp.ClientSession(
                timeout=aiohttp.ClientTimeout(total=self._timeout, sock_connect=5)
            )

    async def close(self) -> None:
        if self._owns_session and self._session is not None:
            await self._session.close()
            self._session = None

    @property
    def configured(self) -> bool:
        return bool(self._token) and self._channel_id > 0

    def _headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self._token}"}

    def _url(self, suffix: str) -> str:
        return f"{self._base_url}/api/forward/channels/{self._channel_id}{suffix}"

    async def upload(self, data: bytes, filename: str, content_type: str | None = None) -> int:
        """上传附件，返回 attachment id（≤10MB，可执行扩展名会被服务端拒绝）。"""
        if not self.configured:
            raise ForwardApiError("forward token / channel_id 未配置")
        if not data:
            raise ForwardApiError("附件为空")
        if len(data) > MAX_FILE_SIZE:
            raise ForwardApiError(f"附件超过 10MB 上限（{len(data)} 字节）")
        if is_blocked(filename):
            raise ForwardApiError(f"被禁止的扩展名: {os.path.splitext(filename)[1]}")

        session = self._session
        if session is None:
            await self.start()
            session = self._session
        assert session is not None

        content_type = content_type or guess_content_type(filename)
        form = aiohttp.FormData()
        form.add_field("file", data, filename=filename, content_type=content_type)
        async with session.post(self._url("/upload"), data=form, headers=self._headers()) as resp:
            body = await resp.text()
            if resp.status != 200:
                raise ForwardApiError(f"上传失败 HTTP {resp.status}", status=resp.status, body=body[:300])
            try:
                payload = await resp.json(content_type=None)
            except Exception as exc:  # noqa: BLE001
                raise ForwardApiError(f"上传响应不是 JSON: {body[:200]}", status=resp.status) from exc

        attachment_id = payload.get("id")
        if not isinstance(attachment_id, int):
            raise ForwardApiError(f"上传响应缺少 id: {payload}")
        return attachment_id

    async def post_message(
        self,
        *,
        source: str = "qq",
        content: str = "",
        source_message_id: str = "",
        sender_qq: int | None = None,
        sender_username: str = "",
        nickname: str = "",
        reply_source_message_id: str = "",
        reply_nickname: str = "",
        reply_content: str = "",
        attachment_ids: Iterable[int] | None = None,
    ) -> dict[str, Any]:
        """转发一条消息；content 与 attachment_ids 至少要有一个非空。"""
        if source not in ("qq", "game"):
            raise ForwardApiError(f"source 必须是 qq 或 game，收到 {source!r}")
        attachments = [int(a) for a in (attachment_ids or [])]
        if not content.strip() and not attachments:
            raise ForwardApiError("content 与 attachment_ids 不能同时为空")
        if not self.configured:
            raise ForwardApiError("forward token / channel_id 未配置")

        sender: dict[str, Any] = {}
        if sender_qq:
            sender["qq"] = int(sender_qq)
        if sender_username:
            sender["username"] = sender_username
        if nickname:
            sender["nickname"] = nickname

        payload: dict[str, Any] = {
            "source": source,
            "sender": sender,
            "content": content,
            "source_message_id": str(source_message_id or ""),
            "attachment_ids": attachments,
        }
        if reply_source_message_id:
            payload["reply"] = {
                "source_message_id": str(reply_source_message_id),
                "nickname": reply_nickname,
                "content": reply_content,
            }

        session = self._session
        if session is None:
            await self.start()
            session = self._session
        assert session is not None

        async with session.post(self._url("/messages"), json=payload, headers=self._headers()) as resp:
            body = await resp.text()
            if resp.status != 201:
                raise ForwardApiError(f"转发失败 HTTP {resp.status}", status=resp.status, body=body[:300])
            try:
                return await resp.json(content_type=None)
            except Exception as exc:  # noqa: BLE001
                raise ForwardApiError(f"转发响应不是 JSON: {body[:200]}", status=resp.status) from exc
