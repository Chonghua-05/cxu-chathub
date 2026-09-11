"""读方向：轮询 chatroom 频道消息，供 `!q` 与 chatroom→游戏使用。"""

from __future__ import annotations

import logging
from typing import Any

import aiohttp

from .chatroom_auth import ChatroomAuth
from .state import StateStore

log = logging.getLogger(__name__)

MESSAGES_ENDPOINT = "/api/channels/{channel_id}/messages"
QQ_FORWARD_PREFIX = "!q"


def extract_qq_forward(content: str) -> str | None:
    """`!q xxx` -> `xxx`；不是 !q 消息返回 None。"""
    stripped = content.strip()
    if not stripped.lower().startswith(QQ_FORWARD_PREFIX):
        return None
    payload = stripped[len(QQ_FORWARD_PREFIX) :].strip()
    return payload or None


class ChatroomReader:
    """轮询频道消息并维护读游标（游标持久化在 state.json）。"""

    def __init__(
        self,
        base_url: str,
        channel_id: int,
        auth: ChatroomAuth,
        *,
        store: StateStore | None = None,
        session: aiohttp.ClientSession | None = None,
        fetch_limit: int = 10,
        skip_backlog_on_start: bool = True,
    ) -> None:
        self._base_url = base_url.rstrip("/")
        self._channel_id = int(channel_id)
        self._auth = auth
        self._store = store
        self._session = session
        self._owns_session = session is None
        self._fetch_limit = fetch_limit
        self._initialized = False
        self._skip_backlog = skip_backlog_on_start
        self._cursor = store.last_read_message_id if store is not None else 0

    async def start(self) -> None:
        if self._session is None:
            self._session = aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=15, sock_connect=5))

    async def close(self) -> None:
        if self._owns_session and self._session is not None:
            await self._session.close()
            self._session = None

    @property
    def cursor(self) -> int:
        return self._cursor

    def _advance(self, message_id: int) -> None:
        if message_id <= self._cursor:
            return
        self._cursor = message_id
        if self._store is not None:
            self._store.set_last_read_message_id(message_id)

    async def fetch_messages(self) -> list[dict[str, Any]] | None:
        """拉取最近消息（API 一般按新→旧返回）。"""
        if not await self._auth.ensure_token():
            return None
        session = self._session
        if session is None:
            await self.start()
            session = self._session
        assert session is not None

        url = f"{self._base_url}{MESSAGES_ENDPOINT.format(channel_id=self._channel_id)}"
        params = {"limit": self._fetch_limit}
        try:
            async with session.get(
                url, headers={"Authorization": f"Bearer {self._auth.access_token}"}, params=params
            ) as resp:
                if resp.status != 200:
                    log.warning("读取频道消息失败 HTTP %s", resp.status)
                    return None
                data = await resp.json(content_type=None)
        except (aiohttp.ClientError, TimeoutError) as exc:
            log.warning("读取频道消息异常: %s", exc)
            return None

        if isinstance(data, list):
            return data
        if isinstance(data, dict):
            for key in ("data", "messages", "items"):
                value = data.get(key)
                if isinstance(value, list):
                    return value
        return []

    async def poll_once(self) -> list[dict[str, Any]]:
        """返回本次新消息（按旧→新排序）。首次调用只初始化游标。"""
        messages = await self.fetch_messages()
        if messages is None:
            return []

        if not self._initialized:
            self._initialized = True
            if self._skip_backlog:
                newest = max((int(m.get("id") or 0) for m in messages), default=0)
                self._advance(newest)
                log.info("读方向初始化完成，游标=%s（历史消息不重放）", self._cursor)
                return []

        fresh: list[dict[str, Any]] = []
        for message in messages:
            message_id = int(message.get("id") or 0)
            if message_id <= self._cursor:
                continue
            fresh.append(message)
        fresh.sort(key=lambda m: int(m.get("id") or 0))
        for message in fresh:
            self._advance(int(message.get("id") or 0))
        return fresh

    def is_own_message(self, message: dict[str, Any]) -> bool:
        bot_id = self._auth.user_id
        return bot_id is not None and message.get("user_id") == bot_id
