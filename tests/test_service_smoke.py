"""端到端装配烟测：真实 WS 连接 + 真实 OneBotServer，chatroom 侧用假 API。

验证的是我们自己的接线：token 校验 → 事件解析 → 去重 → 转发调用 → 命令回群。
"""

from __future__ import annotations

import asyncio
import json
import socket
from typing import Any

import aiohttp

from chatroom_bridge.config import AppConfig, ChatroomConfig, CommandsConfig, OneBotConfig
from chatroom_bridge.main import BridgeService

GROUP_ID = 123456789
USER_ID = 10001
TOKEN = "secret-token"


class FakeForwardApi:
    def __init__(self) -> None:
        self.posts: list[dict[str, Any]] = []
        self.uploads: list[str] = []
        self.started = False

    async def start(self) -> None:
        self.started = True

    async def close(self) -> None:
        return None

    async def upload(self, data: bytes, filename: str, content_type: str | None = None) -> int:
        self.uploads.append(filename)
        return len(self.uploads)

    async def post_message(self, **kwargs: Any) -> dict[str, Any]:
        self.posts.append(kwargs)
        return {"id": 1000 + len(self.posts)}


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def make_config(tmp_path, port: int) -> AppConfig:
    return AppConfig(
        onebot=OneBotConfig(listen_host="127.0.0.1", listen_port=port, access_token=TOKEN, self_id=10000),
        chatroom=ChatroomConfig(base_url="https://example.invalid", channel_id=1, group_ids=[GROUP_ID]),
        commands=CommandsConfig(group_allow_all=True),
        state_path=str(tmp_path / "state.json"),
        log_level="WARNING",
    )


def group_event(text: str, message_id: int, *, segments: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    return {
        "post_type": "message",
        "message_type": "group",
        "self_id": 10000,
        "group_id": GROUP_ID,
        "user_id": USER_ID,
        "message_id": message_id,
        "sender": {"user_id": USER_ID, "nickname": "玩家A", "card": ""},
        "message": segments or [{"type": "text", "data": {"text": text}}],
    }


async def _scenario(tmp_path) -> dict[str, Any]:
    port = free_port()
    cfg = make_config(tmp_path, port)
    api = FakeForwardApi()
    service = BridgeService(cfg, forward_api=api)  # type: ignore[arg-type]
    # 命令层不联网：直接把查询结果打桩
    async def fake_get_json(url: str) -> Any:
        return {"channels": [{"channel_name": "大厅", "server_name": "主节点", "users": ["玩家A"]}], "total_users": 1}

    service.commands._get_json = fake_get_json  # type: ignore[assignment]
    await service.start()

    received: list[dict[str, Any]] = []
    try:
        async with aiohttp.ClientSession() as session:
            headers = {"Authorization": f"Bearer {TOKEN}"}
            async with session.ws_connect(f"http://127.0.0.1:{port}/ws", headers=headers) as ws:
                async def pump() -> None:
                    async for raw in ws:
                        if raw.type != aiohttp.WSMsgType.TEXT:
                            continue
                        payload = json.loads(raw.data)
                        received.append(payload)
                        if "action" in payload:
                            await ws.send_json({"status": "ok", "retcode": 0, "data": {}, "echo": payload.get("echo")})

                pump_task = asyncio.create_task(pump())
                await ws.send_json(group_event("你好 chatroom", 111))
                await ws.send_json(group_event("你好 chatroom", 111))  # 重复投递，应被去重
                await ws.send_json(group_event("/chatroom", 112))
                await asyncio.sleep(0.6)
                pump_task.cancel()

        # token 错误必须被拒（握手直接失败）
        async with aiohttp.ClientSession() as session:
            try:
                ws = await session.ws_connect(
                    f"http://127.0.0.1:{port}/ws", headers={"Authorization": "Bearer wrong"}
                )
            except aiohttp.WSServerHandshakeError as exc:
                rejected = exc.status == 401
            else:
                await ws.close()
                rejected = False
    finally:
        await service.stop()

    return {"api": api, "received": received, "rejected": rejected, "state": service.state.snapshot()}


def test_end_to_end_wiring(tmp_path):
    result = asyncio.run(_scenario(tmp_path))
    api = result["api"]

    # 只转发一次（去重生效）
    assert len(api.posts) == 1
    post = api.posts[0]
    assert post["source"] == "qq"
    assert post["content"] == "你好 chatroom"
    assert post["source_message_id"] == "111"
    assert post["sender_qq"] == USER_ID
    assert post["nickname"] == "玩家A"
    assert result["state"].forwarded_count == 1

    # 命令回群走真实 send_group_msg 动作
    actions = [p for p in result["received"] if p.get("action") == "send_group_msg"]
    assert actions, "应有一条命令响应"
    text = actions[0]["params"]["message"][0]["data"]["text"]
    assert "在线人数" in text and "大厅" in text

    # 错误 token 被拒
    assert result["rejected"] is True
