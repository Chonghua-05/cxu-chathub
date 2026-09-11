"""chatroom 用户侧鉴权：refresh_token -> access_token（读方向用）。

写方向走官方 Forward Bot API（Bearer 静态 token），不需要这里；
读方向（`!q`、chatroom→游戏）仍需要普通用户 JWT。
"""

from __future__ import annotations

import base64
import json
import logging
import time
from typing import Any

import aiohttp

from .state import StateStore

log = logging.getLogger(__name__)

REFRESH_ENDPOINT = "/api/auth/refresh"
REFRESH_MARGIN = 300  # 提前 5 分钟刷新


def decode_jwt_payload(token: str) -> dict[str, Any]:
    """解析 JWT payload（不校验签名，只取 exp / user id）。"""
    try:
        payload_b64 = token.split(".")[1]
        payload_b64 += "=" * (-len(payload_b64) % 4)
        return json.loads(base64.urlsafe_b64decode(payload_b64))
    except (IndexError, ValueError):
        return {}


class ChatroomAuth:
    """维护 access_token，并把轮换后的 refresh_token 持久化。"""

    def __init__(
        self,
        base_url: str,
        refresh_token: str = "",
        *,
        store: StateStore | None = None,
        session: aiohttp.ClientSession | None = None,
        timeout: float = 15.0,
    ) -> None:
        self._base_url = base_url.rstrip("/")
        self._store = store
        self._session = session
        self._owns_session = session is None
        self._timeout = timeout
        self._config_refresh_token = refresh_token
        self._refresh_token = (store.refresh_token if store and store.refresh_token else refresh_token)
        self._access_token = ""
        self._expires_at = 0.0
        self._user_id: int | None = None

    async def start(self) -> None:
        if self._session is None:
            self._session = aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=self._timeout))

    async def close(self) -> None:
        if self._owns_session and self._session is not None:
            await self._session.close()
            self._session = None

    @property
    def access_token(self) -> str:
        return self._access_token

    @property
    def user_id(self) -> int | None:
        """bot 自己的 chatroom user id，用于过滤自身消息。"""
        return self._user_id

    @property
    def has_refresh_token(self) -> bool:
        return bool(self._refresh_token or self._config_refresh_token)

    async def ensure_token(self) -> bool:
        if self._access_token and time.time() < self._expires_at - REFRESH_MARGIN:
            return True
        if await self.refresh():
            return True
        # 内存里的 refresh_token 失效时，回退到配置里的初始值再试一次
        if self._config_refresh_token and self._refresh_token != self._config_refresh_token:
            log.warning("refresh_token 刷新失败，回退到配置文件中的初始值")
            previous, self._refresh_token = self._refresh_token, self._config_refresh_token
            if await self.refresh():
                return True
            self._refresh_token = previous
        return False

    async def refresh(self) -> bool:
        token = self._refresh_token or self._config_refresh_token
        if not token:
            log.warning("没有 refresh_token，读方向不可用（!q / chatroom→游戏）")
            return False
        session = self._session
        if session is None:
            await self.start()
            session = self._session
        assert session is not None

        try:
            async with session.post(
                f"{self._base_url}{REFRESH_ENDPOINT}", json={"refresh_token": token}
            ) as resp:
                if resp.status != 200:
                    body = (await resp.text())[:200]
                    log.error("refresh_token 刷新失败 HTTP %s: %s", resp.status, body)
                    if "invalid refresh token" in body.lower():
                        log.error(
                            "refresh_token 已失效：请重新登录 chatroom 后在 "
                            "Local Storage 取 refresh_token 更新到配置"
                        )
                    return False
                data = await resp.json(content_type=None)
        except (aiohttp.ClientError, TimeoutError) as exc:
            log.error("refresh_token 刷新异常: %s", exc)
            return False

        access = str(data.get("access_token") or "")
        if not access:
            log.error("刷新返回 200 但没有 access_token")
            return False

        self._access_token = access
        new_refresh = str(data.get("refresh_token") or "")
        if new_refresh:
            self._refresh_token = new_refresh
            if self._store is not None:
                self._store.set_refresh_token(new_refresh)

        payload = decode_jwt_payload(access)
        exp = payload.get("exp")
        self._expires_at = float(exp) if isinstance(exp, (int, float)) else time.time() + 1800
        for key in ("user_id", "uid", "id", "sub"):
            value = payload.get(key)
            if isinstance(value, int):
                self._user_id = value
                break
            if isinstance(value, str) and value.isdigit():
                self._user_id = int(value)
                break
        log.info("access_token 已刷新（user_id=%s）", self._user_id)
        return True
