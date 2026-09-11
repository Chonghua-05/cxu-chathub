"""ChatBridge 异步客户端（从旧框架插件原样移植，去掉框架依赖）。

协议参考: https://github.com/TISUnion/ChatBridge
- TCP 传输，4 字节长度前缀 + AES-CBC 加密 JSON
- 登录握手 → keep-alive → ChatPayload 收发
"""

from __future__ import annotations

import asyncio
import json
import logging
import struct
from binascii import a2b_hex, b2a_hex
from hashlib import sha256
from typing import Any, Callable

from Crypto.Cipher import AES

log = logging.getLogger(__name__)

PACKET_TYPE_KEEP_ALIVE = "chatbridge.keep_alive"
PACKET_TYPE_CHAT = "chatbridge.chat"
SERVER_NAME = "#SERVER"

KEEP_ALIVE_INTERVAL = 60
KEEP_ALIVE_TIMEOUT = 15
RECONNECT_DELAY = 5
CONNECT_TIMEOUT = 20


class AESCryptor:
    """与 ChatBridge 兼容的 AES-CBC（SHA256 派生密钥、零填充、hex 输出）。"""

    def __init__(self, key: str) -> None:
        self._key_empty = len(key) == 0
        self._hashed_key: bytes = sha256(self._to_16_length(key)).digest()

    def _get_cipher(self):
        return AES.new(self._hashed_key, AES.MODE_CBC, self._hashed_key[:16])

    @staticmethod
    def _to_16_length(text: str) -> bytes:
        data = text.encode("utf-8")
        pad = (16 - (len(data) % 16)) % 16
        return data + b"\0" * pad

    def encrypt(self, text: str) -> bytes:
        if self._key_empty:
            return text.encode("utf-8")
        return b2a_hex(self._get_cipher().encrypt(self._to_16_length(text)))

    def decrypt(self, data: bytes) -> str:
        if self._key_empty:
            return data.decode("utf-8")
        return self._get_cipher().decrypt(a2b_hex(data)).decode("utf-8").rstrip("\0")


