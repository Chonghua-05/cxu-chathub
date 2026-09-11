"""commands / status_render 模板的单元测试（不需要浏览器）。"""

from __future__ import annotations

import asyncio

import chatroom_bridge.status_render as status_render
from chatroom_bridge.commands import (
    CommandHandler,
    format_status,
    format_voice_channels,
    parse_command,
)

VOICE_DATA = {
    "total_users": 2,
    "channels": [
        {"server_name": "主节点", "channel_name": "大厅", "users": ["玩家A", "甲"]},
    ],
}

STATUS_DATA = {
    "network_routes": [{"route_name": "节点A", "online": True, "latency": 21.5, "packet_loss": 0.0}],
    "servers": [
        {"server_name": "服务端A", "online": True, "online_players": ["• 玩家A"]},
        {"server_name": "服务端B", "online": False, "online_players": []},
    ],
}


def _handler(**kwargs) -> CommandHandler:
    handler = CommandHandler(**kwargs)
    async def fake_get_json(url: str):
        return VOICE_DATA if "voice" in url else STATUS_DATA
    handler._get_json = fake_get_json  # type: ignore[assignment]
    return handler


def test_parse_command_forms():
    assert parse_command("/chatroom") == ("chatroom", "")
    assert parse_command("/status 额外") == ("status", "额外")
    assert parse_command("/Server@10000") == ("server", "")
    assert parse_command("chatroom") is None
    assert parse_command("") is None


def test_format_voice_channels_empty():
    assert format_voice_channels({"channels": [], "total_users": 0}) == "当前没有人在语音频道中。"
    assert format_voice_channels(None) == "无法获取语音频道信息。"


def test_chatroom_command_returns_text():
    handler = _handler(allow_all=True)
    result = asyncio.run(handler.handle("/chatroom", user_id=1))
    assert result is not None and result.image is None
    assert "在线人数" in result.text and "大厅" in result.text


def test_status_falls_back_to_text_without_renderer():
    handler = _handler(allow_all=True, status_image=True)

    async def boom(data):
        raise RuntimeError("no browser")

    status_render.render_status_png = boom  # type: ignore[assignment]
    try:
        result = asyncio.run(handler.handle("/status", user_id=1))
    finally:
        del status_render.render_status_png
    assert result is not None and result.image is None
    assert "服务端A" in result.text


def test_status_returns_image_when_renderer_available():
    handler = _handler(allow_all=True, status_image=True)

    async def fake_png(data, addresses=None):
        assert data is STATUS_DATA
        assert addresses  # 地址列表由命令层下发
        return b"\x89PNG-fake"

    status_render.render_status_png = fake_png  # type: ignore[assignment]
    try:
        result = asyncio.run(handler.handle("/server", user_id=1))
    finally:
        del status_render.render_status_png
    assert result is not None and result.image == b"\x89PNG-fake"


def test_status_is_alias_of_server():
    """命令名统一为 /server；/status 只作兼容别名，统计也归到 server。"""
    handler = _handler(allow_all=True)
    result = asyncio.run(handler.handle("/status", user_id=1))
    assert result is not None and "服务端A" in result.text
    assert handler.stats == {"server": 1}


def test_permission_denied_is_silent():
    handler = _handler(allow_all=False, allow_from=[42])
    assert asyncio.run(handler.handle("/chatroom", user_id=1)) is None
    assert asyncio.run(handler.handle("/chatroom", user_id=42)) is not None


def test_unknown_command_ignored():
    handler = _handler()
    assert asyncio.run(handler.handle("/hello", user_id=1)) is None
    assert asyncio.run(handler.handle("普通文本", user_id=1)) is None


def test_format_status_text_card():
    text = format_status(STATUS_DATA)
    assert "服务端A" in text and "在线 1 人: 玩家A" in text
    assert "服务端B" in text and "❌" in text


def test_status_html_template_renders_values():
    html = status_render.build_status_html(STATUS_DATA, background_base64=None)
    assert "game.example.com" in html
    assert "服务端A" in html
    assert "玩家A" in html
    assert "linear-gradient" in html  # 没背景图时用渐变兜底
    assert "{routes}" not in html and "{servers}" not in html
