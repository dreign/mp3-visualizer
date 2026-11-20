"""找到 MP3 可视化程序窗口，置于前台，然后截图。纯标准库。"""
import ctypes
import os
import time
from ctypes import wintypes

user32 = ctypes.windll.user32
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
    print("found windows:", wins)
    if not wins:
        print("未找到程序窗口，请确认程序正在运行")
        return
    # 选可见且在任务栏的顶层窗口；多个实例时取第一个可见的
    hwnd, title = wins[0]
    user32.ShowWindow(hwnd, 9)   # SW_RESTORE
    # 绕过 Windows 前台锁定：附加到当前前台线程再置前
    fg = user32.GetForegroundWindow()
    tid_fg = user32.GetWindowThreadProcessId(fg, None)
    tid_me = user32.GetWindowThreadProcessId(hwnd, None)
    user32.AttachThreadInput(tid_me, tid_fg, True)
    user32.BringWindowToTop(hwnd)
    user32.SetForegroundWindow(hwnd)
    user32.SetActiveWindow(hwnd)
    user32.AttachThreadInput(tid_me, tid_fg, False)
    time.sleep(1.2)

    # 截图（复用 snap.py 逻辑）
    import importlib.util
    spec = importlib.util.spec_from_file_location("snap", os.path.join(os.path.dirname(__file__), "snap.py"))
    snap = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(snap)
    snap.main()

if __name__ == "__main__":
    main()
