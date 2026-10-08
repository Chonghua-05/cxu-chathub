//! `/status`、`/server` 的状态图渲染：**纯 Rust，不再依赖 Chromium**。
//!
//! 管线：cosmic-text 做整形 / 换行 / 定位（与系统字体同源），每个字形经
//! `SwashCache::get_outline_commands` 取轮廓 → 转成 SVG `<path>`；面板、卡片、
//! 图标用 SVG 基本元素；背景图先解码 → 面板区域做局部高斯模糊 → resvg 光栅化
//! （`resvg::render`）。产物仍是 PNG 字节，交给上层 base64 发图。
//!
//! 视觉常量照抄旧 `status.html`（见 git 历史 bff03e2）的 CSS，目标是与
//! Chromium 渲染“肉眼接近”。

use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use cosmic_text::fontdb;
use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, Stretch, SwashCache, Weight, Wrap,
};
use serde_json::Value;

use crate::services::commands::DEFAULT_SERVER_ADDRESSES;
use crate::services::{clean_player_name, py_get_str, truthy};

/// 状态图背景（原图，不再重编码）
const BACKGROUND_JPG: &[u8] = include_bytes!("templates/status-bg.jpg");
/// 预裁好的「横向成品」背景条：宽=nominal 画布宽、高=nominal cover 高。
/// 宽高固定时直接裁剪即可，免去运行时的 Lanczos 缩放（见 [`strip_canvas`]）。
const BACKGROUND_STRIP_PNG: &[u8] = include_bytes!("templates/status-bg-strip.png");
/// 状态图标（预渲染 PNG，绿色对勾 / 红色叉）
const ICON_OK_PNG: &[u8] = include_bytes!("../../assets/icons/ok.png");
const ICON_FAIL_PNG: &[u8] = include_bytes!("../../assets/icons/fail.png");

// ---------------- 视觉常量（照抄旧 status.html，见 git 历史 bff03e2） ----------------

const FONT_FAMILY: &str = "Noto Sans CJK SC";
/// 行高倍数：CSS 未设 line-height，走字体 normal；实测对齐 Chromium（Noto Sans CJK ≈1.47）
const LINE_H: f32 = 1.47;

const BODY_PAD: f32 = 40.0;
const PANEL_PAD: f32 = 40.0;
const PANEL_RADIUS: f32 = 24.0;
const PANEL_MIN_W: f32 = 600.0; // box-sizing: border-box
const CARD_RADIUS: f32 = 16.0;

const SECTION_MB: f32 = 40.0;
const TITLE_FS: f32 = 36.0;
const TITLE_LS: f32 = 1.0;
const TITLE_MB: f32 = 25.0;
const TITLE_BAR_W: f32 = 3.0;
const TITLE_BAR_GAP: f32 = 15.0; // border-left 与文字间距（padding-left）

const ADDR_PAD_V: f32 = 22.0;
const ADDR_PAD_H: f32 = 25.0;
const ADDR_MB: f32 = 25.0;
const ADDR_ROW_FS: f32 = 25.0;
const ADDR_ROW_PAD_V: f32 = 8.0;
const ADDR_LABEL_MIN: f32 = 245.0;
const ADDR_GAP: f32 = 24.0;
const ADDR_VALUE_LS: f32 = 0.3;

const GRID_GAP: f32 = 20.0;
const ITEM_PAD: f32 = 25.0;
const ITEM_MB: f32 = 20.0;
const ITEM_NAME_FS: f32 = 30.0;
const ITEM_NAME_LS: f32 = 0.5;
const ITEM_NAME_MB: f32 = 12.0;

const STATUS_FS: f32 = 24.0;
const STATUS_PAD_V: f32 = 6.0;
const STATUS_PAD_H: f32 = 14.0;
const STATUS_RADIUS: f32 = 20.0;
const STATUS_MB: f32 = 10.0;
const ICON_SZ: f32 = 24.0;
const ICON_GAP: f32 = 6.0;

const DETAIL_FS: f32 = 26.0;
const PLAYERS_MT: f32 = 15.0;
const PLAYERS_PL: f32 = 12.0; // border 2 + padding-left 10
const PLAYER_FS: f32 = 24.0;
const PLAYER_GAP: f32 = 6.0;

const DIVIDER_H: f32 = 1.0;
const DIVIDER_M: f32 = 40.0;

// 颜色
const TEAL: (u8, u8, u8) = (0x4d, 0xb6, 0xac);
const GREEN: (u8, u8, u8) = (0x4c, 0xaf, 0x50);
const RED: (u8, u8, u8) = (0xf4, 0x43, 0x36);

const WHITE: (u8, u8, u8) = (0xff, 0xff, 0xff);
const BLACK: (u8, u8, u8) = (0x00, 0x00, 0x00);

// ---------------------------- 错误类型 ----------------------------

/// 渲染错误；`Display` 消息进入上层「回退文本」的警告日志。
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// 找不到所需中文字体（字体未安装）
    #[error("未找到字体 {0}（Docker 需装 fonts-noto-cjk）")]
    FontNotFound(String),
    /// 背景图解码失败
    #[error("背景图解码失败: {0}")]
    Background(String),
    /// SVG 解析失败
    #[error("SVG 解析失败: {0}")]
    Svg(String),
    /// 画布 / PNG 编码失败
    #[error("光栅化失败: {0}")]
    Raster(String),
}

// ---------------------------- 全局共享状态 ----------------------------

fn font_system() -> &'static Mutex<FontSystem> {
    static FS: OnceLock<Mutex<FontSystem>> = OnceLock::new();
    FS.get_or_init(|| Mutex::new(FontSystem::new()))
}

fn swash_cache() -> &'static Mutex<SwashCache> {
    static SC: OnceLock<Mutex<SwashCache>> = OnceLock::new();
    SC.get_or_init(|| Mutex::new(SwashCache::new()))
}

/// 渲染并发上限：渲染是纯 CPU 活且全程持有全局字体锁，串行足够；
/// 多个并发调用方在此排队，避免同时占多个 blocking 线程并互相争抢字体锁。
static RENDER_PERMIT: OnceLock<tokio::sync::Semaphore> = OnceLock::new();

