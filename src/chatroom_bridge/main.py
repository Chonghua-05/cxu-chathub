"""服务入口：装配 OneBot 服务端、chatroom 双向同步、游戏桥接与命令。

运行：``python -m chatroom_bridge.main --config /app/config.json``
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import itertools
import logging
import signal
import time
from typing import Any

from .bridge import ChatroomForwarder
from .chatbridge import ChatBridgeClient
from .chatroom_auth import ChatroomAuth
from .chatroom_read import ChatroomReader, QQ_FORWARD_PREFIX, extract_qq_forward
from .commands import CommandHandler
from .config import AppConfig, describe, load_config
from .forward_api import ForwardApi, ForwardApiError
from .onebot import GroupMessage, OneBotConnection, OneBotServer
from .player_tracker import PlayerEvent, PlayerTracker
from .state import StateStore

log = logging.getLogger("chatroom_bridge")

CHATROOM_READ_INTERVAL = 10
PLAYER_POLL_INTERVAL = 30


def setup_logging(level: str) -> None:
    logging.basicConfig(
        level=getattr(logging, level.upper(), logging.INFO),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )


class BridgeService:
    """把 OneBot 事件、chatroom 读方向、ChatBridge 与玩家追踪接到一起。"""

    def __init__(self, cfg: AppConfig, *, forward_api: Any | None = None) -> None:
        self.cfg = cfg
        self.state = StateStore(cfg.state_path)
        self.forward_api = forward_api or ForwardApi(
            cfg.chatroom.base_url, cfg.chatroom.forward_token, cfg.chatroom.channel_id
        )
        self.forwarder = ChatroomForwarder(
            self.forward_api,
            self.state,
            enabled=cfg.chatroom.qq_sync_enabled,
            self_id=cfg.onebot.self_id,
        )
        self.auth = ChatroomAuth(
            cfg.chatroom.base_url, cfg.chatroom.refresh_token, store=self.state
        )
        self.reader = ChatroomReader(
            cfg.chatroom.base_url, cfg.chatroom.channel_id, self.auth, store=self.state
        )
        self.tracker = PlayerTracker(
            debounce_count=cfg.chatroom.debounce_count,
            on_event=self.on_player_event,
            status_api=cfg.chatroom.status_api,
        )
        self.commands = CommandHandler(
            allow_all=cfg.commands.group_allow_all,
            allow_from=cfg.commands.allow_from,
            status_image=cfg.commands.status_image,
            voice_api=cfg.chatroom.voice_api,
            status_api=cfg.chatroom.status_api,
            server_addresses=cfg.chatroom.server_addresses,
        )
        self.server = OneBotServer(
            cfg.onebot.listen_host,
            cfg.onebot.listen_port,
            path=cfg.onebot.path,
            access_token=cfg.onebot.access_token,
            on_group_message=self.on_group_message,
        )
        self.chatbridge: ChatBridgeClient | None = None
        if cfg.chatbridge.enabled and cfg.chatbridge.host:
            self.chatbridge = ChatBridgeClient(
                cfg.chatbridge.host,
                cfg.chatbridge.port,
                cfg.chatbridge.name,
                cfg.chatbridge.password,
                cfg.chatbridge.aes_key,
            )
            self.chatbridge.set_on_chat(self.on_game_chat)

        self._group_ids = cfg.group_ids
        self._tasks: list[asyncio.Task] = []
        self._game_seq = itertools.count(1)

    # --- 生命周期 ---
    async def start(self) -> None:
        await self.forward_api.start()
        await self.forwarder.start()
        await self.auth.start()
        await self.reader.start()
        await self.tracker.start()
        await self.commands.start()
        await self.server.start()

        if self.reader._auth.has_refresh_token:  # noqa: SLF001 - 内部状态，启动日志用
            self._tasks.append(asyncio.create_task(self._chatroom_loop(), name="chatroom-read"))
        else:
            log.warning("未配置 refresh_token：!q 与 chatroom→游戏 已禁用")
        if self.cfg.chatroom.player_tracking_enabled:
            self._tasks.append(asyncio.create_task(self._player_loop(), name="player-tracker"))
        if self.chatbridge is not None:
            self._tasks.append(asyncio.create_task(self.chatbridge.run(), name="chatbridge"))
        log.info("服务已启动；状态: %s", self.state.snapshot())

    async def stop(self) -> None:
        for task in self._tasks:
            task.cancel()
        for task in self._tasks:
            try:
                await task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
        self._tasks.clear()
        if self.chatbridge is not None:
            self.chatbridge.stop()
        await self.server.stop()
        await self.commands.close()
        await self.tracker.close()
        await self.reader.close()
        await self.auth.close()
        await self.forwarder.close()
        await self.forward_api.close()

    # --- QQ 群事件 ---
    async def on_group_message(self, conn: OneBotConnection, msg: GroupMessage) -> None:
        if self._group_ids and msg.group_id not in self._group_ids:
            return

        reply = await self.commands.handle(msg.text, user_id=msg.user_id)
        if reply is not None and not reply.empty:
            try:
                if reply.image is not None:
                    log.info("命令响应 -> 群 %s: [图片 %d 字节]", msg.group_id, len(reply.image))
                    await conn.send_group_image(
                        msg.group_id, "base64://" + base64.b64encode(reply.image).decode("ascii")
                    )
                else:
                    log.info("命令响应 -> 群 %s: %.40s", msg.group_id, reply.text.replace("\n", " "))
                    await conn.send_group_text(msg.group_id, reply.text)
            except Exception:  # noqa: BLE001
                log.exception("发送命令响应失败")
            return

        await self.forwarder.handle(conn, msg)

    async def send_to_qq_groups(self, text: str) -> bool:
        """向配置的 QQ 群发文本（!q 转发用）。"""
        conn = self.server.connection
        if conn is None:
            log.warning("QQ 未连接，!q 转发跳过: %.40s", text)
            return False
        sent = False
        for group_id in sorted(self._group_ids):
            try:
                await conn.send_group_text(group_id, text)
                sent = True
                log.info("!q 转发到 QQ 群 %s: %.60s", group_id, text)
            except Exception:  # noqa: BLE001
                log.exception("!q 转发到 QQ 群 %s 失败", group_id)
        return sent

    # --- chatroom 读方向 ---
    async def _chatroom_loop(self) -> None:
        while True:
            try:
                messages = await self.reader.poll_once()
                for message in messages:
                    await self._dispatch_chatroom_message(message)
            except asyncio.CancelledError:
                raise
            except Exception:  # noqa: BLE001
                log.exception("chatroom 轮询异常")
            await asyncio.sleep(CHATROOM_READ_INTERVAL)

    async def _dispatch_chatroom_message(self, message: dict[str, Any]) -> None:
        content = str(message.get("content") or "").strip()
        if not content:
            return
        username = str(message.get("username") or "")
        if self.reader.is_own_message(message):
            return

        # !q -> QQ 群
        if self.cfg.chatroom.qq_forward_enabled:
            payload = extract_qq_forward(content)
            if payload:
                label = f"[Chatroom] {username}" if username else "[Chatroom]"
                await self.send_to_qq_groups(f"{label}: {payload}")

        # chatroom -> 游戏（原始消息照常广播）
        if (
            self.cfg.chatroom.qq_to_game_enabled
            and self.chatbridge is not None
            and self.chatbridge.is_connected
        ):
            game_msg = f"[Chatroom] {username}: {content}" if username else f"[Chatroom] {content}"
            await self.chatbridge.broadcast_chat(game_msg)

    # --- 游戏 -> chatroom ---
    async def on_game_chat(self, sender: str, author: str, message: str) -> None:
        """ChatBridge 收到游戏内聊天：转发到 chatroom，并处理 !q。"""
        content = message.strip()
        if not content:
            return
        if author:
            text = f"🎮 [{sender}] {author}: {content}"
        else:
            text = f"🟢 {content}"
        await self._forward_game_message(
            content=text,
            nickname=author or sender,
            username=author,
        )

        if author and content.lower().startswith(QQ_FORWARD_PREFIX):
            payload = extract_qq_forward(content)
            if payload:
                await self.send_to_qq_groups(f"[游戏|{sender}] {author}: {payload}")

        if self._is_snapshot_notice(sender, content):
            prefix = str(self.cfg.chatroom.snapshot_prefix or "!snap")
            payload = content[len(prefix) :].strip()
            if payload:
                await self.send_to_qq_groups(f"[快照服] {payload}")

    def _is_snapshot_notice(self, sender: str, content: str) -> bool:
        """快照服的更新通知：只认指定 ChatBridge 客户端名，玩家无法冒用。"""
        if not self.cfg.chatroom.qq_forward_enabled:
            return False
        expected = str(self.cfg.chatroom.snapshot_sender or "")
        prefix = str(self.cfg.chatroom.snapshot_prefix or "!snap").lower()
        return bool(expected) and sender == expected and content.lower().startswith(prefix)

    async def on_player_event(self, event: PlayerEvent) -> bool:
        """玩家上下线 -> chatroom（写方向走官方 forward API）。"""
        return await self._forward_game_message(
            content=event.format_message(),
            nickname=event.player,
            username=event.player,
            source_id_prefix="game-event",
        )

    async def _forward_game_message(
        self,
        *,
        content: str,
        nickname: str,
        username: str,
        source_id_prefix: str = "game-chat",
    ) -> bool:
        if not self.forward_api.configured:
            return False
        source_id = f"{source_id_prefix}-{int(time.time() * 1000)}-{next(self._game_seq)}"
        try:
            await self.forward_api.post_message(
                source="game",
                content=content,
                source_message_id=source_id,
                sender_username=username,
                nickname=nickname,
            )
            return True
        except ForwardApiError as exc:
            log.warning("游戏侧消息转发失败: %s", exc)
            return False

    # --- 玩家轮询 ---
    async def _player_loop(self) -> None:
        while True:
            try:
                events = await self.tracker.poll_once()
                for event in events:
                    log.info("已推送: %s | %s @ %s", event.type, event.player, event.server)
            except asyncio.CancelledError:
                raise
            except Exception:  # noqa: BLE001
                log.exception("玩家状态轮询异常")
            await asyncio.sleep(PLAYER_POLL_INTERVAL)


async def _run(config_path: str) -> None:
    cfg = load_config(config_path)
    setup_logging(cfg.log_level)
    log.info("配置摘要: %s", describe(cfg))
    if not cfg.chatroom.forward_token:
        log.warning("chatroom.forward_token 为空：QQ→chatroom 转发会被跳过，直到配置 token")

    service = BridgeService(cfg)
    await service.start()

    stop_event = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, stop_event.set)
        except NotImplementedError:  # pragma: no cover - 非 POSIX
            pass

    conn = await service.server.wait_connection(timeout=30)
    if conn is None:
        log.warning("30 秒内没有 OneBot 客户端连入，请检查 NapCat 的 websocketClients 配置")
    else:
        log.info("OneBot 已就绪 self_id=%s", conn.self_id or cfg.onebot.self_id)

    await stop_event.wait()
    log.info("收到停止信号，正在关闭...")
    await service.stop()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="cxu-chathub 消息同步桥")
    parser.add_argument("--config", default="/app/config.json", help="配置文件路径")
    args = parser.parse_args(argv)
    try:
        asyncio.run(_run(args.config))
    except KeyboardInterrupt:  # pragma: no cover
        return 130
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