class ChatBridgeClient:
    """异步 ChatBridge 客户端：断线自动重连，回调订阅聊天消息。"""

    def __init__(
        self,
        host: str,
        port: int,
        name: str,
        password: str,
        aes_key: str = "ThisIstheSecret",
    ) -> None:
        self._host = host
        self._port = int(port)
        self._name = name
        self._password = password
        self._cryptor = AESCryptor(aes_key)

        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        self._running = False
        self._connected = False
        self._pong_event = asyncio.Event()

        self._on_chat: Callable[[str, str, str], Any] | None = None
        self._on_connected: Callable[[], Any] | None = None
        self._on_disconnected: Callable[[], Any] | None = None

    # --- 回调 ---
    def set_on_chat(self, callback: Callable[[str, str, str], Any]) -> None:
        self._on_chat = callback

    def set_on_connected(self, callback: Callable[[], Any]) -> None:
        self._on_connected = callback

    def set_on_disconnected(self, callback: Callable[[], Any]) -> None:
        self._on_disconnected = callback

    @property
    def is_connected(self) -> bool:
        return self._connected

    # --- 生命周期 ---
    async def run(self) -> None:
        self._running = True
        while self._running:
            try:
                await self._connect_and_login()
                self._connected = True
                if self._on_connected:
                    await self._call_callback(self._on_connected)
                log.info("ChatBridge 已连接: %s:%s", self._host, self._port)

                keep_alive_task = asyncio.create_task(self._keep_alive_loop())
                try:
                    await self._receive_loop()
                finally:
                    keep_alive_task.cancel()
                    try:
                        await keep_alive_task
                    except asyncio.CancelledError:
                        pass
            except asyncio.CancelledError:
                break
            except Exception as exc:  # noqa: BLE001
                log.error("ChatBridge 连接异常: %s", exc)
            finally:
                self._connected = False
                await self._close()
                if self._on_disconnected:
                    await self._call_callback(self._on_disconnected)
                if not self._running:
                    break
                log.info("ChatBridge %ss 后重连...", RECONNECT_DELAY)
                await asyncio.sleep(RECONNECT_DELAY)

    def stop(self) -> None:
        self._running = False

    # --- 发送 ---
    async def send_chat(self, target: str, message: str, author: str = "") -> None:
        await self._send_packet(
            {
                "sender": self._name,
                "receivers": [target],
                "broadcast": False,
                "type": PACKET_TYPE_CHAT,
                "payload": {"author": author, "message": message},
            }
        )

    async def broadcast_chat(self, message: str, author: str = "") -> None:
        await self._send_packet(
            {
                "sender": self._name,
                "receivers": [],
                "broadcast": True,
                "type": PACKET_TYPE_CHAT,
                "payload": {"author": author, "message": message},
            }
        )

    # --- 内部 ---
    async def _connect_and_login(self) -> None:
        log.info("ChatBridge 正在连接 %s:%s ...", self._host, self._port)
        try:
            await asyncio.wait_for(self._connect_and_login_inner(), CONNECT_TIMEOUT)
        except asyncio.TimeoutError as exc:
            raise ConnectionError(f"ChatBridge 握手超时（{CONNECT_TIMEOUT}s）") from exc
        log.info("ChatBridge 登录成功（客户端名 %s）", self._name)

    async def _connect_and_login_inner(self) -> None:
        self._reader, self._writer = await asyncio.open_connection(self._host, self._port)
        await self._send_raw({"name": self._name, "password": self._password})
        result = await self._receive_raw()
        if result.get("message") != "ok":
            raise ConnectionError(f"ChatBridge 登录失败: {result}")

    async def _receive_loop(self) -> None:
        consecutive_errors = 0
        while self._running and self._connected:
            try:
                packet = await self._receive_raw()
                await self._dispatch(packet)
                consecutive_errors = 0
            except asyncio.CancelledError:
                break
            except (ConnectionError, OSError, asyncio.IncompleteReadError, EOFError) as exc:
                # IncompleteReadError/EOFError 也意味着对端已关闭连接：必须跳出重连，
                # 否则 readexactly 会立即再次抛错，形成不 await 让出的死循环（会把事件循环打满）。
                log.warning("ChatBridge 连接断开: %s", exc)
                break
            except Exception as exc:  # noqa: BLE001
                consecutive_errors += 1
                log.error("ChatBridge 处理消息异常: %s", exc)
                if consecutive_errors >= 5:
                    log.warning("ChatBridge 连续 %d 次处理异常，按连接断开处理并重连", consecutive_errors)
                    break

    async def _dispatch(self, packet: dict) -> None:
        ptype = packet.get("type", "")
        sender = packet.get("sender", "")
        payload = packet.get("payload", {})
        if not isinstance(payload, dict):
            return

        if ptype == PACKET_TYPE_KEEP_ALIVE:
            ping_type = payload.get("ping_type", "")
            if ping_type == "ping":
                await self._send_packet(
                    {
                        "sender": self._name,
                        "receivers": [sender],
                        "broadcast": False,
                        "type": PACKET_TYPE_KEEP_ALIVE,
                        "payload": {"ping_type": "pong"},
                    }
                )
            elif ping_type == "pong":
                self._pong_event.set()
        elif ptype == PACKET_TYPE_CHAT:
            author = str(payload.get("author", ""))
            message = str(payload.get("message", ""))
            if message and self._on_chat:
                await self._call_callback(self._on_chat, sender, author, message)

    async def _keep_alive_loop(self) -> None:
        while self._running and self._connected:
            await asyncio.sleep(KEEP_ALIVE_INTERVAL)
            if not self._connected:
                break
            self._pong_event.clear()
            try:
                await self._send_packet(
                    {
                        "sender": self._name,
                        "receivers": [SERVER_NAME],
                        "broadcast": False,
                        "type": PACKET_TYPE_KEEP_ALIVE,
                        "payload": {"ping_type": "ping"},
                    }
                )
                await asyncio.wait_for(self._pong_event.wait(), timeout=KEEP_ALIVE_TIMEOUT)
            except asyncio.TimeoutError:
                log.warning("ChatBridge keep-alive 超时，主动断开重连")
                await self._close()
                break

    async def _send_packet(self, packet: dict) -> None:
        if not self._connected or self._writer is None:
            return
        try:
            await self._send_raw(packet)
        except Exception as exc:  # noqa: BLE001
            log.warning("ChatBridge 发送失败: %s", exc)

    async def _send_raw(self, data: dict) -> None:
        if self._writer is None:
            raise ConnectionError("writer is None")
        encrypted = self._cryptor.encrypt(json.dumps(data, ensure_ascii=False))
        self._writer.write(struct.pack("I", len(encrypted)) + encrypted)
        await self._writer.drain()

    async def _receive_raw(self) -> dict:
        if self._reader is None:
            raise ConnectionError("reader is None")
        header = await self._reader.readexactly(4)
        remaining = struct.unpack("I", header)[0]
        chunks: list[bytes] = []
        while remaining > 0:
            chunk = await self._reader.read(remaining)
            if not chunk:
                raise ConnectionError("连接断开")
            chunks.append(chunk)
            remaining -= len(chunk)
        return json.loads(self._cryptor.decrypt(b"".join(chunks)))

    async def _close(self) -> None:
        self._connected = False
        if self._writer:
            try:
                self._writer.close()
                await self._writer.wait_closed()
            except Exception:  # noqa: BLE001
                pass
            self._writer = None
            self._reader = None

    async def _call_callback(self, callback: Callable, *args: Any) -> Any:
        try:
            result = callback(*args)
            if asyncio.iscoroutine(result):
                return await result
            return result
        except Exception as exc:  # noqa: BLE001
            log.error("ChatBridge 回调异常: %s", exc)
            return None