fn render_permit() -> &'static tokio::sync::Semaphore {
    RENDER_PERMIT.get_or_init(|| tokio::sync::Semaphore::new(1))
}

/// 背景图解码（原图；仅在做 cover 缩放时解码，不长期常驻）。
fn decode_background() -> Option<image::DynamicImage> {
    image::load_from_memory(BACKGROUND_JPG).ok()
}

/// 预裁背景条的快速路径：直接按高度居中裁剪，**不做缩放**。
/// 仅当条宽/高都够用（sw>=iw 且 sh>=ih）时命中；否则返回 None 走通用 cover 路径。
/// 与旧 cover 的居中裁剪一致（宽度方向条宽=画布宽时 ox=0）。
fn strip_canvas(iw: u32, ih: u32) -> Option<image::RgbaImage> {
    let img = image::load_from_memory(BACKGROUND_STRIP_PNG).ok()?;
    let (sw, sh) = (img.width(), img.height());
    if sw < iw || sh < ih {
        return None;
    }
    let rgb = match img {
        image::DynamicImage::ImageRgb8(buf) => buf,
        other => other.to_rgb8(),
    };
    let ox = (sw - iw) / 2;
    let oy = (sh - ih) / 2; // 与旧 cover 的纵向居中裁剪一致
    if ox == 0 && oy == 0 && rgb.width() == iw && rgb.height() == ih {
        return Some(image::DynamicImage::ImageRgb8(rgb).to_rgba8());
    }
    let crop = image::imageops::crop_imm(&rgb, ox, oy, iw, ih).to_image();
    Some(image::DynamicImage::ImageRgb8(crop).to_rgba8())
}

/// 简单双线性缩放（u8→u8），**不产生** `image::imageops::resize` 那样的 f32 大中间缓冲
/// （其缓冲 ≈ 目标宽 × 源高 × 通道 × 4，高图时可达数十 MB）。背景回退路径用；
/// 该背景在面板内本就会被模糊，边缘的轻微缩放差异可接受。
fn resize_bilinear(src: &image::RgbImage, dw: u32, dh: u32) -> image::RgbImage {
    let (sw, sh) = (src.width().max(1), src.height().max(1));
    let (dw, dh) = (dw.max(1), dh.max(1));
    let mut out = image::RgbImage::new(dw, dh);
    let xr = sw as f32 / dw as f32;
    let yr = sh as f32 / dh as f32;
    for y in 0..dh {
        let sy = ((y as f32 + 0.5) * yr - 0.5).max(0.0);
        let y0 = (sy.floor() as u32).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let fy = sy - y0 as f32;
        for x in 0..dw {
            let sx = ((x as f32 + 0.5) * xr - 0.5).max(0.0);
            let x0 = (sx.floor() as u32).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            let fx = sx - x0 as f32;
            let p00 = src.get_pixel(x0, y0);
            let p10 = src.get_pixel(x1, y0);
            let p01 = src.get_pixel(x0, y1);
            let p11 = src.get_pixel(x1, y1);
            let mut px = [0u8; 3];
            for c in 0..3 {
                let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
                let bot = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
                px[c] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
            }
            out.put_pixel(x, y, image::Rgb(px));
        }
    }
    out
}

/// 把背景按 cover 适配缩放到 (iw, ih)：解码 → 裁剪可见区域 → 缩放 → 转 RGBA。
/// 全程在 RGB 空间缩放/裁剪，最后一步才转 RGBA，避免“全尺寸 RGBA”中间态。
fn cover_canvas(iw: u32, ih: u32) -> Option<image::RgbaImage> {
    // 快速路径：预裁背景条够用时直接裁剪（免 Lanczos 缩放，省 ~4×目标的 f32 缓冲）
    if let Some(c) = strip_canvas(iw, ih) {
        return Some(c);
    }
    let bg = decode_background()?;
    let (bw, bh) = (bg.width(), bg.height());
    let scale = (iw as f32 / bw as f32).max(ih as f32 / bh as f32);
    let sw = (bw as f32 * scale).ceil() as u32;
    let sh = (bh as f32 * scale).ceil() as u32;
    let ox = (sw.saturating_sub(iw)) / 2;
    let oy = (sh.saturating_sub(ih)) / 2;
    let rgb = match bg {
        image::DynamicImage::ImageRgb8(buf) => buf,
        other => other.to_rgb8(),
    };
    // 先把源里「可见区域」裁出来，再缩放到目标尺寸：避免先生成 sw×sh 的大中间图
    // （高图时 sw 也变得很大，image 的 resize 还要按其开 ~4× 的 f32 缓冲，实测可上百 MB）。
    let sx = ((ox as f32 / scale).floor() as u32).min(bw.saturating_sub(1));
    let sy = ((oy as f32 / scale).floor() as u32).min(bh.saturating_sub(1));
    let ex = (((ox + iw) as f32 / scale).ceil() as u32).clamp(sx + 1, bw);
    let ey = (((oy + ih) as f32 / scale).ceil() as u32).clamp(sy + 1, bh);
    let region = image::imageops::crop_imm(&rgb, sx, sy, ex - sx, ey - sy).to_image();
    let scaled = resize_bilinear(&region, iw.max(1), ih.max(1));
    Some(image::DynamicImage::ImageRgb8(scaled).to_rgba8())
}

/// 全分辨率下的高斯 sigma（与原 `blur_region` 的 `sigma=20` 保持一致）。
const BG_BLUR_SIGMA: f32 = 20.0;

/// 整幅高斯模糊：降采样 1/4 → σ/4 高斯 → 放大回原尺寸（与旧逻辑同参，效果一致）。
fn blur_canvas(sharp: &image::RgbaImage) -> image::RgbaImage {
    const DOWN: u32 = 4;
    let (iw, ih) = (sharp.width(), sharp.height());
    let dw = (iw / DOWN).max(1);
    let dh = (ih / DOWN).max(1);
    let small = image::imageops::resize(sharp, dw, dh, image::imageops::FilterType::Triangle);
    let bl = imageproc::filter::gaussian_blur_f32(&small, BG_BLUR_SIGMA / DOWN as f32);
    image::imageops::resize(&bl, iw, ih, image::imageops::FilterType::Triangle)
}

