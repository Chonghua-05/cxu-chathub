#!/usr/bin/env python3
"""生成「版本播报长图」专用的宽字符集中文子集（与状态图窄子集分离）。

状态图的窄子集（tools/make_font_subset.py）只含几十个固定汉字，够渲染状态图；
但 v0.5 版本播报要渲染任意中文补丁说明，窄子集会大片缺字。这里按
「ASCII + GB2312 全表 + 常见标点/全角」做一份宽字符集子集，内嵌进 bridge
二进制（见 status_render.rs 的 BULLETIN_FONT_*），专供 render_bulletin_png 使用。

用法：
    # 需要 fonttools： uv pip install --target ./pylibs fonttools
    PYTHONPATH=./pylibs python3 tools/make_bulletin_font.py \
        --regular <NotoSansCJK-Regular.ttc> --bold <NotoSansCJK-Bold.ttc> \
        --out rust/assets/fonts/bulletin
"""
import argparse
import os
import subprocess
import sys

SC_FACE_HINT = "Noto Sans CJK SC"

# 常见标点 / 全角 / 符号补充（GB2312 之外常用的）
EXTRA = (
    "　，。、；：？！“”‘’（）《》〈〉【】「」『』—…·•～￥％＆＊＋－＝／＼｜＠＃"
    "×÷°±§¶©®™→←↑↓★☆●○■□◆◇※′″€£¥№℃㎡①②③④⑤⑥⑦⑧⑨⑩"
)


def collect_chars() -> str:
    chars = set(chr(c) for c in range(0x20, 0x7F))  # ASCII 可打印
    # GB2312 符号区 + 汉字区（全表）
    for hi in range(0xA1, 0xF8):
        for lo in range(0xA1, 0xFF):
            try:
                chars.add(bytes([hi, lo]).decode("gb2312"))
            except Exception:
                pass
    chars.update(EXTRA)
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
    cmd = [
        sys.executable, "-m", "fontTools.subset", ttc_path,
        f"--font-number={find_sc_face(ttc_path)}",
        f"--text-file={chars_file}",
        f"--output-file={out_path}",
        "--layout-features=*",
        "--name-IDs=*",          # 保留 name 表，family 仍为 "Noto Sans CJK SC"（fontdb 按此名查询）
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
    ap.add_argument("--out", default="rust/assets/fonts/bulletin")
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    chars = collect_chars()
    chars_file = os.path.join(args.out, ".bulletin-chars.txt")
    with open(chars_file, "w", encoding="utf-8") as f:
        f.write(chars)
    print(f"字符集 {len(chars)} 个")

    subset(args.regular, os.path.join(args.out, "NotoSansCJKsc-Regular.otf"), chars_file)
    subset(args.bold, os.path.join(args.out, "NotoSansCJKsc-Bold.otf"), chars_file)


if __name__ == "__main__":
    main()
