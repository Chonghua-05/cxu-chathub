"""`/status`、`/server` 的 HTML → PNG 渲染（Playwright/Chromium）。

渲染放在容器里（compose 里给足内存），产物以 base64 交给 NapCat 发图，
因此不依赖容器与宿主机共享文件系统。
"""

from __future__ import annotations

import base64
import io
import logging
from pathlib import Path
from typing import Any

log = logging.getLogger(__name__)

TEMPLATE_PATH = Path(__file__).with_name("templates") / "status.html"
BACKGROUND_PATH = Path(__file__).with_name("templates") / "status-bg.jpg"

ADDRESSES = [
    ("主IP", "game.example.com"),
    ("备用地址", "backup.example.com:25565"),
]


def _load_background() -> str | None:
    try:
        from PIL import Image
    except ImportError:  # pragma: no cover
        return None
    try:
        with Image.open(BACKGROUND_PATH) as image:
            if image.mode in ("RGBA", "LA", "P"):
                image = image.convert("RGB")
            output = io.BytesIO()
            image.save(output, format="JPEG", quality=60, optimize=True)
            return base64.b64encode(output.getvalue()).decode("ascii")
    except (OSError, ValueError):
        return None


def build_status_html(
    data: dict[str, Any],
    background_base64: str | None = None,
    addresses: list[tuple[str, str]] | None = None,
) -> str:
    """填充状态页模板（不依赖浏览器，可单测）。"""
    addresses_html = "".join(
        f'<div class="address-row"><span class="address-label">{label}</span>'
        f'<span class="address-value">{value}</span></div>'
        for label, value in (addresses or ADDRESSES)
    )

    routes_html = ""
    for route in data.get("network_routes") or []:
        if not isinstance(route, dict):
            continue
        online = bool(route.get("online"))
        icon, text = ("✅", "在线") if online else ("❌", "离线")
        color = "#4caf50" if online else "#f44336"
        latency = f"{route.get('latency', 0):.2f}ms" if online else "N/A"
        loss = f"{route.get('packet_loss', 0):.1f}%" if online else "N/A"
        routes_html += f"""
            <div class="item">
                <div class="item-name">{route.get('route_name', 'Unknown')}</div>
                <div class="status" style="color: {color};">{icon} {text}</div>
                <div class="detail">延迟: {latency}</div>
                <div class="detail">丢包: {loss}</div>
            </div>"""

    servers_html = ""
    for server in data.get("servers") or []:
        if not isinstance(server, dict):
            continue
        online = bool(server.get("online"))
        icon, text = ("✅", "在线") if online else ("❌", "离线")
        color = "#4caf50" if online else "#f44336"
        players_html = ""
        if online:
            players = [str(p).lstrip("• ").strip() for p in (server.get("online_players") or [])]
            players_html = "<div class='players'>" + "".join(
                f"<div class='player'>{p}</div>" for p in players
            ) + "</div>"
            if not players:
                players_html = "<div class='players'><div class='player'>(无)</div></div>"
        servers_html += f"""
            <div class="item">
                <div class="item-name">{server.get('server_name', 'Unknown')}</div>
                <div class="status" style="color: {color};">{icon} {text}</div>
                {players_html}
            </div>"""

    background = (
        f"background: url('data:image/jpeg;base64,{background_base64}') no-repeat center center;"
        if background_base64
        else "background: linear-gradient(135deg, #1a1a2e 0%, #16213e 50%, #0f3460 100%);"
    )

    template = TEMPLATE_PATH.read_text(encoding="utf-8")
    return template.format(
        background=background,
        addresses=addresses_html,
        routes=routes_html,
        servers=servers_html,
    )


class StatusRenderer:
    """Playwright 渲染器；浏览器不可用时抛异常，由上层回退到文本。"""

    def __init__(
        self,
        *,
        width: int = 1500,
        height: int = 1400,
        addresses: list[tuple[str, str]] | None = None,
    ) -> None:
        self._width = width
        self._height = height
        self._addresses = [tuple(pair) for pair in addresses] if addresses else list(ADDRESSES)
        self._background: str | None = None

    async def render(self, data: dict[str, Any]) -> bytes:
        from playwright.async_api import async_playwright

        if self._background is None:
            self._background = _load_background()
        html = build_status_html(data, self._background, self._addresses)

        async with async_playwright() as playwright:
            browser = await playwright.chromium.launch(args=["--no-sandbox", "--disable-dev-shm-usage"])
            try:
                page = await browser.new_page(viewport={"width": self._width, "height": self._height})
                await page.set_content(html)
                box = await page.locator("body").bounding_box()
                if box:
                    await page.set_viewport_size(
                        {"width": int(box["width"]), "height": int(box["height"])}
                    )
                    return await page.screenshot(type="png")
                return await page.screenshot(type="png", full_page=True)
            finally:
                await browser.close()


async def render_status_png(
    data: dict[str, Any], addresses: list[tuple[str, str]] | None = None
) -> bytes | None:
    """渲染失败返回 None（上层回退文本）。"""
    try:
        return await StatusRenderer(addresses=addresses).render(data)
    except Exception as exc:  # noqa: BLE001
        log.warning("状态图渲染失败，回退文本: %s", exc)
        return None