// ---------------------------- 文本整形 ----------------------------

/// CSS font-weight → 容器内实际可用字重（只有 Regular 400 / Bold 700）。
fn css_weight(weight: f32) -> Weight {
    if weight >= 600.0 {
        Weight::BOLD
    } else {
        Weight::NORMAL
    }
}

fn font_available(fs: &mut FontSystem) -> bool {
    let query = fontdb::Query {
        families: &[Family::Name(FONT_FAMILY)],
        weight: Weight::NORMAL,
        stretch: Stretch::Normal,
        style: Style::Normal,
    };
    fs.db().query(&query).is_some()
}

fn shape_width(
    fs: &mut FontSystem,
    text: &str,
    size: f32,
    weight: Weight,
    width: Option<f32>,
) -> Buffer {
    let metrics = Metrics::new(size, size * LINE_H);
    let mut buffer = Buffer::new(fs, metrics);
    buffer.set_wrap(if width.is_some() {
        Wrap::WordOrGlyph
    } else {
        Wrap::None
    });
    buffer.set_size(width, None);
    let attrs = Attrs::new()
        .family(Family::Name(FONT_FAMILY))
        .weight(weight);
    buffer.set_text(text, &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(fs, false);
    buffer
}

/// 单行文本宽度 = 各行最大宽度 + letter-spacing*n（CSS letter-spacing 近似）。
fn measure(fs: &mut FontSystem, text: &str, size: f32, weight: f32, ls: f32) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let buffer = shape_width(fs, text, size, css_weight(weight), None);
    let mut w: f32 = 0.0;
    for run in buffer.layout_runs() {
        w = w.max(run.line_w);
    }
    w + ls * text.chars().count() as f32
}

// ---------------------------- SVG 生成基元 ----------------------------

/// 圆角矩形（fill 可带透明度；stroke 可选）。
#[allow(clippy::too_many_arguments)]
fn push_rect(
    svg: &mut String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    fill: (u8, u8, u8),
    fill_opacity: f32,
    stroke: Option<((u8, u8, u8), f32, f32)>,
) {
    let (fr, fg, fb) = fill;
    svg.push_str(&format!(
        "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"{r:.1}\" ry=\"{r:.1}\" fill=\"#{fr:02X}{fg:02X}{fb:02X}\" fill-opacity=\"{fill_opacity:.3}\""
    ));
    if let Some(((sr, sg, sb), so, sw)) = stroke {
        svg.push_str(&format!(
            " stroke=\"#{sr:02X}{sg:02X}{sb:02X}\" stroke-opacity=\"{so:.3}\" stroke-width=\"{sw:.1}\""
        ));
    }
    svg.push_str("/>");
}

/// 内嵌 PNG 图标。
fn push_icon(svg: &mut String, png: &[u8], x: f32, y: f32, w: f32, h: f32) {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    svg.push_str(&format!(
        "<image x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" href=\"data:image/png;base64,{b64}\"/>"
    ));
}

/// 把 zeno 路径命令转成 SVG path `d`（字体坐标 y 向上 → SVG y 向下）。
fn commands_to_path(cmds: &[cosmic_text::Command], ox: f32, baseline: f32) -> String {
    use cosmic_text::Command::*;
    let mut d = String::new();
    for cmd in cmds {
        match cmd {
            MoveTo(p) => d.push_str(&format!("M{:.1} {:.1}", ox + p.x, baseline - p.y)),
            LineTo(p) => d.push_str(&format!("L{:.1} {:.1}", ox + p.x, baseline - p.y)),
            QuadTo(c, p) => d.push_str(&format!(
                "Q{:.1} {:.1} {:.1} {:.1}",
                ox + c.x,
                baseline - c.y,
                ox + p.x,
                baseline - p.y
            )),
            CurveTo(c1, c2, p) => d.push_str(&format!(
                "C{:.1} {:.1} {:.1} {:.1} {:.1} {:.1}",
                ox + c1.x,
                baseline - c1.y,
                ox + c2.x,
                baseline - c2.y,
                ox + p.x,
                baseline - p.y
            )),
            Close => d.push('Z'),
        }
    }
    d
}

/// 追加一段文本（字形转 `<path>`），返回其推进宽度。
/// `top` 为行盒顶部 y（基线由 cosmic-text 度量决定）。
#[allow(clippy::too_many_arguments)]
fn push_text(
    svg: &mut String,
    fs: &mut FontSystem,
    cache: &mut SwashCache,
    text: &str,
    size: f32,
    weight: f32,
    ls: f32,
    x0: f32,
    top: f32,
    color: (u8, u8, u8),
    opacity: f32,
) -> f32 {
    push_flow(svg, fs, cache, text, size, weight, ls, x0, top, color, opacity, None)
}

/// 与 [`push_text`] 相同，但支持按宽度换行（多行），返回块高度（行数 × 行高）。
#[allow(clippy::too_many_arguments)]
fn push_flow(
    svg: &mut String,
    fs: &mut FontSystem,
    cache: &mut SwashCache,
    text: &str,
    size: f32,
    weight: f32,
    ls: f32,
    x0: f32,
    top: f32,
    color: (u8, u8, u8),
    opacity: f32,
    width: Option<f32>,
) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let w = css_weight(weight);
    let buffer = shape_width(fs, text, size, w, width);
    let (r, g, b) = color;
    let fill = if opacity >= 1.0 {
        format!("#{r:02X}{g:02X}{b:02X}")
    } else {
        format!("#{r:02X}{g:02X}{b:02X}\" fill-opacity=\"{opacity:.3}")
    };
    let line_h = size * LINE_H;
    let mut lines = 0u32;
    for run in buffer.layout_runs() {
        let baseline = top + run.line_y;
        let mut pen = x0;
        for glyph in run.glyphs.iter() {
            let key = glyph.physical((0.0, 0.0), 1.0).cache_key;
            if let Some(cmds) = cache.get_outline_commands(fs, key) {
                let d = commands_to_path(cmds, pen, baseline);
                if !d.is_empty() {
                    svg.push_str(&format!("<path d=\"{d}\" fill=\"{fill}\"/>"));
                }
            }
            pen += glyph.w + ls;
        }
        lines += 1;
    }
    lines as f32 * line_h
}

