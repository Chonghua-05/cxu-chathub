#!/usr/bin/env bash
# 图标资源准备脚本：把 ✅ / ❌ 从系统 Noto Color Emoji 渲染成 PNG，裁剪透明边、缩小后
# 落到本目录（ok.png / fail.png）。**一次性离线生成，不进构建流程**；生成结果提交进仓库。
#
# 依赖：chromium（无头）+ python3（标准库）。二者只需在“生成图标的机器”上存在。
# 用法：
#   ./prepare_icons.sh                       # 用 PATH 里的 chromium / python3
#   CHROME=/usr/bin/chromium ./prepare_icons.sh
#
# 生成原理（与线上现状同源）：用 Chromium 把 emoji 字形渲染到透明画布 → 取 alpha 包围盒
# 裁剪 → box 缩小到目标高度（默认 96px，供状态图里 24px 显示，4x 冗余）→ 重编码 PNG。
set -euo pipefail

CHROME="${CHROME:-chromium}"
PY="${PY:-python3}"
HERE="$(cd "$(dirname "$0")" && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

emit_html() { # $1=emoji $2=out
  printf '<html><head><meta charset="utf-8"></head><body style="margin:0;background:transparent;padding:60px"><div style="font-size:180px;line-height:1;display:inline-block">%s</div></body></html>' "$1" > "$2"
}

render() { # $1=html $2=out.png
  "$CHROME" --headless=new --no-sandbox --disable-gpu \
    --default-background-color=00000000 --window-size=360,360 \
    --screenshot="$2" "file://$1" >/dev/null 2>&1
}

emit_html "✅" "$TMP/ok.html"
emit_html "❌" "$TMP/fail.html"
render "$TMP/ok.html"   "$TMP/ok_raw.png"
render "$TMP/fail.html" "$TMP/fail_raw.png"

"$PY" "$HERE/trim_png.py" "$TMP/ok_raw.png"   "$HERE/ok.png"   96
"$PY" "$HERE/trim_png.py" "$TMP/fail_raw.png" "$HERE/fail.png" 96

echo "已生成：$HERE/ok.png $HERE/fail.png"
