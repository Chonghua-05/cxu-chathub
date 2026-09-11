"""事件处理器里调用 OneBot 动作必须能拿到 echo 响应。

回归：早期实现把事件处理器直接 ``await`` 在 WS 读取循环里。处理器再用同一条连接
调用动作（发回复等）时，echo 响应永远读不到，只能等动作超时（默认 20s）才报错——
线上表现就是「命令响应已生成但发送失败」，群里其实收到了消息，日志却报错。
"""

from __future__ import annotations

import asyncio
import json
import socket
import time
from typing import Any

import aiohttp

from chatroom_bridge.onebot import OneBotServer

TOKEN = "test-token"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def group_event(message_id: int = 1) -> dict[str, Any]:
    return {
        "post_type": "message",
        "message_type": "group",
        "self_id": 1,
        "group_id": 123456789,
        "user_id": 10001,
        "message_id": message_id,
        "sender": {"nickname": "玩家A"},
        "message": [{"type": "text", "data": {"text": "/chatroom"}}],
    }


def test_action_call_inside_handler_resolves():
    async def scenario() -> tuple[list[Any], float]:
        port = free_port()
        results: list[Any] = []

        async def handler(conn, msg) -> None:
            results.append(await conn.call("get_status"))

        server = OneBotServer("127.0.0.1", port, access_token=TOKEN, on_group_message=handler)
        await server.start()
        started = time.monotonic()
        try:
            async with aiohttp.ClientSession() as session:
                async with session.ws_connect(
                    f"http://127.0.0.1:{port}/ws", headers={"Authorization": f"Bearer {TOKEN}"}
                ) as ws:
                    async def pump() -> None:
                        async for raw in ws:
                            if raw.type != aiohttp.WSMsgType.TEXT:
                                continue
                            payload = json.loads(raw.data)
                            if "action" in payload:
                                await ws.send_json(
                                    {
                                        "status": "ok",
                                        "retcode": 0,
                                        "data": {"detail": "pong"},
                                        "echo": payload["echo"],
                                    }
                                )

                    pump_task = asyncio.create_task(pump())
                    await ws.send_json(group_event())
                    for _ in range(60):  # 最多等 3 秒
                        if results:
                            break
                        await asyncio.sleep(0.05)
                    pump_task.cancel()
        finally:
            await server.stop()
        return results, time.monotonic() - started

    results, elapsed = asyncio.run(scenario())
    assert results == [{"detail": "pong"}], "处理器必须拿到动作响应"
    assert elapsed < 3, f"动作往返不应等到超时（实际 {elapsed:.1f}s）"


def test_event_order_is_preserved():
    """连续多条事件必须按到达顺序处理。"""

    async def scenario() -> list[int]:
        port = free_port()
        seen: list[int] = []

        async def handler(conn, msg) -> None:
            seen.append(msg.message_id)
            await asyncio.sleep(0.02 if msg.message_id == 1 else 0)

        server = OneBotServer("127.0.0.1", port, access_token=TOKEN, on_group_message=handler)
        await server.start()
        try:
            async with aiohttp.ClientSession() as session:
                async with session.ws_connect(
                    f"http://127.0.0.1:{port}/ws", headers={"Authorization": f"Bearer {TOKEN}"}
                ) as ws:
                    for mid in (1, 2, 3):
                        await ws.send_json(group_event(mid))
                    for _ in range(60):
                        if len(seen) >= 3:
                            break
                        await asyncio.sleep(0.05)
        finally:
            await server.stop()
        return seen

    assert asyncio.run(scenario()) == [1, 2, 3]
