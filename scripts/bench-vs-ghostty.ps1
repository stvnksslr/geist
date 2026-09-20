#!/usr/bin/env pwsh
#requires -version 7
<#
.SYNOPSIS
  Apples-to-apples engine-throughput comparison: giest's `stream` bench vs
  Ghostty's own `ghostty-bench terminal-stream`, over the SAME corpus file.

.DESCRIPTION
  giest reuses Ghostty's VT engine (libghostty-vt). This script builds Ghostty's
  benchmark binary from the *same pinned commit* the vt lib is built from, runs
  `ghostty-bench terminal-stream --data <corpus>` (wall-clock timed, the way
  Ghostty's own methodology measures it), then runs giest's `stream` bench with
  `GIEST_BENCH_DATA` pointed at the same corpus, and prints both throughputs.

  It degrades gracefully: if Zig 0.16.0, the ghostty source, or a corpus can't be
  found it prints a clear notice and skips that half rather than failing.

  Note on methodology: the two harnesses differ (Ghostty = single-pass wall clock
  incl. process startup; giest = criterion steady-state). Use a LARGE corpus
  (tens of MiB+) so per-run processing dominates startup, and read the numbers as
  a relative ratio, not an exact figure. See docs/benchmarking.md.

.PARAMETER Corpus
  Path to the VT byte stream to feed both tools. Defaults to the small checked-in
  sample (too small for stable numbers — pass a real capture for measurements).

.PARAMETER Cols / Rows
  Terminal dimensions for both tools (kept identical for fairness).

.PARAMETER Runs
  Repetitions for the ghostty-bench wall-clock timing when hyperfine is absent.

.EXAMPLE
  ./scripts/bench-vs-ghostty.ps1 -Corpus C:\caps\big-session.vt -Cols 80 -Rows 24
#>
[CmdletBinding()]
param(
    [string]$Corpus = "$PSScriptRoot/../benches/data/ansi-sample.vt",
    [int]$Cols = 80,
    [int]$Rows = 24,
    [int]$Runs = 20,
    # Skip building/running Ghostty's bench (just run giest's side over the corpus).
    [switch]$SkipGhostty
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path "$PSScriptRoot/..").Path
$GHOSTTY_COMMIT = 'b869a6e5ab0a50ce01e8eb5aa408a02b3cbe4f3a'
$GHOSTTY_REPO = 'https://github.com/ghostty-org/ghostty.git'

function Note($msg) { Write-Host "›› $msg" -ForegroundColor Cyan }
function Warn($msg) { Write-Host "!! $msg" -ForegroundColor Yellow }

# --- Resolve a Zig 0.16.0 invoker (prefer mise, like the project's build) -------
$zigPrefix = $null
if (Get-Command mise -ErrorAction SilentlyContinue) {
    $zigPrefix = @('mise', 'exec', 'zig@0.16.0', '--')
} elseif (Get-Command zig -ErrorAction SilentlyContinue) {
    $zigPrefix = @()  # zig already on PATH
} else {
    Warn "Neither mise nor zig found on PATH; cannot build ghostty-bench. Skipping the Ghostty side."
}

# --- Resolve the ghostty source (env override > fetched copy > fresh clone) ------
function Find-GhosttySource {
    if ($env:GHOSTTY_SOURCE_DIR -and (Test-Path "$env:GHOSTTY_SOURCE_DIR/build.zig")) {
        return (Resolve-Path $env:GHOSTTY_SOURCE_DIR).Path
    }
    # The vt-sys build fetches the pinned commit into target/.../out/ghostty-src.
    $fetched = Get-ChildItem -Path "$repo/target" -Recurse -Directory -Filter 'ghostty-src' -ErrorAction SilentlyContinue |
        Where-Object { Test-Path "$($_.FullName)/build.zig" } |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($fetched) { return $fetched.FullName }
    return $null
}

$ghosttyBenchExe = $null
if ($SkipGhostty) {
    Note "-SkipGhostty set; running giest side only."
} elseif ($null -ne $zigPrefix) {
    $src = Find-GhosttySource
    if (-not $src) {
        # Last resort: shallow-clone the pinned commit into a scratch dir.
        $src = "$repo/target/ghostty-bench-src"
        if (-not (Test-Path "$src/build.zig")) {
            Note "Cloning ghostty $GHOSTTY_COMMIT (one-time) ..."
            git clone --filter=blob:none --no-checkout $GHOSTTY_REPO $src
            git -C $src checkout $GHOSTTY_COMMIT
        }
    }
    Note "Ghostty source: $src"

    # Build the bench binaries (ghostty-gen + ghostty-bench, always ReleaseFast).
    $prefix = "$src/zig-out"
    Note "Building ghostty-bench (-Demit-bench) ... first run pulls Zig deps and takes a while."
    Push-Location $src
    try {
        $buildArgs = @($zigPrefix) + @('zig', 'build', '-Demit-bench', '-Doptimize=ReleaseFast', '--prefix', $prefix)
        & $buildArgs[0] @($buildArgs[1..($buildArgs.Count - 1)])
    } finally { Pop-Location }

    $cand = Join-Path $prefix 'bin/ghostty-bench.exe'
    if (-not (Test-Path $cand)) { $cand = Join-Path $prefix 'bin/ghostty-bench' }
    if (Test-Path $cand) { $ghosttyBenchExe = $cand }
    else { Warn "ghostty-bench not found under $prefix/bin after build." }
}

# --- Corpus ---------------------------------------------------------------------
if (-not (Test-Path $Corpus)) {
    Warn "Corpus not found: $Corpus"
    exit 1
}
$Corpus = (Resolve-Path $Corpus).Path
$sizeBytes = (Get-Item $Corpus).Length
$sizeMiB = [math]::Round($sizeBytes / 1MB, 3)
Note "Corpus: $Corpus ($sizeMiB MiB), grid ${Cols}x${Rows}"
if ($sizeBytes -lt 4MB) {
    Warn "Corpus is small (<4 MiB); throughput numbers will be noisy. Capture a larger stream (see benches/data/README.md)."
}

Write-Host ""
Write-Host "==================== RESULTS ====================" -ForegroundColor Green

# --- Ghostty side ---------------------------------------------------------------
if ($ghosttyBenchExe) {
    $benchArgs = @('terminal-stream', '--data', $Corpus, '--terminal-cols', $Cols, '--terminal-rows', $Rows)
    if (Get-Command hyperfine -ErrorAction SilentlyContinue) {
        Note "ghostty-bench (timed by hyperfine):"
        $line = "`"$ghosttyBenchExe`" " + ($benchArgs -join ' ')
        hyperfine --warmup 3 --runs $Runs -- $line
        # hyperfine prints its own table; derive MiB/s manually below if desired.
    } else {
        Note "hyperfine not found; timing $Runs runs with Measure-Command."
        $times = @()
        for ($i = 0; $i -lt $Runs; $i++) {
            $t = Measure-Command { & $ghosttyBenchExe @benchArgs | Out-Null }
            $times += $t.TotalSeconds
        }
        $median = ($times | Sort-Object)[[int]($times.Count / 2)]
        $mibps = [math]::Round($sizeMiB / $median, 1)
        Write-Host ("  ghostty terminal-stream : {0,8:N1} MiB/s  (median of {1} runs, {2:N4}s/run)" -f $mibps, $Runs, $median)
    }
} else {
    Warn "Ghostty side skipped (no ghostty-bench built)."
}

# --- giest side -----------------------------------------------------------------
if ($null -ne $zigPrefix -or (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Note "giest stream bench (criterion, corpus case):"
    $env:GIEST_BENCH_DATA = $Corpus
    try {
        $filter = "corpus/${Cols}x${Rows}"
        $pfx = if ($zigPrefix) { $zigPrefix } else { @() }
        $cargoArgs = @($pfx) + @('cargo', 'bench', '--bench', 'stream', '--', $filter, '--warm-up-time', '1', '--measurement-time', '3')
        $out = & $cargoArgs[0] @($cargoArgs[1..($cargoArgs.Count - 1)]) 2>&1
        # Surface criterion's absolute throughput line for the corpus case (skip
        # its change-vs-baseline percentage line).
        $out | Select-String -Pattern 'thrpt:' | Where-Object { $_ -notmatch '%' } | ForEach-Object {
            Write-Host ("  giest engine.write      : " + ($_.ToString().Trim()))
        }
    } finally {
        Remove-Item Env:\GIEST_BENCH_DATA -ErrorAction SilentlyContinue
    }
} else {
    Warn "giest side skipped (no cargo)."
}

Write-Host "=================================================" -ForegroundColor Green
Write-Host "Different harnesses — compare the ratio, on the same corpus. See docs/benchmarking.md." -ForegroundColor DarkGray
