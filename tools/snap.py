"""用 Windows GDI（纯标准库 ctypes）截取全屏，避免依赖 PIL/PowerShell。"""
import ctypes
from ctypes import wintypes

user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32
user32.SetProcessDPIAware()

# 用 BMP 保存（GDI 原生支持），无需额外库
class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [
        ("biSize", wintypes.DWORD), ("biWidth", wintypes.LONG), ("biHeight", wintypes.LONG),
        ("biPlanes", wintypes.WORD), ("biBitCount", wintypes.WORD), ("biCompression", wintypes.DWORD),
        ("biSizeImage", wintypes.DWORD), ("biXPelsPerMeter", wintypes.LONG), ("biYPelsPerMeter", wintypes.LONG),
        ("biClrUsed", wintypes.DWORD), ("biClrImportant", wintypes.DWORD),
    ]

def main():
    w = user32.GetSystemMetrics(0)
    h = user32.GetSystemMetrics(1)
    hdc = user32.GetDC(0)
    memdc = gdi32.CreateCompatibleDC(hdc)
    bmp = gdi32.CreateCompatibleBitmap(hdc, w, h)
    gdi32.SelectObject(memdc, bmp)
    gdi32.BitBlt(memdc, 0, 0, w, h, hdc, 0, 0, 0x00CC0020)  # SRCCOPY

    hdr = BITMAPINFOHEADER()
    hdr.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    hdr.biWidth = w
    hdr.biHeight = h  # 正数 = bottom-up
    hdr.biPlanes = 1
    hdr.biBitCount = 24
    hdr.biCompression = 0
    stride = ((w * 3 + 3) // 4) * 4
    size = stride * h
    buf = ctypes.create_string_buffer(size)
    gdi32.GetDIBits(memdc, bmp, 0, h, buf, ctypes.byref(hdr), 0)

    import os
    out = os.path.join(os.path.dirname(__file__), "screenshot.bmp")
    with open(out, "wb") as f:
        # BMP 文件头
        f.write(b"BM")
        f.write((54 + size).to_bytes(4, "little"))
        f.write((0).to_bytes(4, "little"))
        f.write((54).to_bytes(4, "little"))
        # DIB 头
        f.write(bytes(ctypes.string_at(ctypes.byref(hdr), ctypes.sizeof(hdr))))
        f.write(buf.raw)

    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(memdc)
    user32.ReleaseDC(0, hdc)
    print(f"saved: {out}  {w}x{h}")

if __name__ == "__main__":
    main()
