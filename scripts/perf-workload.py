"""Deterministic workloads to run *inside* a terminal while profiling it.

    python scripts/perf-workload.py flood --secs 10 --out result.json
    python scripts/perf-workload.py tui   --secs 10 --out result.json

flood: colored log-style lines (SGR fg/bg, bold, a long line now and then),
       scrolling as fast as the terminal accepts them. Reports MiB/s.
tui:   a fullscreen app on the alternate screen that redraws *every* row each
       frame inside a synchronized update (DEC 2026) -- the pattern of Claude
       Code, htop, lazygit and friends. Reports frames/s.

Throughput here is what the *program* sees: a terminal that drains its PTY
slowly makes these writes block, the same quantity vtebench and perf-flood.ps1
time. Run it via `geist -e python scripts/perf-workload.py ...` (or a .cmd
wrapper) and point tools/sampler at the geist pid meanwhile.
"""

import argparse
import json
import os
import random
import sys
import time

ESC = "\x1b"


def flood_chunk(rng, cols, lines=400):
    out = []
    words = ["error", "warn", "info", "debug", "compile", "geist", "render", "atlas",
             "pty", "frame", "snapshot", "shaping", "=>", "!=", "->", "::", "0x7ff3"]
    for i in range(lines):
        fg = 31 + rng.randrange(7)
        parts = [f"{ESC}[2m{i:06d}{ESC}[0m ", f"{ESC}[1;{fg}m{rng.choice(words):>8}{ESC}[0m "]
        n = cols - 16 if rng.random() > 0.05 else cols * 3  # occasional soft-wrap
        line = []
        while sum(len(w) + 1 for w in line) < n:
            line.append(rng.choice(words))
        text = " ".join(line)[:n]
        if rng.random() < 0.3:
            text = f"{ESC}[48;5;{rng.randrange(256)}m{text[:20]}{ESC}[0m{text[20:]}"
        parts.append(text)
        out.append("".join(parts))
    return ("\r\n".join(out) + "\r\n").encode()


def tui_frame(rng, cols, rows, n):
    buf = [f"{ESC}[?2026h{ESC}[H"]
    for y in range(rows):
        buf.append(f"{ESC}[{y + 1};1H")
        x = 0
        while x < cols:
            w = min(cols - x, 4 + rng.randrange(14))
            fg = rng.randrange(256)
            seg = "".join(chr(0x61 + (x + y + n + k) % 26) for k in range(w))
            if (y + n) % 7 == 0:
                buf.append(f"{ESC}[48;2;{(y * 9) % 256};{(n * 3) % 256};80m")
            buf.append(f"{ESC}[38;5;{fg}m{seg}{ESC}[0m")
            x += w
    buf.append(f"{ESC}[{rows};1H{ESC}[?2026l")
    return "".join(buf).encode()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["flood", "tui"])
    ap.add_argument("--secs", type=float, default=10)
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    try:
        cols, rows = os.get_terminal_size()
    except OSError:
        cols, rows = 120, 40
    rng = random.Random(1)
    w = sys.stdout.buffer
    start = time.perf_counter()
    nbytes = 0
    frames = 0
    if a.mode == "flood":
        chunks = [flood_chunk(rng, cols) for _ in range(8)]
        while time.perf_counter() - start < a.secs:
            c = chunks[frames % len(chunks)]
            w.write(c)
            w.flush()
            nbytes += len(c)
            frames += 1
    else:
        w.write(f"{ESC}[?1049h{ESC}[?25l".encode())
        pre = [tui_frame(rng, cols, rows, n) for n in range(32)]
        while time.perf_counter() - start < a.secs:
            f = pre[frames % len(pre)]
            w.write(f)
            w.flush()
            nbytes += len(f)
            frames += 1
        w.write(f"{ESC}[?25h{ESC}[?1049l".encode())
        w.flush()
    el = time.perf_counter() - start
    res = {"mode": a.mode, "cols": cols, "rows": rows, "secs": round(el, 2),
           "mib_s": round(nbytes / el / 2**20, 2), "frames": frames,
           "frames_s": round(frames / el, 1)}
    if a.out:
        with open(a.out, "w") as f:
            json.dump(res, f)
    print(json.dumps(res))


if __name__ == "__main__":
    main()
