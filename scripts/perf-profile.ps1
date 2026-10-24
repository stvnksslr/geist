# Profile geist under a workload: launch it (isolated config + IPC pipe, so it
# never talks to a geist you have open), run scripts/perf-workload.py inside
# it, and point tools/sampler at the process while the workload runs.
#
#   $env:LIBGHOSTTY_VT_SYS_OPTIMIZE = "ReleaseFast"   # see docs/benchmarking.md
#   cargo build --profile profiling
#   pwsh scripts/fetch-conpty.ps1 -Profiles profiling
#   cargo build --release --manifest-path tools/sampler/Cargo.toml
#   pwsh scripts/perf-profile.ps1 -Mode tui
#   pwsh scripts/perf-profile.ps1 -Mode flood -Cols 200 -Rows 60
#
# Writes <OutDir>/<label>-<mode>.{json,txt,folded}: the workload's own result
# (MiB/s, frames/s), the sampler's report, and collapsed stacks for a
# flamegraph. Pass -NoSample for a clean throughput number (sampling suspends
# the UI thread, and with it the whole PTY pipeline: measured ~20x slower).
param(
    [ValidateSet("tui", "flood")][string]$Mode = "tui",
    [string]$Exe = "$PSScriptRoot/../target/profiling/geist.exe",
    [int]$Secs = 12,
    [int]$Cols = 120,
    [int]$Rows = 40,
    [string]$Label = "geist",
    [string]$OutDir = "$PSScriptRoot/../target/perf-profile",
    [switch]$NoSample,
    # Per-thread CPU from cycle counters only: no thread is ever suspended, so
    # the workload runs at full speed. Stack sampling slows a TUI ~20x here
    # (it stalls the UI thread that drains the PTY), so read throughput only
    # from -NoSample or -CpuOnly runs.
    [switch]$CpuOnly,
    # Replace the sampler's default arguments, e.g. to look at where the UI
    # thread *waits*, gently: -SamplerArgs --only,main,--waits,--interval-ms,20
    [string[]]$SamplerArgs
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$Exe = (Resolve-Path $Exe).Path
$sampler = "$root/tools/sampler/target/release/sampler.exe"
New-Item -ItemType Directory -Force $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path
$base = Join-Path $OutDir "$Label-$Mode"
Remove-Item "$base.json", "$base.txt", "$base.folded" -ErrorAction SilentlyContinue

$cfg = Join-Path $OutDir "config"
Set-Content $cfg "jump-list = false`nconfirm-close-surface = false`nauto-update = off`n"
$env:geist_CONFIG = $cfg
$env:geist_IPC_PIPE = "geist-perf-$PID-$(Get-Random)"

$py = (Get-Command python).Source
$wl = "$root/scripts/perf-workload.py"
$p = Start-Process -PassThru -FilePath $Exe -ArgumentList @(
    "--window-width=$Cols", "--window-height=$Rows", "-e", $py, $wl, $Mode,
    "--secs", "$Secs", "--out", "$base.json")
try {
    # Let the window come up and the workload reach a steady state.
    Start-Sleep -Seconds 2
    # Whether geist is the foreground window decides how often Windows lets it
    # present, and so (since the UI thread is what drains the PTY) how fast the
    # workload runs; record it next to every number.
    Add-Type -Namespace PerfProbe -Name U -MemberDefinition @'
[DllImport("user32.dll")] public static extern System.IntPtr GetForegroundWindow();
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(System.IntPtr h, out uint pid);
'@ -ErrorAction SilentlyContinue
    $fgPid = 0
    [void][PerfProbe.U]::GetWindowThreadProcessId([PerfProbe.U]::GetForegroundWindow(), [ref]$fgPid)
    $foreground = $fgPid -eq $p.Id
    if (-not $NoSample) {
        # The console host is half of every number here (ConPTY re-renders the
        # child's output before geist sees it), so sample it too.
        $conhost = Get-CimInstance Win32_Process -Filter "ParentProcessId = $($p.Id)" |
            Where-Object { $_.Name -in "OpenConsole.exe", "conhost.exe" } | Select-Object -First 1
        $side = $null
        if ($conhost) {
            $sideArgs = @("$($conhost.ProcessId)", "--secs", "$($Secs - 4)", "--top", "30")
            if ($CpuOnly) { $sideArgs += "--cpu-only" }
            $side = Start-Process -PassThru -NoNewWindow -FilePath $sampler -ArgumentList $sideArgs `
                -RedirectStandardOutput "$base-$($conhost.Name).txt" -RedirectStandardError "$base-sampler.err"
        }
        [string[]]$mainArgs = if ($SamplerArgs) { $SamplerArgs } elseif ($CpuOnly) { @("--cpu-only") } else { @("--top", "45", "--folded", "$base.folded") }
        & $sampler $p.Id --secs ($Secs - 4) @mainArgs | Set-Content "$base.txt"
        if ($side) { $side.WaitForExit() }
    }
    if (-not $p.WaitForExit(($Secs + 20) * 1000)) {
        # Record where every thread is stuck before the kill below destroys it.
        & $sampler $p.Id --secs 2 --waits --interval-ms 50 --top 15 --folded "$base-hang.folded" |
            Set-Content "$base-hang.txt"
        throw "geist did not exit (thread stacks: $base-hang.txt)"
    }
} finally {
    if (-not $p.HasExited) { $p.Kill() }
}
if (-not (Test-Path "$base.json")) { throw "workload wrote no result (did geist start the command?)" }
$res = Get-Content "$base.json" | ConvertFrom-Json
$res | Add-Member foreground $foreground
$res | Add-Member label $Label
$res | ConvertTo-Json -Compress | Tee-Object -FilePath "$base.json"
