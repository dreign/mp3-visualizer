"""只截取程序窗口的客户区（不含标题栏/边框），坐标稳定，便于局部裁剪检查。纯标准库。"""
import ctypes
import struct
import time
from ctypes import wintypes

user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32
user32.SetProcessDPIAware()

ENUM = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)


def find_window():
    found = []

    def cb(hwnd, _):
        if not user32.IsWindowVisible(hwnd):
            return True
        n = user32.GetWindowTextLengthW(hwnd)
        if n == 0:
            return True
        buf = ctypes.create_unicode_buffer(n + 1)
        user32.GetWindowTextW(hwnd, buf, n + 1)
        title = buf.value
        if "MP3" in title and ("可视化" in title or "Visual" in title):
            found.append((hwnd, title))
        return True

    user32.EnumWindows(ENUM(cb), 0)
    return found


def main():
    wins = find_window()
    if not wins:
        print("未找到窗口")
        return
    hwnd = wins[0][0]
    user32.ShowWindow(hwnd, 9)
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.8)

    rect = wintypes.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(rect))
    w, h = rect.right, rect.bottom
    pt = wintypes.POINT(0, 0)
    user32.ClientToScreen(hwnd, ctypes.byref(pt))

    hdc_screen = user32.GetDC(0)
    hdc = gdi32.CreateCompatibleDC(hdc_screen)
    bmp = gdi32.CreateCompatibleBitmap(hdc_screen, w, h)
    gdi32.SelectObject(hdc, bmp)
    gdi32.BitBlt(hdc, 0, 0, w, h, hdc_screen, pt.x, pt.y, 0x00CC0020)

    # BITMAPINFOHEADER for 32bpp top-down
    bi = struct.pack('<IiiHHIIiiII', 40, w, -h, 1, 32, 0, 0, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(w * h * 4)
    gdi32.GetDIBits(hdc, bmp, 0, h, buf, bi, 0)

    out = bytearray()
    out += struct.pack('<2sIHHI', b'BM', 54 + len(buf.raw), 0, 0, 54)
    out += bi
    out += buf.raw
    with open('tools/win.bmp', 'wb') as f:
        f.write(out)

    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(hdc)
    user32.ReleaseDC(0, hdc_screen)
    print(f"client {w}x{h} -> tools/win.bmp")


if __name__ == '__main__':
    main()
