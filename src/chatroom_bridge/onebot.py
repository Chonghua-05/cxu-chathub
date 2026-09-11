"""OneBot v11 反向 WS 服务端 + 动作调用。

NapCat 作为客户端连入 ``ws://<host>:<port><path>``：

* 校验 token（``Authorization: Bearer <token>`` 或 ``?access_token=``）
* 收 ``post_type=message``/``message_type=group`` 事件，解析为 :class:`GroupMessage`
* 通过同一条连接发送动作（``send_group_msg`` 等），按 ``echo`` 匹配响应

设计上与旧框架的 aiocqhttp 平台完全解耦：不再依赖任何框架对象。
"""

from __future__ import annotations

import asyncio
import itertools
import json
import logging
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable

from aiohttp import WSMsgType, web

log = logging.getLogger(__name__)

GroupMessageHandler = Callable[["OneBotConnection", "GroupMessage"], Awaitable[None]]


@dataclass
class Segment:
    """OneBot 消息段。"""

    type: str
    data: dict[str, Any] = field(default_factory=dict)

    @property
    def text(self) -> str:
        return str(self.data.get("text", ""))

    @property
    def file(self) -> str:
        return str(self.data.get("file", ""))

    @property
    def url(self) -> str:
        return str(self.data.get("url", ""))

    @property
    def summary(self) -> str:
        return str(self.data.get("summary", ""))


@dataclass
class GroupMessage:
    """群消息事件。"""

    group_id: int
    user_id: int
    message_id: int
    nickname: str
    card: str
    segments: list[Segment]
    raw: dict[str, Any] = field(default_factory=dict)

    @property
    def display_name(self) -> str:
        return self.card or self.nickname or str(self.user_id)

    @property
    def text(self) -> str:
        return "".join(s.text for s in self.segments if s.type == "text").strip()

    @property
    def reply_message_id(self) -> str | None:
        for seg in self.segments:
            if seg.type == "reply":
                value = seg.data.get("id")
                return str(value) if value else None
        return None

    @property
    def images(self) -> list[Segment]:
        return [s for s in self.segments if s.type == "image"]

    @property
    def at_user_ids(self) -> list[int]:
        users = []
        for seg in self.segments:
            if seg.type == "at":
                qq = seg.data.get("qq")
                if isinstance(qq, (int, str)) and str(qq).isdigit():
                    users.append(int(qq))
        return users

    def describe(self) -> str:
        return f"群={self.group_id} 用户={self.display_name}({self.user_id}) 消息ID={self.message_id}"


def parse_segments(message: Any) -> list[Segment]:
    """把 OneBot message 字段解析成 Segment 列表。"""
    if isinstance(message, str):
        return [Segment("text", {"text": message})] if message else []
    segments: list[Segment] = []
    if isinstance(message, list):
        for item in message:
            if not isinstance(item, dict):
                continue
            seg_type = item.get("type")
            if not seg_type:
                continue
            data = item.get("data")
            segments.append(Segment(str(seg_type), dict(data) if isinstance(data, dict) else {}))
    return segments


def parse_group_message(raw: dict) -> GroupMessage | None:
    """从原始事件里提取群消息；非群消息返回 None。"""
    if raw.get("post_type") != "message" or raw.get("message_type") != "group":
        return None
    sender = raw.get("sender") if isinstance(raw.get("sender"), dict) else {}
    return GroupMessage(
        group_id=int(raw.get("group_id") or 0),
        user_id=int(raw.get("user_id") or 0),
        message_id=int(raw.get("message_id") or 0),
        nickname=str(sender.get("nickname") or ""),
        card=str(sender.get("card") or ""),
        segments=parse_segments(raw.get("message")),
        raw=raw,
    )


class OneBotConnection:
    """一条已建立的 OneBot 连接。动作调用与事件接收共用这条 WS。"""

    def __init__(self, ws: web.WebSocketResponse, *, call_timeout: float = 20.0) -> None:
        self._ws = ws
        self._call_timeout = call_timeout
        self._counter = itertools.count(1)
        self._pending: dict[str, asyncio.Future] = {}
        self.self_id: int = 0

    @property
    def connected(self) -> bool:
        return not self._ws.closed

    async def call(self, action: str, **params: Any) -> Any:
        if self._ws.closed:
            raise ConnectionError("OneBot 连接已关闭")
        echo = str(next(self._counter))
        future: asyncio.Future = asyncio.get_running_loop().create_future()
        self._pending[echo] = future
        try:
            await self._ws.send_json({"action": action, "params": params, "echo": echo})
            return await asyncio.wait_for(future, self._call_timeout)
        finally:
            self._pending.pop(echo, None)

    def feed(self, payload: dict) -> bool:
        """把动作响应喂给等待中的 future；不是响应则返回 False。"""
        echo = payload.get("echo")
        if echo is None:
            return False
        future = self._pending.get(str(echo))
        if future is None or future.done():
            return False
        status = payload.get("status")
        retcode = payload.get("retcode")
        if status == "ok" or retcode == 0:
            future.set_result(payload.get("data"))
        else:
            detail = payload.get("message") or payload.get("wording") or ""
            future.set_exception(RuntimeError(f"OneBot 动作失败 retcode={retcode} {detail}"))
        return True

    # --- 常用动作 ---
    async def send_group_msg(self, group_id: int, message: list[dict[str, Any]]) -> Any:
        return await self.call("send_group_msg", group_id=int(group_id), message=message)

    async def send_group_text(self, group_id: int, text: str) -> Any:
        return await self.send_group_msg(group_id, [{"type": "text", "data": {"text": text}}])

    async def send_group_image(self, group_id: int, file_ref: str) -> Any:
        return await self.send_group_msg(group_id, [{"type": "image", "data": {"file": file_ref}}])

    async def get_image(self, file: str) -> Any:
        """取图片的真实下载地址（供转发到 chatroom 时上传）。"""
        return await self.call("get_image", file=file)


