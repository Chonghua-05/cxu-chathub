"""MC 玩家上下线追踪：轮询状态 API+ 状态驱动防抖。

从旧框架插件的 `_poll_once` 逻辑移植：只有连续 N 次轮询都看到同一变化
才确认事件，避免玩家瞬断/重连造成的事件抖动。
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Callable, Awaitable

import aiohttp

log = logging.getLogger(__name__)

STATUS_API = "https://status.example.com/api/qqbot/status"


@dataclass
class PlayerEvent:
    type: str  # online | offline
    server: str
    player: str
    timestamp: float = field(default_factory=time.time)

    def format_message(self, server_names: dict[str, str] | None = None) -> str:
        icon = "🎮" if self.type == "online" else "🚪"
        action = "上线" if self.type == "online" else "下线"
        clock = datetime.fromtimestamp(self.timestamp).strftime("%H:%M:%S")
        display_server = server_names.get(self.server, self.server) if server_names else self.server
        return f"{icon} {self.player} → {display_server} {action}  {clock}"


class PlayerTracker:
    """对比在线玩家快照，产生带防抖的上下线事件。"""

    def __init__(
        self,
        *,
        session: aiohttp.ClientSession | None = None,
        debounce_count: int = 2,
        on_event: Callable[[PlayerEvent], Awaitable[bool]] | None = None,
        status_api: str = STATUS_API,
    ) -> None:
        self._session = session
        self._owns_session = session is None
        self._debounce = max(1, int(debounce_count))
        self._on_event = on_event
        self._status_api = status_api
        self._confirmed: dict[str, set[str]] = {}
        self._pending: dict[tuple[str, str, str], int] = {}
        self.stats = {"online": 0, "offline": 0, "failed": 0}

    async def start(self) -> None:
        if self._session is None:
            self._session = aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=15, sock_connect=5))

    async def close(self) -> None:
        if self._owns_session and self._session is not None:
            await self._session.close()
            self._session = None

    async def fetch_raw(self) -> dict[str, set[str]] | None:
        """拉取当前在线玩家 {server: {player}}；失败返回 None。"""
        session = self._session
        if session is None:
            await self.start()
            session = self._session
        assert session is not None
        try:
            async with session.get(self._status_api) as resp:
                if resp.status != 200:
                    log.warning("状态 API返回 HTTP %s", resp.status)
                    return None
                data = await resp.json(content_type=None)
        except (aiohttp.ClientError, TimeoutError) as exc:
            log.warning("状态 API请求失败: %s", exc)
            return None
        if not isinstance(data, dict):
            return None
        result: dict[str, set[str]] = {}
        for server in data.get("servers", []) or []:
            if not isinstance(server, dict):
                continue
            name = str(server.get("server_name") or "unknown")
            players = {str(p) for p in (server.get("online_players") or []) if p}
            if players:
                result[name] = players
        return result

    async def poll_once(self, current: dict[str, set[str]] | None = None) -> list[PlayerEvent]:
        """跑一轮比对，返回本轮确认的事件。"""
        if current is None:
            current = await self.fetch_raw()
        if current is None:
            return []

        events: list[PlayerEvent] = []
        all_servers = set(current.keys()) | set(self._confirmed.keys())
        for server in all_servers:
            players_now = current.get(server, set())
            confirmed = self._confirmed.get(server, set())

            for player in sorted(players_now - confirmed):
                event = await self._maybe_confirm("online", server, player)
                if event:
                    events.append(event)

            for player in sorted(confirmed - players_now):
                event = await self._maybe_confirm("offline", server, player)
                if event:
                    events.append(event)

            # 误报抵消
            for player in players_now & confirmed:
                self._pending.pop((player, server, "offline"), None)
            for player in confirmed - players_now:
                self._pending.pop((player, server, "online"), None)

            # 清理跨轮次残留
            for key in [k for k in self._pending if k[1] == server]:
                player, _srv, kind = key
                if kind == "online" and player not in players_now:
                    del self._pending[key]
                elif kind == "offline" and player not in confirmed:
                    del self._pending[key]

        for server in [s for s, p in self._confirmed.items() if not p and s not in current]:
            del self._confirmed[server]

        return events

    async def _maybe_confirm(self, kind: str, server: str, player: str) -> PlayerEvent | None:
        key = (player, server, kind)
        self._pending[key] = self._pending.get(key, 0) + 1
        if self._pending[key] < self._debounce:
            return None

        event = PlayerEvent(kind, server, player)
        if self._on_event is not None:
            try:
                ok = await self._on_event(event)
            except Exception:  # noqa: BLE001
                log.exception("推送玩家事件失败")
                ok = False
            if not ok:
                self.stats["failed"] += 1
                return None  # 保留 pending，下轮再试

        del self._pending[key]
        if kind == "online":
            self._confirmed.setdefault(server, set()).add(player)
            self.stats["online"] += 1
        else:
            self._confirmed.get(server, set()).discard(player)
            self.stats["offline"] += 1
        return event

    def snapshot(self) -> dict[str, list[str]]:
        return {server: sorted(players) for server, players in self._confirmed.items()}

    def restore(self, snapshot: dict[str, Any]) -> None:
        if isinstance(snapshot, dict):
            self._confirmed = {str(k): {str(p) for p in (v or [])} for k, v in snapshot.items()}
