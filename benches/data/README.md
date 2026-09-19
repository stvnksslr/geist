# Benchmark corpora

Real captured VT streams for the benches, mirroring Ghostty's `--data <file>`
corpus approach (feed a benchmark a realistic byte stream instead of only the
seeded synthetic generators in `src/synthetic.rs`).

## Using a corpus

Set `GIEST_BENCH_DATA` to a file path; the `stream` bench (and anything that calls
`synthetic::corpus`) will use its bytes verbatim instead of the synthetic
generator, and label the case `corpus`:

```powershell
$env:GIEST_BENCH_DATA = "benches\data\ansi-sample.vt"
mise exec zig@0.16.0 -- cargo bench --bench stream
Remove-Item Env:\GIEST_BENCH_DATA   # back to synthetic
```

## What's checked in

- **`ansi-sample.vt`** — a small (~15 KB) representative stream: SGR colors
  (16/256/truecolor), box-drawing, cursor/line ops (`\r`, `CR`, erase-line), and
  multibyte text (accents, CJK, emoji). Deliberately tiny to keep the repo light —
  it exercises the parser's escape-heavy paths but is too small for stable
  throughput numbers. For real measurements, capture a larger stream (below).

## Capturing a larger corpus

The point of a corpus is a *real* workload. Capture the raw bytes a program emits
**to a terminal** (escape sequences only appear when stdout is a tty/ConPTY, not a
plain pipe). Some options:

- Record a session with [`asciinema`](https://asciinema.org) and extract the output
  bytes from the `.cast` file, or
- On Linux/macOS, `script -q out.vt -c 'ls --color=always -R /usr; cat bigfile'`, or
- Replay a known stress file (e.g. a large `cat` of source, `htop` for a few
  seconds, a noisy build log).

Keep large captures **out of git** — point `GIEST_BENCH_DATA` at a local file.
Only small, representative samples belong here.

> For an apples-to-apples comparison, feed the *same* corpus file to Ghostty's own
> `ghostty-bench terminal-stream --data <file>` — see `scripts/bench-vs-ghostty.ps1`.
