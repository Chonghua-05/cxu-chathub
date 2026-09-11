"""QQ 群命令：/chatroom、/server（/status 为兼容别名）。

命令都不需要 AI，纯 HTTP 查询 + 格式化后回群。
"""

from __future__ import annotations

import base64
import logging
from dataclasses import dataclass
from typing import Any

import aiohttp

log = logging.getLogger(__name__)

# 默认地址仅作示例，实际部署请通过 config.json 的 chatroom.* 覆盖
DEFAULT_VOICE_API = "https://chatroom.example.com/api/voice/qqbot/get_voice_channel_people"
DEFAULT_STATUS_API = "https://status.example.com/api/qqbot/status"
DEFAULT_SERVER_ADDRESSES: list[tuple[str, str]] = [
    ("主IP", "game.example.com"),
    ("备用地址", "backup.example.com:25565"),
]

COMMAND_TIMEOUT = aiohttp.ClientTimeout(total=20, sock_connect=5, sock_read=15)


@dataclass
class CommandResult:
    """命令响应：文本或图片（图片优先）。"""

    text: str = ""
    image: bytes | None = None

    @property
    def empty(self) -> bool:
        return not self.text and self.image is None


def parse_command(text: str) -> tuple[str, str] | None:
    """识别命令，返回 (命令名, 参数)；不是命令则返回 None。"""
    text = text.strip()
    if not text:
        return None
    head, _, rest = text.partition(" ")
    if not head.startswith("/"):
        return None
    name = head[1:].split("@", 1)[0].lower()  # 容忍 /cmd@bot 形式
    return name, rest.strip()


def format_voice_channels(data: Any) -> str:
    """格式化语音频道在线人员（与旧插件行为一致）。"""
    if not isinstance(data, dict):
        return "无法获取语音频道信息。"
    channels = data.get("channels", [])
    total_users = data.get("total_users", 0)
    if not isinstance(channels, list) or not channels:
        return "当前没有人在语音频道中。"

    lines = [f"当前语音频道在线人数: {total_users}", ""]
    for channel in channels:
        if not isinstance(channel, dict):
            continue
        channel_name = channel.get("channel_name", "未知频道")
        server_name = channel.get("server_name", "未知服务器")
        users = channel.get("users", [])
        if not isinstance(users, list):
            users = []
        lines.append(f"【{server_name}】{channel_name} ({len(users)}人)")
        for user in users:
            lines.append(f"  - {user}")
        lines.append("")
    if len(lines) <= 2:
        return "当前没有人在语音频道中。"
    return "\n".join(lines).strip()


def format_status(data: Any) -> str:
    """把状态 API 的返回格式化成文本卡片（图片渲染见 status_render）。"""
    if not isinstance(data, dict):
        return "无法获取服务器状态。"
    lines: list[str] = ["服务器状态", ""]

    routes = data.get("network_routes") or []
    if routes:
        lines.append("【节点状态】")
        for route in routes:
            if not isinstance(route, dict):
                continue
            online = bool(route.get("online"))
            mark = "✅" if online else "❌"
            if online:
                detail = f"延迟 {route.get('latency', 0):.2f}ms / 丢包 {route.get('packet_loss', 0):.1f}%"
            else:
                detail = "离线"
            lines.append(f"{mark} {route.get('route_name', 'Unknown')} - {detail}")
        lines.append("")

    servers = data.get("servers") or []
    if servers:
        lines.append("【服务器状态】")
        for server in servers:
            if not isinstance(server, dict):
                continue
            online = bool(server.get("online"))
            mark = "✅" if online else "❌"
            players = server.get("online_players") or []
            players = [str(p).lstrip("• ").strip() for p in players] if online else []
            lines.append(f"{mark} {server.get('server_name', 'Unknown')}")
            if online:
                lines.append(f"   在线 {len(players)} 人: {', '.join(players) if players else '(无)'}")
        lines.append("")

    return "\n".join(lines).strip()


class CommandHandler:
    """命令分发：/chatroom、/server（/status 为兼容别名）。"""

    def __init__(
        self,
        *,
        session: aiohttp.ClientSession | None = None,
        allow_all: bool = True,
        allow_from: list[int] | None = None,
        status_image: bool = False,
        voice_api: str = DEFAULT_VOICE_API,
        status_api: str = DEFAULT_STATUS_API,
        server_addresses: list[list[str]] | list[tuple[str, str]] | None = None,
    ) -> None:
        self._session = session
        self._owns_session = session is None
        self._allow_all = allow_all
        self._allow_from = {int(x) for x in (allow_from or [])}
        self._status_image = status_image
        self._voice_api = voice_api
        self._status_api = status_api
        self._addresses = (
            [tuple(pair) for pair in server_addresses]
            if server_addresses
            else list(DEFAULT_SERVER_ADDRESSES)
        )
        self.stats: dict[str, int] = {}

    async def start(self) -> None:
        if self._session is None:
            self._session = aiohttp.ClientSession(timeout=COMMAND_TIMEOUT)

    async def close(self) -> None:
        if self._owns_session and self._session is not None:
            await self._session.close()
            self._session = None

    def allowed(self, user_id: int) -> bool:
        return self._allow_all or user_id in self._allow_from

    async def handle(self, text: str, *, user_id: int) -> CommandResult | None:
        """返回命令响应；None 表示不响应。"""
        parsed = parse_command(text)
        if parsed is None:
            return None
        name, _args = parsed
        if name not in ("chatroom", "server", "status"):
            return None
        if not self.allowed(user_id):
            return None
        if name == "status":  # 兼容别名：/status 与 /server 同义
            name = "server"
        self.stats[name] = self.stats.get(name, 0) + 1

        if name == "chatroom":
            return CommandResult(text=format_voice_channels(await self._get_json(self._voice_api)))

        data = await self._get_json(self._status_api)
        if self._status_image:
            image = await self._render_status(data)
            if image is not None:
                return CommandResult(image=image)
        return CommandResult(text=format_status(data))

    async def _render_status(self, data: Any) -> bytes | None:
        """渲染状态图；浏览器不可用或数据非法时返回 None（回退文本）。"""
        if not isinstance(data, dict):
            return None
        try:
            from .status_render import render_status_png
        except ImportError:  # pragma: no cover
            return None
        try:
            return await render_status_png(data, self._addresses)
        except Exception as exc:  # noqa: BLE001 - 渲染失败必须回退文本，不能拖垮命令
            log.warning("状态图渲染异常，回退文本: %s", exc)
            return None

    async def _get_json(self, url: str) -> Any:
        session = self._session
        if session is None:
            await self.start()
            session = self._session
        assert session is not None
        try:
            async with session.get(url) as resp:
                if resp.status != 200:
                    log.warning("命令查询失败 %s HTTP %s", url, resp.status)
                    return None
                return await resp.json(content_type=None)
        except (aiohttp.ClientError, TimeoutError) as exc:
            log.warning("命令查询异常 %s: %s", url, exc)
            return None