// ---------------------------- 布局 + SVG 组装 ----------------------------

struct Card {
    name: String,
    online: bool,
    details: Vec<String>,
    players: Option<Vec<String>>, // None = 不显示玩家区；Some(空) = 显示 (无)
}

fn route_cards(data: &Value) -> Vec<Card> {
    let mut out = Vec::new();
    if let Some(Value::Array(routes)) = data.get("network_routes") {
        for route in routes {
            if route.as_object().is_none() {
                continue;
            }
            let online = truthy(route.get("online"));
            let latency = if online {
                format!(
                    "延迟: {:.2}ms",
                    route.get("latency").and_then(Value::as_f64).unwrap_or(0.0)
                )
            } else {
                "延迟: N/A".to_string()
            };
            let loss = if online {
                format!(
                    "丢包: {:.1}%",
                    route.get("packet_loss").and_then(Value::as_f64).unwrap_or(0.0)
                )
            } else {
                "丢包: N/A".to_string()
            };
            out.push(Card {
                name: py_get_str(route, "route_name", "Unknown"),
                online,
                details: vec![latency, loss],
                players: None,
            });
        }
    }
    out
}

fn server_cards(data: &Value) -> Vec<Card> {
    let mut out = Vec::new();
    if let Some(Value::Array(servers)) = data.get("servers") {
        for server in servers {
            if server.as_object().is_none() {
                continue;
            }
            let online = truthy(server.get("online"));
            let players = if online {
                let list: Vec<String> = match server.get("online_players") {
                    Some(Value::Array(items)) => items.iter().map(clean_player_name).collect(),
                    _ => Vec::new(),
                };
                Some(list)
            } else {
                None
            };
            out.push(Card {
                name: py_get_str(server, "server_name", "Unknown"),
                online,
                details: Vec::new(),
                players,
            });
        }
    }
    out
}

/// 状态胶囊宽度（图标 + 间隔 + 文字 + 左右内边距）。
fn status_width(fs: &mut FontSystem, online: bool) -> f32 {
    let text = if online { "在线" } else { "离线" };
    STATUS_PAD_H * 2.0 + ICON_SZ + ICON_GAP + measure(fs, text, STATUS_FS, 500.0, 0.0)
}

/// 单张卡片的内容最大宽度（max-content）。
fn card_content_width(fs: &mut FontSystem, card: &Card) -> f32 {
    let mut w = measure(fs, &card.name, ITEM_NAME_FS, 600.0, ITEM_NAME_LS);
    w = w.max(status_width(fs, card.online));
    for d in &card.details {
        w = w.max(measure(fs, d, DETAIL_FS, 400.0, 0.0));
    }
    if let Some(players) = &card.players {
        let list = if players.is_empty() {
            vec!["(无)".to_string()]
        } else {
            players.clone()
        };
        for p in &list {
            w = w.max(PLAYERS_PL + measure(fs, p, PLAYER_FS, 400.0, 0.0));
        }
    }
    w
}

/// 卡片高度（内容垂直堆叠，逐块累加）。
fn card_height(_fs: &mut FontSystem, card: &Card) -> f32 {
    let mut h = ITEM_PAD * 2.0;
    h += ITEM_NAME_FS * LINE_H + ITEM_NAME_MB;
    h += STATUS_FS * LINE_H + STATUS_PAD_V * 2.0 + STATUS_MB;
    h += DETAIL_FS * LINE_H * card.details.len() as f32;
    if let Some(players) = &card.players {
        h += PLAYERS_MT;
        let list = if players.is_empty() {
            vec!["(无)".to_string()]
        } else {
            players.clone()
        };
        let n = list.len();
        h += n as f32 * PLAYER_FS * LINE_H + (n.saturating_sub(1) as f32) * PLAYER_GAP;
    }
    h
}

/// 网格（两列）的总宽度与外框高度。
fn grid_metrics(fs: &mut FontSystem, cards: &[Card]) -> (f32, f32) {
    if cards.is_empty() {
        return (0.0, 0.0);
    }
    let max_item_w = cards
        .iter()
        .map(|c| card_content_width(fs, c) + ITEM_PAD * 2.0)
        .fold(0.0f32, f32::max);
    let grid_w = max_item_w * 2.0 + GRID_GAP;
    let mut h = 0.0;
    let rows = cards.len().div_ceil(2);
    for r in 0..rows {
        let i = r * 2;
        let mut row_h: f32 = 0.0;
        for c in &cards[i..(i + 2).min(cards.len())] {
            row_h = row_h.max(card_height(fs, c) + ITEM_MB);
        }
        h += row_h;
    }
    h += GRID_GAP * (rows.saturating_sub(1)) as f32;
    (grid_w, h)
}

/// 渲染状态图为 PNG。数据非对象时返回 None（上层回退文本）。
///
/// 渲染是同步 CPU 活（字体整形 / SVG / 光栅化 / PNG 编码）且全程持有全局字体锁：
/// 丢进 `spawn_blocking` 执行不占异步 worker；信号量（1）保证最多一个渲染在途。
pub async fn render_status_png(
    data: &Value,
    addresses: Option<&[(String, String)]>,
) -> Option<Vec<u8>> {
    if !data.is_object() {
        return None;
    }
    // 状态 JSON 只有几 KB，clone 一次换取 'static 闭包
    let data = data.clone();
    let addresses = addresses.map(|list| list.to_vec());
    let _permit = render_permit().acquire().await.ok()?;
    match tokio::task::spawn_blocking(move || render(&data, addresses.as_deref())).await {
        Ok(Ok(png)) => Some(png),
        Ok(Err(err)) => {
            tracing::warn!("状态图渲染失败，回退文本: {err}");
            None
        }
        Err(err) => {
            tracing::warn!("状态图渲染任务异常结束，回退文本: {err}");
            None
        }
    }
}

