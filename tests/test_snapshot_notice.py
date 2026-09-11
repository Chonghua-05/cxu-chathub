"""快照服更新通知：ChatBridge -> QQ 群 的路由规则。"""

from __future__ import annotations

import asyncio
from typing import Any

from chatroom_bridge.config import AppConfig, ChatroomConfig, CommandsConfig, OneBotConfig
from chatroom_bridge.main import BridgeService

GROUP_ID = 123456789


class FakeForwardApi:
    def __init__(self) -> None:
        self.posts: list[dict[str, Any]] = []
        self.configured = True

    async def start(self) -> None:
        return None

    async def close(self) -> None:
        return None

    async def post_message(self, **kwargs: Any) -> dict[str, Any]:
        self.posts.append(kwargs)
        return {"id": len(self.posts)}


def make_service(tmp_path, **chatroom_overrides: Any) -> tuple[BridgeService, FakeForwardApi, list[str]]:
    cfg = AppConfig(
        onebot=OneBotConfig(listen_host="127.0.0.1", listen_port=0),
        chatroom=ChatroomConfig(
            base_url="https://example.invalid", channel_id=1, group_ids=[GROUP_ID], **chatroom_overrides
        ),
        commands=CommandsConfig(),
        state_path=str(tmp_path / "state.json"),
        log_level="WARNING",
    )
    api = FakeForwardApi()
    service = BridgeService(cfg, forward_api=api)  # type: ignore[arg-type]
    sent: list[str] = []

    async def fake_send(text: str) -> bool:
        sent.append(text)
        return True

    service.send_to_qq_groups = fake_send  # type: ignore[assignment]
    return service, api, sent


def test_snapshot_notice_goes_to_qq_and_chatroom(tmp_path):
    async def run() -> tuple[list[str], FakeForwardApi]:
        service, api, sent = make_service(tmp_path)
        await service.on_game_chat("snapshot", "快照服", "!snap 已更新到快照 26.3-rc-1，服务器已就绪")
        return sent, api

    sent, api = asyncio.run(run())
    assert sent == ["[快照服] 已更新到快照 26.3-rc-1，服务器已就绪"]
    # 默认仍同步进 chatroom
    assert len(api.posts) == 1
    assert api.posts[0]["source"] == "game"


def test_player_cannot_impersonate_snapshot_notice(tmp_path):
    async def run() -> list[str]:
        service, _api, sent = make_service(tmp_path)
        # 玩家在自己服务器里打字，sender 是服务器名而不是 snapshot
        await service.on_game_chat("survival", "玩家A", "!snap 假的更新")
        return sent

    assert asyncio.run(run()) == []


def test_plain_game_chat_not_forwarded_to_qq(tmp_path):
    async def run() -> list[str]:
        service, _api, sent = make_service(tmp_path)
        await service.on_game_chat("snapshot", "玩家A", "普通聊天")
        return sent

    assert asyncio.run(run()) == []


def test_snapshot_notice_can_be_disabled(tmp_path):
    async def run() -> list[str]:
        service, _api, sent = make_service(tmp_path, qq_forward_enabled=False)
        await service.on_game_chat("snapshot", "快照服", "!snap 已更新")
        return sent

    assert asyncio.run(run()) == []
