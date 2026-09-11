"""持久化状态：去重表、读游标、refresh_token。原子写，避免半截文件。"""

from __future__ import annotations

import json
import os
import tempfile
import threading
from dataclasses import dataclass
from typing import Any


@dataclass
class StateSnapshot:
    forwarded_count: int
    last_read_message_id: int
    has_refresh_token: bool


class StateStore:
    """去重表 + 读游标 + refresh_token 的小型 JSON 存储。

    去重表是必需的：chatroom 服务端明确不去重，同一 source_message_id
    重复提交会产生新消息。
    """

    def __init__(self, path: str, max_forwarded: int = 2000) -> None:
        self._path = path
        self._max_forwarded = max_forwarded
        self._lock = threading.Lock()
        self._data: dict[str, Any] = {
            "forwarded": {},
            "last_read_message_id": 0,
            "refresh_token": "",
        }
        self.load()

    # --- 载入 / 落盘 ---
    def load(self) -> None:
        try:
            with open(self._path, "r", encoding="utf-8") as fh:
                data = json.load(fh)
            if isinstance(data, dict):
                self._data.update(data)
        except FileNotFoundError:
            pass
        except (json.JSONDecodeError, OSError):
            pass  # 状态文件损坏不应阻塞启动，重建即可
        self._data.setdefault("forwarded", {})
        self._data.setdefault("last_read_message_id", 0)
        self._data.setdefault("refresh_token", "")

    def _flush(self) -> None:
        directory = os.path.dirname(os.path.abspath(self._path)) or "."
        os.makedirs(directory, exist_ok=True)
        fd, tmp = tempfile.mkstemp(dir=directory, prefix=".state-", suffix=".tmp")
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as fh:
                json.dump(self._data, fh, ensure_ascii=False, indent=2)
                fh.flush()
                os.fsync(fh.fileno())
            os.replace(tmp, self._path)
        except BaseException:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise

    # --- 去重表 ---
    def forwarded_id(self, source_message_id: str) -> int | None:
        with self._lock:
            value = self._data["forwarded"].get(str(source_message_id))
        return int(value) if value is not None else None

    def already_forwarded(self, source_message_id: str) -> bool:
        return self.forwarded_id(source_message_id) is not None

    def mark_forwarded(self, source_message_id: str, chatroom_message_id: int) -> None:
        with self._lock:
            forwarded: dict[str, Any] = self._data["forwarded"]
            forwarded[str(source_message_id)] = int(chatroom_message_id)
            while len(forwarded) > self._max_forwarded:
                forwarded.pop(next(iter(forwarded)))
            self._flush()

    # --- 读游标 ---
    @property
    def last_read_message_id(self) -> int:
        return int(self._data.get("last_read_message_id") or 0)

    def set_last_read_message_id(self, message_id: int) -> None:
        with self._lock:
            self._data["last_read_message_id"] = int(message_id)
            self._flush()

    # --- refresh_token ---
    @property
    def refresh_token(self) -> str:
        return str(self._data.get("refresh_token") or "")

    def set_refresh_token(self, token: str) -> None:
        with self._lock:
            self._data["refresh_token"] = token
            self._flush()

    def snapshot(self) -> StateSnapshot:
        with self._lock:
            return StateSnapshot(
                forwarded_count=len(self._data["forwarded"]),
                last_read_message_id=int(self._data["last_read_message_id"]),
                has_refresh_token=bool(self._data["refresh_token"]),
            )