/// 同步渲染入口（便于单测）。
pub fn render(data: &Value, addresses: Option<&[(String, String)]>) -> Result<Vec<u8>, RenderError> {
    let default_addresses: Vec<(String, String)> = DEFAULT_SERVER_ADDRESSES
        .iter()
        .map(|(l, v)| (l.to_string(), v.to_string()))
        .collect();
    let resolved: &[(String, String)] = match addresses {
        Some(list) if !list.is_empty() => list,
        _ => &default_addresses,
    };

    let mut fs = font_system().lock().unwrap();
    if !font_available(&mut fs) {
        return Err(RenderError::FontNotFound(FONT_FAMILY.to_string()));
    }
    let mut cache = swash_cache().lock().unwrap();

    let routes = route_cards(data);
    let servers = server_cards(data);

    // ---- 度量（max-content 决定宽度）----
    let title1 = "节点状态";
    let title2 = "服务器状态";
    let title_w = TITLE_BAR_W
        + TITLE_BAR_GAP
        + measure(&mut fs, title1, TITLE_FS, 500.0, TITLE_LS)
        .max(measure(&mut fs, title2, TITLE_FS, 500.0, TITLE_LS));

    // 地址面板
    let mut addr_rows: Vec<(String, String, f32, f32)> = Vec::new(); // (label, value, label_w, value_w)
    for (label, value) in resolved {
        let lw = measure(&mut fs, label, ADDR_ROW_FS, 600.0, 0.0);
        let vw = measure(&mut fs, value, ADDR_ROW_FS, 500.0, ADDR_VALUE_LS);
        addr_rows.push((label.clone(), value.clone(), lw, vw));
    }
    let addr_content_w = addr_rows
        .iter()
        .map(|(_, _, lw, vw)| lw.max(ADDR_LABEL_MIN) + ADDR_GAP + vw)
        .fold(0.0f32, f32::max);
    let addr_panel_w = addr_content_w + ADDR_PAD_H * 2.0;
    let addr_row_h = ADDR_ROW_FS * LINE_H + ADDR_ROW_PAD_V * 2.0;
    let addr_panel_h = addr_row_h * addr_rows.len() as f32 + ADDR_PAD_V * 2.0;

    let (routes_grid_w, routes_grid_h) = grid_metrics(&mut fs, &routes);
    let (servers_grid_w, servers_grid_h) = grid_metrics(&mut fs, &servers);

    let content_w = [title_w, addr_panel_w, routes_grid_w, servers_grid_w]
        .into_iter()
        .fold(0.0f32, f32::max);
    let panel_w = (content_w + PANEL_PAD * 2.0).max(PANEL_MIN_W);
    let body_w = panel_w + BODY_PAD * 2.0;

    // ---- 垂直布局 ----
    let title_block_h = TITLE_FS * LINE_H;
    let panel_top = BODY_PAD;
    let content_x = BODY_PAD + PANEL_PAD;

    // 第一节
    let sec1_top = BODY_PAD + PANEL_PAD;
    let addr_panel_top = sec1_top + title_block_h + TITLE_MB;
    let routes_grid_top = addr_panel_top + addr_panel_h + ADDR_MB;
    let sec1_bottom = routes_grid_top + routes_grid_h;

    // 分隔条
    let divider_top = sec1_bottom + DIVIDER_M;
    // 第二节
    let sec2_top = divider_top + DIVIDER_H + DIVIDER_M;
    let servers_grid_top = sec2_top + title_block_h + TITLE_MB;
    let sec2_bottom = servers_grid_top + servers_grid_h;

    let panel_h = (sec2_bottom + SECTION_MB + PANEL_PAD) - panel_top;
    let body_h = panel_h + BODY_PAD * 2.0;

    // ---- 组装 SVG（背景在 pixmap 侧，这里只画前景）----
    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{body_w:.0}\" height=\"{body_h:.0}\" viewBox=\"0 0 {body_w:.0} {body_h:.0}\">"
    ));

    // 外层面板（半透明白 + 边框 + 阴影）
    // 阴影用多层向外扩展的描边环近似原 feDropShadow(dy8/σ16/op0.37)：resvg 的 SVG
    // filter 会另开一张全尺寸位图（实测 ~14MB 峰值）。描边环只落在面板外侧——与原
    // filter 合成后一样，不压暗面板内部（实测内部差≈0）。
    {
        let layers: [(f32, f32, f32); 5] = [
            // (外扩, 描边宽, 透明度)
            (3.0, 8.0, 0.24),
            (12.0, 10.0, 0.13),
            (23.0, 12.0, 0.07),
            (35.0, 12.0, 0.03),
            (48.0, 12.0, 0.012),
        ];
        for (grow, stroke_w, alpha) in layers {
            push_rect(
                &mut svg,
                panel_top - grow,
                panel_top - grow + 8.0,
                panel_w + grow * 2.0,
                panel_h + grow * 2.0,
                PANEL_RADIUS + grow,
                BLACK,
                0.0,
                Some((BLACK, alpha, stroke_w)),
            );
        }
    }
    svg.push_str(&format!(
        "<rect x=\"{panel_top:.1}\" y=\"{panel_top:.1}\" width=\"{panel_w:.1}\" height=\"{panel_h:.1}\" rx=\"{PANEL_RADIUS:.1}\" ry=\"{PANEL_RADIUS:.1}\" fill=\"#FFFFFF\" fill-opacity=\"0.6\" stroke=\"#FFFFFF\" stroke-opacity=\"0.3\" stroke-width=\"1\"/>"
    ));

    // 标题 + 竖条
    let draw_title = |svg: &mut String, fs: &mut FontSystem, cache: &mut SwashCache, text: &str, top: f32| {
        let bar_h = TITLE_FS * LINE_H * 0.7;
        let bar_y = top + (TITLE_FS * LINE_H - bar_h) / 2.0;
        push_rect(svg, content_x, bar_y, TITLE_BAR_W, bar_h, TITLE_BAR_W / 2.0, TEAL, 1.0, None);
        push_text(
            svg,
            fs,
            cache,
            text,
            TITLE_FS,
            500.0,
            TITLE_LS,
            content_x + TITLE_BAR_W + TITLE_BAR_GAP,
            top,
            BLACK,
            0.9,
        );
    };
    draw_title(&mut svg, &mut fs, &mut cache, title1, sec1_top);
    draw_title(&mut svg, &mut fs, &mut cache, title2, sec2_top);

    // 地址面板
    push_rect(
        &mut svg,
        content_x,
        addr_panel_top,
        addr_panel_w,
        addr_panel_h,
        CARD_RADIUS,
        WHITE,
        0.15,
        Some((WHITE, 0.25, 1.0)),
    );
    for (i, (label, value, lw, vw)) in addr_rows.iter().enumerate() {
        let row_top = addr_panel_top + ADDR_PAD_V + i as f32 * addr_row_h;
        let text_top = row_top + ADDR_ROW_PAD_V;
        push_text(
            &mut svg,
            &mut fs,
            &mut cache,
            label,
            ADDR_ROW_FS,
            600.0,
            0.0,
            content_x + ADDR_PAD_H,
            text_top,
            BLACK,
            0.72,
        );
        let _ = lw;
        // 值右对齐到面板内容右缘
        let right = content_x + addr_panel_w - ADDR_PAD_H;
        push_text(
            &mut svg,
            &mut fs,
            &mut cache,
            value,
            ADDR_ROW_FS,
            500.0,
            ADDR_VALUE_LS,
            right - vw,
            text_top,
            BLACK,
            1.0,
        );
    }

    // ---- 卡片：路由网格 ----
    let draw_grid = |svg: &mut String,
                     fs: &mut FontSystem,
                     cache: &mut SwashCache,
                     cards: &[Card],
                     grid_top: f32,
                     col_w: f32| {
        let rows = cards.len().div_ceil(2);
        let mut ry = grid_top;
        for r in 0..rows {
            let i = r * 2;
            let slice = &cards[i..(i + 2).min(cards.len())];
            let row_h = slice
                .iter()
                .map(|c| card_height(fs, c) + ITEM_MB)
                .fold(0.0f32, f32::max);
            for (ci, card) in slice.iter().enumerate() {
                let x = content_x + ci as f32 * (col_w + GRID_GAP);
                let h = card_height(fs, card);
                push_rect(
                    svg,
                    x,
                    ry,
                    col_w,
                    h,
                    CARD_RADIUS,
                    WHITE,
                    0.15,
                    Some((WHITE, 0.25, 1.0)),
                );
                let mut cy = ry + ITEM_PAD;
                push_text(
                    svg,
                    fs,
                    cache,
                    &card.name,
                    ITEM_NAME_FS,
                    600.0,
                    ITEM_NAME_LS,
                    x + ITEM_PAD,
                    cy,
                    BLACK,
                    1.0,
                );
                cy += ITEM_NAME_FS * LINE_H + ITEM_NAME_MB;
                // 状态胶囊
                let sw = status_width(fs, card.online);
                let sh = STATUS_FS * LINE_H + STATUS_PAD_V * 2.0;
                push_rect(svg, x + ITEM_PAD, cy, sw, sh, STATUS_RADIUS, WHITE, 0.2, None);
                let color = if card.online { GREEN } else { RED };
                let icon = if card.online { ICON_OK_PNG } else { ICON_FAIL_PNG };
                let icon_y = cy + STATUS_PAD_V + (STATUS_FS * LINE_H - ICON_SZ) / 2.0;
                push_icon(svg, icon, x + ITEM_PAD + STATUS_PAD_H, icon_y, ICON_SZ, ICON_SZ);
                let status_text = if card.online { "在线" } else { "离线" };
                push_text(
                    svg,
                    fs,
                    cache,
                    status_text,
                    STATUS_FS,
                    500.0,
                    0.0,
                    x + ITEM_PAD + STATUS_PAD_H + ICON_SZ + ICON_GAP,
                    cy + STATUS_PAD_V,
                    color,
                    1.0,
                );
                cy += sh + STATUS_MB;
                // 详情
                for d in &card.details {
                    push_text(
                        svg,
                        fs,
                        cache,
                        d,
                        DETAIL_FS,
                        400.0,
                        0.0,
                        x + ITEM_PAD,
                        cy,
                        BLACK,
                        0.6,
                    );
                    cy += DETAIL_FS * LINE_H;
                }
                // 玩家
                if let Some(players) = &card.players {
                    cy += PLAYERS_MT;
                    let list: Vec<String> = if players.is_empty() {
                        vec!["(无)".to_string()]
                    } else {
                        players.clone()
                    };
                    // 左侧细线
                    let list_h = list.len() as f32 * PLAYER_FS * LINE_H
                        + (list.len().saturating_sub(1) as f32) * PLAYER_GAP;
                    push_rect(
                        svg,
                        x + ITEM_PAD,
                        cy,
                        2.0,
                        list_h,
                        0.0,
                        WHITE,
                        0.1,
                        None,
                    );
                    for (pi, p) in list.iter().enumerate() {
                        push_text(
                            svg,
                            fs,
                            cache,
                            p,
                            PLAYER_FS,
                            400.0,
                            0.0,
                            x + ITEM_PAD + PLAYERS_PL,
                            cy + pi as f32 * (PLAYER_FS * LINE_H + PLAYER_GAP),
                            BLACK,
                            0.7,
                        );
                    }
                }
            }
            ry += row_h;
        }
    };

    // 两列等宽：统一列宽取两个网格的最大值，保证左右对齐
    let col_w = routes_grid_w.max(servers_grid_w) / 2.0 - GRID_GAP / 2.0;
    draw_grid(&mut svg, &mut fs, &mut cache, &routes, routes_grid_top, col_w);
    draw_grid(&mut svg, &mut fs, &mut cache, &servers, servers_grid_top, col_w);

    // 分隔条
    svg.push_str(&format!(
        "<rect x=\"{content_x:.1}\" y=\"{divider_top:.1}\" width=\"{:.1}\" height=\"{DIVIDER_H:.1}\" fill=\"#FFFFFF\" fill-opacity=\"0.2\"/>",
        panel_w - PANEL_PAD * 2.0
    ));

    svg.push_str("</svg>");

    // ---- 光栅化 ----
    rasterize(&svg, body_w, body_h, panel_top, panel_top, panel_w, panel_h)
}

