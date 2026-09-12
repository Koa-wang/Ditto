#!/usr/bin/env python3
"""在真实 pty 里跑一遍 dtto，把渲染出的画面还原成文本，用于更新 README 的截图。

为什么需要这个脚本：
- TUI 的输出是 ANSI 转义流，不能直接 grep 出「屏幕长什么样」，必须按光标定位
  把字符填回一个二维网格才能还原。
- cursive 是**差分渲染**：只重画变化的格子。所以字节流里不会出现完整的
  「共 3 条」这种字符串（只有数字那一格被重写），靠 grep 字符串验证界面是错的。
- 退出阶段 crossterm 会 Clear(All) 擦屏，必须把这段截掉再还原，否则得到空白屏。

需要 pty 成为子进程的**控制终端**（setsid + TIOCSCTTY）：否则 crossterm 的
tty_fd() 会去 open("/dev/tty") 并以 ENXIO 失败。

用法：
    python3 tools/capture_screenshot.py            # 打印画面
    python3 tools/capture_screenshot.py --write    # 同时写回 README.md 的标记之间
"""

import argparse
import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time
import unicodedata

COLS, ROWS = 100, 12
BEGIN = "<<<DITTO-SCREENSHOT-BEGIN"
END = "<<<DITTO-SCREENSHOT-END"
README_BEGIN = "<!-- ditto-screenshot:start"
README_END = "<!-- ditto-screenshot:end"

# 放几条有代表性的内容：长 ASCII、中文、长 URL —— 正好覆盖截断与宽度计算。
SAMPLE = [
    "curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9' https://api.example.com/v1/items",
    "今天下午的会议纪要：讨论了三个方案，最终决定先做交互原型，下周再评估数据层",
    "https://example.com/a/very/long/path/that/keeps/going/and/going?q=1",
]


def char_width(ch: str) -> int:
    return 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1


def reconstruct(raw: str, rows: int, cols: int) -> list[str]:
    """把 ANSI 流转回屏幕文本。只处理 cursive/crossterm 实际会用到的序列。"""
    grid = [[" "] * cols for _ in range(rows)]
    row = col = 0
    i = 0
    while i < len(raw):
        ch = raw[i]
        if ch == "\x1b":
            m = re.match(r"\x1b\[([0-9;?]*)([a-zA-Z])", raw[i:])
            if m:
                params, final = m.group(1), m.group(2)
                nums = [int(x) for x in params.replace("?", "").split(";") if x.isdigit()]
                if final in "Hf":  # 光标定位
                    row = (nums[0] - 1) if nums else 0
                    col = (nums[1] - 1) if len(nums) > 1 else 0
                elif final == "J":  # 擦屏
                    grid = [[" "] * cols for _ in range(rows)]
                elif final == "K":  # 擦到行尾
                    for x in range(col, cols):
                        grid[row][x] = " "
                i += m.end()
                continue
            for pattern in (r"\x1b\][^\x07]*\x07", r"\x1b[()][A-Z0-9]"):
                m = re.match(pattern, raw[i:])
                if m:
                    i += m.end()
                    break
            else:
                i += 1
            continue

        if ch == "\r":
            col = 0
        elif ch == "\n":
            row, col = min(row + 1, rows - 1), 0
        elif ch == "\b":
            col = max(col - 1, 0)
        elif ch >= " ":
            w = char_width(ch)
            if row < rows and col < cols:
                grid[row][col] = ch
                for k in range(1, w):  # 宽字符的占位格
                    if col + k < cols:
                        grid[row][col + k] = ""
            col += w
        i += 1

    return ["".join(line).rstrip() for line in grid]


def capture(binary: str) -> str:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    def become_controlling_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    proc = subprocess.Popen(
        [binary],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        preexec_fn=become_controlling_terminal,
        close_fds=True,
    )
    os.close(slave)

    chunks: list[bytes] = []

    def pump(seconds: float) -> None:
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([master], [], [], 0.05)
            if master in ready:
                try:
                    data = os.read(master, 1 << 16)
                except OSError:
                    return
                if not data:
                    return
                chunks.append(data)

    for item in SAMPLE:
        pump(1.7)
        subprocess.run(["pbcopy"], input=item.encode(), check=True)

    pump(2.0)
    os.write(master, b"\x1b")  # 退出
    pump(1.2)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()

    raw = b"".join(chunks).decode("utf-8", "replace")
    # 退出阶段会擦屏，必须截掉，否则还原出来是空白
    cut = raw.find("\x1b[?1049l")
    return raw[:cut] if cut != -1 else raw


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--write", action="store_true", help="写回 README.md")
    parser.add_argument("--binary", default="./target/debug/dtto")
    args = parser.parse_args()

    if not os.path.exists(args.binary):
        print(f"找不到 {args.binary}，先 cargo build", file=sys.stderr)
        return 1

    screen = reconstruct(capture(args.binary), ROWS, COLS)
    print(BEGIN)
    print("\n".join(screen))
    print(END)

    if not args.write:
        return 0

    readme = open("README.md", encoding="utf-8").read().split("\n")
    out, inside = [], False
    for line in readme:
        if README_BEGIN in line:
            inside = True
            out += [line, "```"] + screen + ["```"]
            continue
        if README_END in line:
            inside = False
            out.append(line)
            continue
        if not inside:
            out.append(line)
    open("README.md", "w", encoding="utf-8").write("\n".join(out))
    print("已写回 README.md", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
