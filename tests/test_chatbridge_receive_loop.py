"""ChatBridge 接收循环：连接被对端关闭时必须跳出重连，不能空转。"""

from __future__ import annotations

import asyncio

from chatroom_bridge.chatbridge import ChatBridgeClient


class EofReader:
    """模拟对端已关闭：readexactly 立即抛 IncompleteReadError。"""

    def __init__(self) -> None:
        self.calls = 0

    async def readexactly(self, n: int) -> bytes:
        self.calls += 1
        raise asyncio.IncompleteReadError(partial=b"", expected=n)


def _client_with_reader(reader) -> ChatBridgeClient:
    client = ChatBridgeClient("127.0.0.1", 21027, "web", "pw", "ThisIstheSecret")
    client._reader = reader  # noqa: SLF001 - 直接注入假 reader
    client._running = True  # noqa: SLF001
    client._connected = True  # noqa: SLF001
    return client


def test_closed_connection_exits_receive_loop():
    """回归：对端关闭后 _receive_loop 必须立刻返回（旧实现会 100% CPU 死循环）。"""

    async def run() -> int:
        reader = EofReader()
        client = _client_with_reader(reader)
        await asyncio.wait_for(client._receive_loop(), timeout=3)  # noqa: SLF001
        return reader.calls

    assert asyncio.run(run()) == 1


class FlakyReader:
    """模拟解密/解析持续失败的坏包。"""

    def __init__(self) -> None:
        self.calls = 0

    async def readexactly(self, n: int) -> bytes:
        self.calls += 1
        if self.calls > 1:
            return b"\x00\x00\x00\x00"
        return b"\x00\x00\x00\x01"


def test_repeated_errors_break_loop():
    async def run() -> int:
        reader = FlakyReader()
        client = _client_with_reader(reader)

        async def boom(packet):
            raise ValueError("坏包")

        client._receive_raw = lambda: _raw(reader)  # type: ignore[assignment]  # noqa: SLF001
        client._dispatch = boom  # type: ignore[assignment]  # noqa: SLF001
        await asyncio.wait_for(client._receive_loop(), timeout=3)  # noqa: SLF001
        return reader.calls

    async def _raw(reader):
        await reader.readexactly(4)
        return {}

    assert asyncio.run(run()) == 5