/// 背景铺底 + 面板区域套用预模糊结果 + resvg 渲染前景。
/// 清晰背景每次现算；**模糊结果**按画布尺寸缓存（见 [`blurred_background`]）。
fn rasterize(
    svg: &str,
    w: f32,
    h: f32,
    panel_x: f32,
    panel_y: f32,
    panel_w: f32,
    panel_h: f32,
) -> Result<Vec<u8>, RenderError> {
    use resvg::tiny_skia;
    let (iw, ih) = (w.round() as u32, h.round() as u32);
    let (iw, ih) = (iw.max(1), ih.max(1));

    // 清晰画布每次现算；模糊也现算、用完即释放（渲染稀疏，无需常驻缓存）
    let mut canvas = cover_canvas(iw, ih)
        .ok_or_else(|| RenderError::Background("解码失败".into()))?;
    {
        let blurred = blur_canvas(&canvas);
        // 面板矩形用模糊结果覆盖（整幅已模糊，边缘无接缝，无需外扩）
        paste_region(&mut canvas, &blurred, panel_x, panel_y, panel_w, panel_h);
    }

    let mut pixmap = tiny_skia::Pixmap::from_vec(canvas.into_raw(), tiny_skia::IntSize::from_wh(iw, ih).ok_or_else(|| RenderError::Raster("尺寸非法".into()))?)
        .ok_or_else(|| RenderError::Raster("画布创建失败".into()))?;

    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(svg, &opt)
        .map_err(|e| RenderError::Svg(e.to_string()))?;
    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    pixmap
        .encode_png()
        .map_err(|e| RenderError::Raster(e.to_string()))
}

