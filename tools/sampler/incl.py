"""Inclusive share of named frames in a sampler `--folded` file.

    python tools/sampler/incl.py target/perf-profile/geist-tui.folded [substr ...]

With no substrings, prints every geist:: frame (and the engine's C entry
points) above 0.5%. Waits are excluded from the total, so shares are of the
time the thread was actually running.
"""

import sys
from collections import Counter

WAITS = ("NtWaitFor", "ZwWaitFor", "NtUserMsgWaitFor", "NtDelayExecution", "NtUserGetMessage",
         "NtGdiDdDDIWaitFor", "NtRemoveIoCompletion", "NtReadFile")

path, pats = sys.argv[1], sys.argv[2:]
incl = Counter()
total = 0
for line in open(path, encoding="utf-8"):
    stack, _, n = line.rstrip("\n").rpartition(" ")
    frames = stack.split(";")
    if frames[-1].startswith(WAITS):
        continue
    n = int(n)
    total += n
    for f in set(frames[1:]):
        if pats:
            if any(p in f for p in pats):
                incl[f] += n
        elif f.startswith("geist::") or f.startswith("ghostty_") or "vt_write" in f:
            incl[f] += n
print(f"{total} running samples")
for f, n in incl.most_common(60):
    if n * 200 >= total:
        print(f"{n * 100 / total:6.1f}%  {f[:140]}")
