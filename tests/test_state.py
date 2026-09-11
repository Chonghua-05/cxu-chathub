"""state 模块测试：去重表、读游标、refresh_token、损坏文件恢复。"""

from __future__ import annotations

import json

from chatroom_bridge.state import StateStore


def test_missing_file_starts_empty(tmp_path):
    store = StateStore(str(tmp_path / "state.json"))
    assert store.forwarded_id("123") is None
    assert store.last_read_message_id == 0
    assert store.refresh_token == ""
    assert store.snapshot().forwarded_count == 0


def test_mark_and_lookup(tmp_path):
    store = StateStore(str(tmp_path / "state.json"))
    store.mark_forwarded("9001", 55)
    assert store.forwarded_id("9001") == 55
    assert store.already_forwarded("9001") is True
    assert store.already_forwarded("9002") is False

    # 重新载入后仍然存在（原子写 + 持久化）
    reloaded = StateStore(str(tmp_path / "state.json"))
    assert reloaded.forwarded_id("9001") == 55


def test_refresh_token_roundtrip(tmp_path):
    store = StateStore(str(tmp_path / "state.json"))
    store.set_refresh_token("rt-abc")
    assert StateStore(str(tmp_path / "state.json")).refresh_token == "rt-abc"


def test_cursor_roundtrip(tmp_path):
    store = StateStore(str(tmp_path / "state.json"))
    store.set_last_read_message_id(4242)
    assert StateStore(str(tmp_path / "state.json")).last_read_message_id == 4242


def test_forwarded_table_is_bounded(tmp_path):
    store = StateStore(str(tmp_path / "state.json"), max_forwarded=3)
    for i in range(5):
        store.mark_forwarded(f"m{i}", i)
    assert store.forwarded_id("m0") is None
    assert store.forwarded_id("m4") == 4
    assert store.snapshot().forwarded_count == 3


def test_corrupt_file_recovers(tmp_path):
    path = tmp_path / "state.json"
    path.write_text("{ not json", encoding="utf-8")
    store = StateStore(str(path))
    store.mark_forwarded("1", 2)
    assert json.loads(path.read_text(encoding="utf-8"))["forwarded"]["1"] == 2