/// 把 `src` 中 [x,y,w,h] 矩形逐行拷贝到 `dst` 同位置（clamp 到画布）。
/// 两张图尺寸相同；按行 `copy_from_slice` 走内存块，避免逐像素 get/put。
#[allow(clippy::too_many_arguments)]
fn paste_region(
    dst: &mut image::RgbaImage,
    src: &image::RgbaImage,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) {
    let (iw, ih) = (dst.width() as i64, dst.height() as i64);
    let x0 = (x.floor() as i64).clamp(0, iw);
    let y0 = (y.floor() as i64).clamp(0, ih);
    let x1 = ((x + w).ceil() as i64).clamp(0, iw);
    let y1 = ((y + h).ceil() as i64).clamp(0, ih);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let row_bytes = ((x1 - x0) as usize) * 4;
    let stride = (iw as usize) * 4;
    let src_raw = src.as_raw();
    let dst_raw = dst.as_mut();
    for yy in y0..y1 {
        let row = (yy as usize) * stride;
        let col = (x0 as usize) * 4;
        let s = row + col;
        dst_raw[s..s + row_bytes].copy_from_slice(&src_raw[s..s + row_bytes]);
    }
}

// ---------------------------- v0.5 播报长图（黑底白字，极简） ----------------------------

/// 播报长图的内容块（由 HTML 粗剥离得到，不追求结构还原）。
#[derive(Clone, Debug)]
pub enum Block {
    /// 标题（h1/h2/h3）
    Title(String),
    /// 普通段落（p）
    Paragraph(String),
    /// 列表项（li）
    Item(String),
}

const BULLETIN_W: f32 = 900.0;
const BULLETIN_PAD: f32 = 32.0;
const BULLETIN_TITLE_FS: f32 = 28.0;
const BULLETIN_H_FS: f32 = 20.0;
const BULLETIN_BODY_FS: f32 = 18.0;
const BULLETIN_FOOT_FS: f32 = 13.0;
const BULLETIN_BG: (u8, u8, u8) = (0x0a, 0x0a, 0x0a);
const BULLETIN_FG: (u8, u8, u8) = (0xf2, 0xf2, 0xf2);
const BULLETIN_DIM: (u8, u8, u8) = (0x8a, 0x8f, 0x99);

/// 播报长图专用字体：覆盖常用汉字的宽字符集子集（ASCII + GB2312 全表），
/// 内嵌进二进制。与状态图那套「窄子集」（只有几十个固定汉字）分离——播报要
/// 渲染任意中文补丁说明，窄子集会大片缺字。
const BULLETIN_FONT_REGULAR: &[u8] =
    include_bytes!("../../assets/fonts/bulletin/NotoSansCJKsc-Regular.otf");
const BULLETIN_FONT_BOLD: &[u8] =
    include_bytes!("../../assets/fonts/bulletin/NotoSansCJKsc-Bold.otf");

/// 播报专用的字体系统（仅装内嵌的宽字符集字体，不扫描系统字体）。
fn bulletin_font_system() -> &'static Mutex<FontSystem> {
    static FS: OnceLock<Mutex<FontSystem>> = OnceLock::new();
    FS.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_font_data(BULLETIN_FONT_REGULAR.to_vec());
        db.load_font_data(BULLETIN_FONT_BOLD.to_vec());
        Mutex::new(FontSystem::new_with_locale_and_db("zh-CN".to_string(), db))
    })
}

fn block_text(block: &Block) -> String {
    match block {
        Block::Title(s) => s.clone(),
        Block::Paragraph(s) => s.clone(),
        Block::Item(s) => format!("• {s}"),
    }
}

fn block_font(block: &Block) -> (f32, f32) {
    match block {
        Block::Title(_) => (BULLETIN_H_FS, 700.0),
        _ => (BULLETIN_BODY_FS, 400.0),
    }
}

