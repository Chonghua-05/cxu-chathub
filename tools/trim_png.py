#!/usr/bin/env python3
"""裁剪 PNG 透明边并缩放（仅用标准库：zlib + struct）。

用法: trim_png.py <in.png> <out.png> <target_height>
- 读入 RGBA8 PNG（Chromium 无头导出的格式），取 alpha>阈值 的包围盒裁剪，
  再 box 平均缩小到 target_height（保持宽高比），重编码为 RGBA8 PNG。
仅支持 8 位 PNG（color type 6/2/0/4），够本仓库图标生成用。
"""
import struct, sys, zlib


def read_png(path):
    d = open(path, "rb").read()
    assert d[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    pos, idat, w, h, ct = 8, b"", None, None, None
    while pos < len(d):
        ln = struct.unpack(">I", d[pos:pos + 4])[0]
        typ = d[pos + 4:pos + 8]
        data = d[pos + 8:pos + 8 + ln]
        pos += 12 + ln
        if typ == b"IHDR":
            w, h, bd, ct, comp, filt, inter = struct.unpack(">IIBBBBB", data)
            assert bd == 8, "only 8-bit PNG supported"
        elif typ == b"IDAT":
            idat += data
        elif typ == b"IEND":
            break
    raw = zlib.decompress(idat)
    ch = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[ct]
    stride = w * ch
    out = bytearray(w * h * ch)
    prev = bytearray(stride)
    p = 0
    for y in range(h):
        f = raw[p]; p += 1
        line = bytearray(raw[p:p + stride]); p += stride
        if f == 1:
            for i in range(ch, stride):
                line[i] = (line[i] + line[i - ch]) & 255
        elif f == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 255
        elif f == 3:
            for i in range(stride):
                a = line[i - ch] if i >= ch else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 255
        elif f == 4:
            for i in range(stride):
                a = line[i - ch] if i >= ch else 0
                b = prev[i]
                c = prev[i - ch] if i >= ch else 0
                pp = a + b - c
                pa, pb, pc = abs(pp - a), abs(pp - b), abs(pp - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 255
        out[y * stride:(y + 1) * stride] = line
        prev = line
    return w, h, ch, out


def write_png_rgba(path, w, h, px):
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        raw += px[y * w * 4:(y + 1) * w * 4]

    def chunk(t, dBytes):
        c = t + dBytes
        return struct.pack(">I", len(dBytes)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

    png = (b"\x89PNG\r\n\x1a\n"
           + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
           + chunk(b"IEND", b""))
    open(path, "wb").write(png)


def main():
    src, dst, target = sys.argv[1], sys.argv[2], int(sys.argv[3])
    w, h, ch, px = read_png(src)
    assert ch == 4, "需要 RGBA PNG"

    x0, y0, x1, y1 = w, h, -1, -1
    for y in range(h):
        base = y * w * 4
        for x in range(w):
            if px[base + x * 4 + 3] > 8:
                x0 = min(x0, x); x1 = max(x1, x)
                y0 = min(y0, y); y1 = max(y1, y)
    cw, chh = x1 - x0 + 1, y1 - y0 + 1

    r = max(1, round(chh / target))
    nw, nh = (cw + r - 1) // r, (chh + r - 1) // r
    o = bytearray(nw * nh * 4)
    for yy in range(nh):
        for xx in range(nw):
            ar = ag = ab = aa = n = 0
            for dy in range(r):
                for dx in range(r):
                    sx, sy = x0 + xx * r + dx, y0 + yy * r + dy
                    if sx < w and sy < h:
                        i = (sy * w + sx) * 4
                        ar += px[i]; ag += px[i + 1]; ab += px[i + 2]; aa += px[i + 3]; n += 1
            j = (yy * nw + xx) * 4
            if n:
                o[j:j + 4] = bytes([ar // n, ag // n, ab // n, aa // n])
    write_png_rgba(dst, nw, nh, o)
    print(f"{dst}: crop {cw}x{chh} -> {nw}x{nh}")


if __name__ == "__main__":
    main()
