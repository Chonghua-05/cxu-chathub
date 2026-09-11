"""player_tracker / chatbridge / chatroom_read 的单元测试（全部离线）。"""

from __future__ import annotations

import asyncio

from chatroom_bridge.chatbridge import AESCryptor
from chatroom_bridge.chatroom_read import ChatroomReader, extract_qq_forward
from chatroom_bridge.player_tracker import PlayerEvent, PlayerTracker
from chatroom_bridge.state import StateStore


# ---------- ChatBridge AES ----------

def test_aes_roundtrip():
    cryptor = AESCryptor("ThisIstheSecret")
    payload = '{"sender": "web", "message": "你好 MC"}'
    assert cryptor.decrypt(cryptor.encrypt(payload)) == payload


def test_aes_empty_key_is_plaintext():
    cryptor = AESCryptor("")
    assert cryptor.encrypt("hello") == b"hello"
    assert cryptor.decrypt(b"hello") == "hello"


# ---------- !q 解析 ----------

def test_extract_qq_forward():
    assert extract_qq_forward("!q 大家好") == "大家好"
    assert extract_qq_forward("!Q 大小写不敏感") == "大小写不敏感"
    assert extract_qq_forward("  !q   多余空格  ") == "多余空格"
    assert extract_qq_forward("!q") is None
    assert extract_qq_forward("普通消息") is None
    assert extract_qq_forward("qq 没有感叹号") is None


# ---------- 玩家上下线防抖 ----------

class RecordingTracker(PlayerTracker):
    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        self.pushed: list[PlayerEvent] = []

    async def _push(self, event: PlayerEvent) -> bool:
        self.pushed.append(event)
        return True


def _tracker(debounce: int, ok: bool = True):
    pushed: list[PlayerEvent] = []

    async def on_event(event: PlayerEvent) -> bool:
        if ok:
            pushed.append(event)
        return ok

    tracker = PlayerTracker(debounce_count=debounce, on_event=on_event)
    return tracker, pushed


def test_online_requires_debounce_confirmation():
    async def scenario():
        tracker, pushed = _tracker(debounce=2)
        first = await tracker.poll_once({"survival": {"玩家A"}})
        second = await tracker.poll_once({"survival": {"玩家A"}})
        return first, second, pushed, tracker.snapshot()

    first, second, pushed, snapshot = asyncio.run(scenario())
    assert first == []                      # 第一次只是 pending
    assert [e.type for e in second] == ["online"]
    assert pushed[0].player == "玩家A"
    assert snapshot == {"survival": ["玩家A"]}


def test_offline_detected_after_debounce():
    async def scenario():
        tracker, pushed = _tracker(debounce=1)
        await tracker.poll_once({"survival": {"玩家A"}})
        events = await tracker.poll_once({})
        return events, pushed

    events, _ = asyncio.run(scenario())
    assert [e.type for e in events] == ["offline"]


def test_transient_blip_does_not_emit():
    """玩家一瞬消失又回来：不应产生任何事件。"""
    async def scenario():
        tracker, pushed = _tracker(debounce=2)
        await tracker.poll_once({"survival": {"玩家A"}})   # pending online=1
        await tracker.poll_once({"survival": {"玩家A"}})   # confirmed online
        await tracker.poll_once({})                       # pending offline=1
        await tracker.poll_once({"survival": {"玩家A"}})   # 回来了 -> pending 抵消
        return pushed

    pushed = asyncio.run(scenario())
    assert [e.type for e in pushed] == ["online"]


def test_push_failure_keeps_pending_for_retry():
    async def scenario():
        tracker, pushed = _tracker(debounce=1, ok=False)
        await tracker.poll_once({"survival": {"玩家A"}})
        assert tracker.snapshot() == {}            # 推送失败 -> 未确认
        assert tracker.stats["failed"] == 1
        tracker2, pushed2 = _tracker(debounce=1, ok=True)
        await tracker2.poll_once({"survival": {"玩家A"}})
        return pushed2

    pushed2 = asyncio.run(scenario())
    assert [e.type for e in pushed2] == ["online"]


def test_event_format_message():
    event = PlayerEvent("online", "survival", "玩家A", timestamp=0)
    text = event.format_message({"survival": "服务端A"})
    assert "玩家A" in text and "服务端A" in text and "上线" in text


# ---------- 读方向游标 ----------

class FakeAuth:
    def __init__(self, token="jwt", user_id=999):
        self._token = token
        self._user_id = user_id
        self.calls = 0

    @property
    def access_token(self):
        return self._token

    @property
    def user_id(self):
        return self._user_id

    async def ensure_token(self):
        self.calls += 1
        return True


class FakeSession:
    def __init__(self, batches):
        self._batches = list(batches)
        self.urls: list[str] = []

    def get(self, url, **kwargs):
        self.urls.append(url)
        batch = self._batches.pop(0) if self._batches else []

        class _Resp:
            status = 200

            async def json(self, content_type=None):
                return batch

            async def __aenter__(self):
                return self

            async def __aexit__(self, *exc):
                return None

        return _Resp()


def test_first_poll_skips_backlog_and_advances_cursor(tmp_path):
    store = StateStore(str(tmp_path / "state.json"))
    session = FakeSession([[
        {"id": 30, "content": "新", "user_id": 1, "username": "甲"},
        {"id": 20, "content": "旧", "user_id": 1, "username": "甲"},
    ]])
    reader = ChatroomReader("https://x.invalid", 1, FakeAuth(), store=store, session=session)  # type: ignore[arg-type]

    assert asyncio.run(reader.poll_once()) == []
    assert reader.cursor == 30
    assert store.last_read_message_id == 30

    # 下一轮只返回游标之后的消息，并按旧→新排序
    session._batches.append([
        {"id": 31, "content": "b", "user_id": 2, "username": "乙"},
        {"id": 33, "content": "c", "user_id": 2, "username": "乙"},
        {"id": 30, "content": "新", "user_id": 1, "username": "甲"},
    ])
    fresh = asyncio.run(reader.poll_once())
    assert [m["id"] for m in fresh] == [31, 33]
    assert reader.cursor == 33


def test_cursor_persists_across_restart(tmp_path):
    store = StateStore(str(tmp_path / "state.json"))
    session = FakeSession([[{"id": 42, "content": "x", "user_id": 1}]])
    reader = ChatroomReader("https://x.invalid", 1, FakeAuth(), store=store, session=session)  # type: ignore[arg-type]
    asyncio.run(reader.poll_once())

    store2 = StateStore(str(tmp_path / "state.json"))
    assert store2.last_read_message_id == 42
    reader2 = ChatroomReader("https://x.invalid", 1, FakeAuth(), store=store2, session=FakeSession([[]]))  # type: ignore[arg-type]
    assert reader2.cursor == 42


def test_own_message_detection():
    reader = ChatroomReader("https://x.invalid", 1, FakeAuth(user_id=999), session=FakeSession([[]]))  # type: ignore[arg-type]
    assert reader.is_own_message({"user_id": 999}) is True
    assert reader.is_own_message({"user_id": 1000}) is False
