"""QQ 群 -> chatroom 转发流水线。

把 OneBot 群消息转成官方 Forward Bot API 的请求：
- 文本直接转发；图片/视频先下载（必要时压缩到 2MB 以内）再上传拿 attachment id
- 引用消息带上 reply 信息（优先用本地近期消息缓存填充被引用内容）
- 以 QQ message_id 作为 source_message_id，并做本地去重
"""

from __future__ import annotations

import io
import logging
from collections import OrderedDict
from dataclasses import dataclass, field

import aiohttp

from .forward_api import ForwardApi, ForwardApiError
from .onebot import GroupMessage, OneBotConnection
from .state import StateStore

log = logging.getLogger(__name__)

MAX_UPLOAD_SIZE = 10 * 1024 * 1024
IMAGE_COMPRESS_THRESHOLD = 2 * 1024 * 1024
IMAGE_MAX_DIMENSION = 1920
IMAGE_QUALITY = 75
RECENT_CACHE_SIZE = 200


@dataclass
class ForwardPlan:
    content: str
    image_refs: list[str] = field(default_factory=list)
    reply_to: str | None = None

    @property
    def empty(self) -> bool:
        return not self.content.strip() and not self.image_refs


def build_forward_plan(msg: GroupMessage) -> ForwardPlan:
    """从群消息构造转发计划（不做网络请求）。"""
    return ForwardPlan(
        content=msg.text,
        image_refs=[seg.url or seg.file for seg in msg.images if (seg.url or seg.file)],
        reply_to=msg.reply_message_id,
    )


class ChatroomForwarder:
    """把 QQ 群消息转发到 chatroom FORWARD 频道。"""

    def __init__(
        self,
        api: ForwardApi,
        state: StateStore,
        *,
        enabled: bool = True,
        self_id: int = 0,
        session: aiohttp.ClientSession | None = None,
    ) -> None:
        self._api = api
        self._state = state
        self._enabled = enabled
        self._self_id = int(self_id)
        self._session = session
        self._owns_session = session is None
        self._recent: OrderedDict[int, tuple[str, str]] = OrderedDict()
        self.stats = {"forwarded": 0, "skipped_duplicate": 0, "failed": 0}

    async def start(self) -> None:
        if self._session is None:
            self._session = aiohttp.ClientSession(
                timeout=aiohttp.ClientTimeout(total=30, sock_connect=5)
            )

    async def close(self) -> None:
        if self._owns_session and self._session is not None:
            await self._session.close()
            self._session = None

    # --- 入口 ---
    async def handle(self, conn: OneBotConnection, msg: GroupMessage) -> bool:
        """返回 True 表示这条消息已被转发。"""
        if not self._enabled:
            return False
        if self._self_id and msg.user_id == self._self_id:
            return False  # 自己发的消息不回灌

        plan = build_forward_plan(msg)
        self._remember(msg)
        if plan.empty:
            return False

        key = str(msg.message_id)
        if self._state.already_forwarded(key):
            self.stats["skipped_duplicate"] += 1
            log.debug("跳过已转发消息 message_id=%s", key)
            return False

        attachment_ids: list[int] = []
        for index, ref in enumerate(plan.image_refs):
            try:
                attachment_ids.append(await self._upload_image(conn, msg, ref, index))
            except (ForwardApiError, aiohttp.ClientError, OSError, ValueError) as exc:
                log.warning("图片上传失败（message_id=%s）: %s", key, exc)

        reply_nickname, reply_content = ("", "")
        if plan.reply_to:
            cached = self._recent.get(int(plan.reply_to)) if plan.reply_to.isdigit() else None
            if cached:
                reply_nickname, reply_content = cached

        try:
            response = await self._api.post_message(
                source="qq",
                content=plan.content,
                source_message_id=key,
                sender_qq=msg.user_id,
                nickname=msg.display_name,
                reply_source_message_id=plan.reply_to or "",
                reply_nickname=reply_nickname,
                reply_content=reply_content,
                attachment_ids=attachment_ids,
            )
        except ForwardApiError as exc:
            self.stats["failed"] += 1
            log.warning("转发到 chatroom 失败（message_id=%s）: %s", key, exc)
            return False

        chatroom_id = response.get("id")
        if isinstance(chatroom_id, int):
            self._state.mark_forwarded(key, chatroom_id)
        self.stats["forwarded"] += 1
        log.info(
            "已转发 QQ 消息到 chatroom: %s: %.40s (%d 个附件)",
            msg.display_name,
            plan.content or "[媒体]",
            len(attachment_ids),
        )
        return True

    # --- 内部工具 ---
    def _remember(self, msg: GroupMessage) -> None:
        summary = msg.text or ("[图片]" if msg.images else "")
        self._recent[int(msg.message_id)] = (msg.display_name, summary[:200])
        while len(self._recent) > RECENT_CACHE_SIZE:
            self._recent.popitem(last=False)

    async def _upload_image(
        self, conn: OneBotConnection, msg: GroupMessage, ref: str, index: int
    ) -> int:
        data, filename, content_type = await self._fetch_image(conn, ref, msg, index)
        return await self._api.upload(data, filename, content_type)

    async def _fetch_image(
        self, conn: OneBotConnection, ref: str, msg: GroupMessage, index: int
    ) -> tuple[bytes, str, str]:
        """取图片字节。ref 直接是 http(s) 时就用它，否则经 OneBot get_image 拿地址。"""
        url = ref if ref.startswith("http") else ""
        if not url:
            info = await conn.get_image(ref)
            if isinstance(info, dict):
                url = str(info.get("url") or "")
        if not url.startswith("http"):
            raise ValueError(f"无法解析图片地址: {ref[:80]}")

        session = self._session
        if session is None:
            await self.start()
            session = self._session
        assert session is not None

        async with session.get(url) as resp:
            if resp.status != 200:
                raise ValueError(f"下载图片失败 HTTP {resp.status}")
            data = await resp.read()
        if len(data) > MAX_UPLOAD_SIZE:
            raise ValueError(f"图片超过 10MB（{len(data)} 字节）")

        filename, content_type = f"qq-{msg.message_id}-{index}.png", "image/png"
        if len(data) > IMAGE_COMPRESS_THRESHOLD:
            compressed = _compress_image(data)
            if compressed is not None:
                data, filename, content_type = compressed
        return data, filename, content_type


def _compress_image(data: bytes) -> tuple[bytes, str, str] | None:
    """按最长边 1920 + JPEG q75 压缩，失败时返回 None（原图上传）。"""
    try:
        from PIL import Image
    except ImportError:  # pragma: no cover - 容器里一定装了 Pillow
        return None
    try:
        with Image.open(io.BytesIO(data)) as image:
            if image.mode in ("RGBA", "LA", "P"):
                image = image.convert("RGB")
            image.thumbnail((IMAGE_MAX_DIMENSION, IMAGE_MAX_DIMENSION))
            output = io.BytesIO()
            image.save(output, format="JPEG", quality=IMAGE_QUALITY, optimize=True)
            return output.getvalue(), "qq-compressed.jpg", "image/jpeg"
    except (OSError, ValueError):
        return None
