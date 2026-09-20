# Cross-terminal comparison: run the SAME inner workload inside giest,
# Windows Terminal and WezTerm, and collect each terminal's results.
#
# The inner workload (`perf-vs-inner.ps1`, generated below) runs in a pwsh
# hosted by the terminal under test and writes JSON to -OutDir:
#   1. flood: `perf-flood.ps1` — write-side throughput of a fixed ANSI corpus
#   2. vtebench: Alacritty's cross-terminal suite (scrolling, alt-screen,
#      cursor motion, dense cells, unicode) — the number terminals quote
#      against each other. Needs `cargo install vtebench` and a checkout of
#      its `benchmarks/` directory.
# Each terminal is launched fresh, runs the workload, and exits. The machine
# should be otherwise idle. All three share ConPTY, so this compares the
# terminals' *own* costs on equal footing.
# Every terminal is pinned to a 120x40 grid: ConPTY's cost scales with the
# cell count, so unequal default sizes would measure window size, not the
# terminal. Note which console host each uses (WezTerm bundles OpenConsole;
# giest uses a sideloaded one only if conpty.dll sits beside the exe).
param(
    [string]$Giest = "$PSScriptRoot/../target/release/giest.exe",
    [string]$WezTerm = "",
    [string]$Corpus = "$PSScriptRoot/../target/audit/corpus.vt",
    [string]$VtebenchDir = "$PSScriptRoot/../target/audit/vtb",
    [string]$OutDir = "$PSScriptRoot/../perf/vs",
    [int]$FloodRepeat = 3,
    [string[]]$Only = @(),
    # Extra giest CLI args, e.g. '--conpty-passthrough=false' to measure the
    # inbox console host; the label gets -Suffix so both variants can coexist.
    [string[]]$GiestExtraArgs = @(),
    [string]$GiestLabelSuffix = ''
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path
$Corpus = (Resolve-Path $Corpus).Path
$VtebenchDir = (Resolve-Path $VtebenchDir).Path
$flood = (Resolve-Path "$PSScriptRoot/perf-flood.ps1").Path
$inner = Join-Path $OutDir 'perf-vs-inner.ps1'
@"
param([string]`$Label)
`$out = '$OutDir'
& '$flood' -Corpus '$Corpus' -Out (Join-Path `$out "`$Label-flood.json") -Repeat $FloodRepeat -Label `$Label
# vtebench workloads, pre-generated to byte streams (its runner needs /bin/sh):
# replay each with the same timer as the flood.
foreach (`$vt in Get-ChildItem '$VtebenchDir' -Filter *.vt) {
    & '$flood' -Corpus `$vt.FullName -Out (Join-Path `$out "`$Label-vtb-`$(`$vt.BaseName).json") -Repeat '$FloodRepeat' -Label `$Label
}
Set-Content (Join-Path `$out "`$Label-done") 'ok'
"@ | Set-Content $inner -Encoding utf8

# Bare `pwsh`, not its full path: `Start-Process -ArgumentList` below joins
# arguments with spaces WITHOUT quoting, so a path like
# `C:\Program Files\PowerShell\7\pwsh.exe` would reach the terminal as two
# arguments. (Not a giest `-e` limitation: a quoted full path works.) Every
# terminal resolves PATH anyway.
$pw = 'pwsh'
$terms = @(
    @{ label = "giest$GiestLabelSuffix"; exe = (Resolve-Path $Giest).Path; args = @('--window-width=120', '--window-height=40') + $GiestExtraArgs + @('-e', $pw, '-NoProfile', '-NoLogo', '-File', $inner, "giest$GiestLabelSuffix") }
    @{ label = 'wt';     exe = 'wt.exe'; args = @('-w', 'new', '--size', '120,40', '--', $pw, '-NoProfile', '-NoLogo', '-File', $inner, 'wt') }
)
if ($WezTerm) {
    $terms += @{ label = 'wezterm'; exe = (Resolve-Path $WezTerm).Path; args = @('--config', 'initial_cols=120', '--config', 'initial_rows=40', 'start', '--', $pw, '-NoProfile', '-NoLogo', '-File', $inner, 'wezterm') }
}
foreach ($t in $terms) {
    if ($Only.Count -and $Only -notcontains $t.label) { continue }
    $done = Join-Path $OutDir "$($t.label)-done"
    Remove-Item $done -ErrorAction SilentlyContinue
    Write-Host ">> $($t.label): $($t.exe) $($t.args -join ' ')"
    $env:GIEST_IPC_PIPE = "giest-perf-vs-$PID"
    $p = Start-Process -FilePath $t.exe -ArgumentList $t.args -PassThru
    $deadline = (Get-Date).AddMinutes(10)
    while (-not (Test-Path $done) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
    if (-not (Test-Path $done)) { Write-Warning "$($t.label): timed out" }
    # Let the terminal close itself (the shell exited); nudge stragglers.
    Start-Sleep 2
    foreach ($n in 'giest','WindowsTerminal','wezterm-gui') {
        Get-Process $n -ErrorAction SilentlyContinue | Where-Object { $_.StartTime -gt (Get-Date).AddMinutes(-11) } | Stop-Process -Force -ErrorAction SilentlyContinue
    }
}
Remove-Item Env:GIEST_IPC_PIPE -ErrorAction SilentlyContinue

# Summarize
$rows = foreach ($t in $terms) {
    $f = Join-Path $OutDir "$($t.label)-flood.json"
    $fl = if (Test-Path $f) { Get-Content $f | ConvertFrom-Json } else { $null }
    $vt = [ordered]@{}
    foreach ($j in Get-ChildItem $OutDir -Filter "$($t.label)-vtb-*.json" | Sort-Object Name) {
        $r = Get-Content $j.FullName | ConvertFrom-Json
        $vt[($j.BaseName -replace "^$($t.label)-vtb-", '')] = $r.mib_per_s
    }
    [pscustomobject]@{ terminal = $t.label; flood_mib_s = $fl.mib_per_s; flood_median_ms = $fl.median_ms; vtb_mib_s = $vt }
}
$rows | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $OutDir 'summary.json') -Encoding utf8
$rows | Format-Table terminal, flood_mib_s, flood_median_ms
$names = $rows[0].vtb_mib_s.Keys
Write-Host ("{0,-24}" -f 'vtebench stream (MiB/s)') -NoNewline; foreach ($r in $rows) { Write-Host ("{0,10}" -f $r.terminal) -NoNewline }; Write-Host
foreach ($n in $names) { Write-Host ("{0,-24}" -f $n) -NoNewline; foreach ($r in $rows) { Write-Host ("{0,10}" -f $r.vtb_mib_s[$n]) -NoNewline }; Write-Host }
