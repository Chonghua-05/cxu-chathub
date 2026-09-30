#!/usr/bin/env python3
"""生成状态图渲染用的 Noto Sans CJK SC「子集」字体。

背景：状态图渲染只用到一小撮汉字（固定标签 + 线路名 + 地址标签），动态内容
（延迟/丢包数字、玩家 ID、服务器名）都是 ASCII。而完整 fonts-noto-cjk 的 4 个
ttc（~93MB）会被 fontdb 全部载入，渲染后常驻数十 MB。这里把 Noto Sans CJK SC
的 Regular / Bold 两个字面按所需字符集做子集，产出 ~100KB 的 otf，替代完整字体。

用法：
    # 需要 fonttools： uv pip install --target ./pylibs fonttools
    PYTHONPATH=./pylibs python3 tools/make_font_subset.py \
        --regular <NotoSansCJK-Regular.ttc> --bold <NotoSansCJK-Bold.ttc> \
        --out rust/assets/fonts

所需字符集的来源（改渲染文案时记得同步）：
  1. `rust/src/services/status_render.rs` 里所有字符串字面量的非 ASCII 字符；
  2. 地址标签（config.chatroom.server_addresses 的 label，见 config.example.json）；
  3. 线路名 route_name（来自 status API，形如 “RMS主线路 / RMS海外加速线路 / RMS 备用线路”）。
玩家名不含中文，故不纳入。
"""
import argparse
import os
import re
import string
import subprocess
import sys

# 线路名 / 地址标签的固定汉字（改配置或 API 文案时需同步更新）
EXTRA_HANZI = "主IP海外加速IP（中国香港）备用地址RMS主线路海外加速线路RMS 备用线路"
# 附带的全角标点等（渲染文案里可能出现）
EXTRA_PUNCT = "—…·•：、！？"

SC_FACE_HINT = "Noto Sans CJK SC"


def collect_chars(render_rs: str) -> str:
    chars = set(string.printable)
    chars.update("".join(EXTRA_HANZI))
    chars.update(EXTRA_PUNCT)
    src = open(render_rs, encoding="utf-8").read()
    for lit in re.findall(r'"((?:[^"\\]|\\.)*)"', src):
        for ch in lit:
            if ord(ch) > 0x2000:
                chars.add(ch)
    return "".join(sorted(chars))


def find_sc_face(ttc_path: str) -> int:
    from fontTools.ttLib import TTCollection

    coll = TTCollection(ttc_path, lazy=True)
    try:
        for i, font in enumerate(coll.fonts):
            if font["name"].getDebugName(1) == SC_FACE_HINT:
                return i
    finally:
        coll.close()
    raise SystemExit(f"{ttc_path}: 找不到 {SC_FACE_HINT!r} 字面")


def subset(ttc_path: str, out_path: str, chars_file: str) -> None:
    face = find_sc_face(ttc_path)
    cmd = [
        sys.executable, "-m", "fontTools.subset", ttc_path,
        f"--font-number={face}",
        f"--text-file={chars_file}",
        f"--output-file={out_path}",
        "--layout-features=*",
        "--name-IDs=*",          # 保留 name 表，family 仍为 “Noto Sans CJK SC”（fontdb 按此名查询）
        "--name-languages=*",
        "--notdef-outline",
        "--recommended-glyphs",
        "--drop-tables+=DSIG",
    ]
    subprocess.run(cmd, check=True)
    print(f"[ok] {out_path}  {os.path.getsize(out_path)} bytes")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--regular", required=True)
    ap.add_argument("--bold", required=True)
    ap.add_argument("--out", default="rust/assets/fonts")
    ap.add_argument("--render-rs", default="rust/src/services/status_render.rs")
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    chars = collect_chars(args.render_rs)
    chars_file = os.path.join(args.out, ".subset-chars.txt")
    with open(chars_file, "w", encoding="utf-8") as f:
        f.write(chars)
    print(f"字符集 {len(chars)} 个")

    subset(args.regular, os.path.join(args.out, "NotoSansCJKsc-Regular.otf"), chars_file)
    subset(args.bold, os.path.join(args.out, "NotoSansCJKsc-Bold.otf"), chars_file)


if __name__ == "__main__":
    main()
