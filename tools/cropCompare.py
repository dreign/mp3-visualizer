"""裁剪两帧截图的左盘中心区域，便于对比喇叭震动前后的差异。"""
import struct


def load_bmp(path):
    data = open(path, "rb").read()
    off = struct.unpack_from("<I", data, 10)[0]
    w = struct.unpack_from("<i", data, 18)[0]
    h = struct.unpack_from("<i", data, 22)[0]
    bpp = struct.unpack_from("<H", data, 28)[0]
    assert bpp == 24, bpp
    stride = ((w * 3 + 3) // 4) * 4
    pixels = bytearray(data[off:off + stride * h])
    return w, h, stride, pixels, data[:off]


def crop(src_path, dst_path, x0, y0, x1, y1):
    w, h, stride, px, header = load_bmp(src_path)
    cw, ch = x1 - x0, y1 - y0
    new_stride = ((cw * 3 + 3) // 4) * 4
    out = bytearray()
    for row in range(ch):
        # BMP 自下而上存储
        sy = h - 1 - (y0 + row)
        s = sy * stride + x0 * 3
        out.extend(px[s:s + cw * 3])
        out.extend(b"\x00" * (new_stride - cw * 3))
    new_header = bytearray(header)
    struct.pack_into("<i", new_header, 18, cw)
    struct.pack_into("<i", new_header, 22, ch)
    struct.pack_into("<I", new_header, 34, len(out))
    open(dst_path, "wb").write(new_header + out)
    return cw, ch, new_stride, out


# 左盘中心大约位于截图 (415, 350)，裁剪 200x200 区域
a = crop("tools/shot_a.bmp", "tools/zoom_a.bmp", 315, 250, 515, 450)
b = crop("tools/shot_b.bmp", "tools/zoom_b.bmp", 315, 250, 515, 450)
diff = sum(1 for i in range(len(a[3])) if a[3][i] != b[3][i])
print("crop size:", a[0], "x", a[1], " changed bytes in speaker region:", diff)