class OneBotServer:
    """aiohttp 上的 OneBot 反向 WS 服务端。"""

    def __init__(
        self,
        host: str,
        port: int,
        *,
        path: str = "/ws",
        access_token: str = "",
        on_group_message: GroupMessageHandler | None = None,
    ) -> None:
        if not path.startswith("/"):
            path = "/" + path
        self._host = host
        self._port = int(port)
        self._path = path
        self._access_token = access_token
        self._on_group_message = on_group_message
        self._connection: OneBotConnection | None = None
        self._runner: web.AppRunner | None = None
        self._queue: asyncio.Queue[tuple[OneBotConnection, GroupMessage]] | None = None
        self._consumer: asyncio.Task | None = None
        self.stats = {"connections": 0, "group_messages": 0}
        self._app = web.Application()
        self._app.router.add_get("/healthz", self._healthz)
        self._app.router.add_get(self._path, self._ws_handler)

    # --- 生命周期 ---
    async def start(self) -> None:
        self._runner = web.AppRunner(self._app)
        await self._runner.setup()
        site = web.TCPSite(self._runner, self._host, self._port)
        await site.start()
        self._queue = asyncio.Queue()
        self._consumer = asyncio.create_task(self._consume(), name="onebot-events")
        log.info("OneBot 反向 WS 服务已启动: ws://%s:%d%s", self._host, self._port, self._path)

    async def stop(self) -> None:
        if self._consumer is not None:
            self._consumer.cancel()
            try:
                await self._consumer
            except asyncio.CancelledError:
                pass
            self._consumer = None
        if self._runner is not None:
            await self._runner.cleanup()
            self._runner = None

    async def _consume(self) -> None:
        """顺序处理事件。

        事件必须在读取循环之外处理：处理器经常会通过同一条 WS 调用动作
        （发回复等），若在读取循环里 await 处理器，echo 响应就永远读不到，
        会一直卡到动作超时。
        """
        assert self._queue is not None
        while True:
            conn, message = await self._queue.get()
            if self._on_group_message is None:
                continue
            try:
                await self._on_group_message(conn, message)
            except asyncio.CancelledError:
                raise
            except Exception:  # noqa: BLE001
                log.exception("处理群消息失败: %s", message.describe())

    @property
    def connection(self) -> OneBotConnection | None:
        conn = self._connection
        return conn if conn and conn.connected else None

    async def wait_connection(self, timeout: float) -> OneBotConnection | None:
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while loop.time() < deadline:
            conn = self.connection
            if conn is not None:
                return conn
            await asyncio.sleep(0.5)
        return self.connection

    # --- 请求处理 ---
    def _authorized(self, request: web.Request) -> bool:
        if not self._access_token:
            return True
        header = request.headers.get("Authorization", "")
        if header.startswith("Bearer ") and header[len("Bearer ") :] == self._access_token:
            return True
        return request.query.get("access_token") == self._access_token

    async def _healthz(self, request: web.Request) -> web.Response:
        conn = self.connection
        return web.json_response(
            {
                "status": "ok",
                "onebot_connected": conn is not None,
                "self_id": conn.self_id if conn else 0,
                "stats": self.stats,
            }
        )

    async def _ws_handler(self, request: web.Request) -> web.StreamResponse:
        if not self._authorized(request):
            log.warning("OneBot 连接被拒绝：token 校验失败")
            return web.Response(status=401, text="unauthorized")

        ws = web.WebSocketResponse(heartbeat=30.0, max_msg_size=16 * 1024 * 1024)
        await ws.prepare(request)
        conn = OneBotConnection(ws)
        self._connection = conn
        self.stats["connections"] += 1
        log.info("OneBot 客户端已连接（第 %d 次）", self.stats["connections"])

        try:
            async for msg in ws:
                if msg.type != WSMsgType.TEXT:
                    continue
                try:
                    payload = json.loads(msg.data)
                except (TypeError, ValueError):
                    continue
                if not isinstance(payload, dict):
                    continue
                if conn.feed(payload):
                    continue
                if payload.get("post_type") == "meta_event":
                    if payload.get("meta_event_type") == "lifecycle":
                        conn.self_id = int(payload.get("self_id") or 0)
                    continue
                message = parse_group_message(payload)
                if message is None:
                    continue
                self.stats["group_messages"] += 1
                if self._queue is not None:
                    self._queue.put_nowait((conn, message))
        finally:
            log.warning("OneBot 客户端已断开")
            if self._connection is conn:
                self._connection = None
        return ws
