"""onebot 模块测试：事件解析与动作调用（不联网）。"""

from __future__ import annotations

from chatroom_bridge.onebot import parse_group_message, parse_segments

RAW_GROUP_MESSAGE = {
    "post_type": "message",
    "message_type": "group",
    "self_id": 10000,
    "group_id": 123456789,
    "user_id": 10001,
    "message_id": 1234567890,
    "sender": {"user_id": 10001, "nickname": "玩家A", "card": ""},
    "message": [
        {"type": "reply", "data": {"id": "987654321"}},
        {"type": "text", "data": {"text": "看看这个 "}},
        {"type": "image", "data": {"file": "abc.png", "url": "https://multimedia.nt.qq.com.cn/x"}},
        {"type": "at", "data": {"qq": "10000"}},
    ],
}


def test_parse_group_message_fields():
    msg = parse_group_message(RAW_GROUP_MESSAGE)
    assert msg is not None
    assert msg.group_id == 123456789
    assert msg.user_id == 10001
    assert msg.message_id == 1234567890
    assert msg.display_name == "玩家A"
    assert msg.text == "看看这个"
    assert msg.reply_message_id == "987654321"
    assert [s.file for s in msg.images] == ["abc.png"]
    assert msg.at_user_ids == [10000]


def test_non_group_events_are_ignored():
    assert parse_group_message({"post_type": "message", "message_type": "private"}) is None
    assert parse_group_message({"post_type": "notice", "notice_type": "group_recall"}) is None
    assert parse_group_message({"post_type": "meta_event", "meta_event_type": "lifecycle"}) is None


def test_card_takes_precedence_over_nickname():
    raw = dict(RAW_GROUP_MESSAGE, sender={"nickname": "玩家A", "card": "[1.21]PlayerCard"})
    msg = parse_group_message(raw)
    assert msg is not None
    assert msg.display_name == "[1.21]PlayerCard"


def test_parse_segments_tolerates_shapes():
    # 字符串形式（CQ 码兜底）：整段当文本
    plain = parse_segments("纯文本")
    assert len(plain) == 1 and plain[0].type == "text" and plain[0].text == "纯文本"
    assert parse_segments("") == []
    assert parse_segments(None) == []
    assert parse_segments([{"type": "text", "data": {"text": "hi"}}])[0].text == "hi"
    assert parse_segments([{"no_type": 1}, "junk", {"type": "face", "data": None}])[0].type == "face"


def test_blank_and_at_only_messages():
    raw = dict(RAW_GROUP_MESSAGE, message=[{"type": "at", "data": {"qq": "10001"}}])
    msg = parse_group_message(raw)
    assert msg is not None
    assert msg.text == ""
    assert msg.images == []