/// 渲染 v0.5 播报长图：黑底白字、固定 900px 宽、高度自适应，标题在顶部。
/// 复用 /server 的 cosmic-text 整形 → glyph path → resvg 管线；无背景/毛玻璃/图标。
pub fn render_bulletin_png(title: &str, blocks: &[Block]) -> Result<Vec<u8>, RenderError> {
    let mut fs = bulletin_font_system().lock().unwrap();
    if !font_available(&mut fs) {
        return Err(RenderError::FontNotFound(FONT_FAMILY.to_string()));
    }
    let mut cache = swash_cache().lock().unwrap();

    let content_w = BULLETIN_W - BULLETIN_PAD * 2.0;
    let x = BULLETIN_PAD;

    // 第一遍：量高度
    let title_h = push_measure(&mut fs, title, BULLETIN_TITLE_FS, 700.0, content_w);
    let mut y = BULLETIN_PAD + title_h + 16.0;
    let mut heights: Vec<f32> = Vec::with_capacity(blocks.len());
    for block in blocks {
        let (fsize, weight) = block_font(block);
        let text = block_text(block);
        let h = push_measure(&mut fs, &text, fsize, weight, content_w);
        heights.push(h);
        y += h + if matches!(block, Block::Title(_)) { 12.0 } else { 8.0 };
    }
    y += BULLETIN_FOOT_FS * LINE_H + 20.0;
    let total_h = y + BULLETIN_PAD;

    // 第二遍：绘制
    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{BULLETIN_W:.0}\" height=\"{total_h:.0}\" viewBox=\"0 0 {BULLETIN_W:.0} {total_h:.0}\">"
    ));
    let (br, bg, bb) = BULLETIN_BG;
    svg.push_str(&format!(
        "<rect x=\"0\" y=\"0\" width=\"{BULLETIN_W:.0}\" height=\"{total_h:.0}\" fill=\"#{br:02X}{bg:02X}{bb:02X}\"/>"
    ));
    let mut cy = BULLETIN_PAD;
    push_flow(&mut svg, &mut fs, &mut cache, title, BULLETIN_TITLE_FS, 700.0, 0.0, x, cy, BULLETIN_FG, 1.0, Some(content_w));
    cy += title_h + 16.0;
    for (i, block) in blocks.iter().enumerate() {
        let (fsize, weight) = block_font(block);
        let text = block_text(block);
        push_flow(&mut svg, &mut fs, &mut cache, &text, fsize, weight, 0.0, x, cy, BULLETIN_FG, 1.0, Some(content_w));
        cy += heights[i] + if matches!(block, Block::Title(_)) { 12.0 } else { 8.0 };
    }
    push_flow(
        &mut svg,
        &mut fs,
        &mut cache,
        "由 cxu-chathub 自动翻译/渲染 · 内容来自 Mojang 官方补丁说明",
        BULLETIN_FOOT_FS,
        400.0,
        0.0,
        x,
        cy + 20.0,
        BULLETIN_DIM,
        1.0,
        Some(content_w),
    );
    svg.push_str("</svg>");

    rasterize_plain(&svg, BULLETIN_W, total_h)
}

/// 量一段（可换行）文本的高度。
fn push_measure(fs: &mut FontSystem, text: &str, size: f32, weight: f32, width: f32) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let buffer = shape_width(fs, text, size, css_weight(weight), Some(width));
    buffer.layout_runs().count() as f32 * size * LINE_H
}

/// 纯色背景光栅化（无背景图 / 无模糊）。
fn rasterize_plain(svg: &str, w: f32, h: f32) -> Result<Vec<u8>, RenderError> {
    use resvg::tiny_skia;
    let iw = (w.round() as u32).max(1);
    let ih = (h.round() as u32).max(1);
    let mut pixmap = tiny_skia::Pixmap::new(iw, ih)
        .ok_or_else(|| RenderError::Raster("画布创建失败".into()))?;
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(svg, &opt).map_err(|e| RenderError::Svg(e.to_string()))?;
    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    pixmap
        .encode_png()
        .map_err(|e| RenderError::Raster(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status_data() -> Value {
        json!({
            "network_routes": [
                {"route_name": "节点A", "online": true, "latency": 21.5, "packet_loss": 0.0},
            ],
            "servers": [
                {"server_name": "服务端A", "online": true, "online_players": ["• 玩家A"]},
                {"server_name": "服务端B", "online": false, "online_players": []},
            ],
        })
    }

    #[test]
    fn default_addresses_when_none() {
        let r = route_cards(&status_data());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "节点A");
        assert_eq!(r[0].details[0], "延迟: 21.50ms");
        assert_eq!(r[0].details[1], "丢包: 0.0%");
    }

    #[test]
    fn server_cards_players_and_offline() {
        let s = server_cards(&status_data());
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].players.as_deref(), Some(&["玩家A".to_string()][..]));
        // 离线服务器不显示玩家区
        assert!(s[1].players.is_none());
        assert!(!s[1].online);
    }

    #[test]
    fn non_dict_rows_skipped() {
        let data = json!({
            "network_routes": ["非法", {"route_name": "节点B", "online": false}],
            "servers": ["非法", {"server_name": "服X", "online": true, "online_players": []}],
        });
        let r = route_cards(&data);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "节点B");
        assert_eq!(server_cards(&data).len(), 1);
    }

    #[test]
    fn empty_players_shows_dash() {
        let data = json!({
            "servers": [{"server_name": "空服", "online": true, "online_players": []}],
        });
        let s = server_cards(&data);
        assert_eq!(s[0].players.as_deref(), Some(&[][..]));
    }

    #[test]
    fn commands_to_path_basic() {
        use cosmic_text::Command;
        let cmds = [
            Command::MoveTo((0.0, 0.0).into()),
            Command::LineTo((10.0, -10.0).into()),
            Command::Close,
        ];
        let d = commands_to_path(&cmds, 5.0, 100.0);
        assert_eq!(d, "M5.0 100.0L15.0 110.0Z");
    }

    /// 真渲染：需要容器内有 fonts-noto-cjk。缺字体时静默跳过（CI 不装字体）。
    #[tokio::test]
    async fn render_png_when_font_available() {
        let mut fs = font_system().lock().unwrap();
        if !font_available(&mut fs) {
            return; // 无字体环境：跳过
        }
        drop(fs);
        let png = render_status_png(&status_data(), None)
            .await
            .expect("字体可用时应渲染成功");
        assert!(png.len() > 1_000);
        assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']);
    }
}
