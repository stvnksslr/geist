# Cross-terminal flood workload: prints a fixed ANSI corpus to the terminal
# this script runs *inside* and records how long the terminal took to accept
# it. Run the same file inside giest, Windows Terminal, WezTerm, ... and
# compare `-Out` files. Wall-clock of the *write* side: a terminal that reads
# its PTY slowly makes the writer block, so this is end-to-end throughput as
# the program sees it (the same thing `ghostty-bench` and vtebench time).
param(
    [string]$Corpus = "$PSScriptRoot/../target/audit/corpus.vt",
    [string]$Out = "$env:TEMP/perf-flood-result.json",
    [int]$Repeat = 3,
    [string]$Label = "unknown"
)
$bytes = [IO.File]::ReadAllBytes((Resolve-Path $Corpus))
$stdout = [Console]::OpenStandardOutput()
$times = @()
for ($i = 0; $i -lt $Repeat; $i++) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $stdout.Write($bytes, 0, $bytes.Length)
    $stdout.Flush()
    $sw.Stop()
    $times += $sw.Elapsed.TotalMilliseconds
}
$sorted = $times | Sort-Object
$median = $sorted[[math]::Floor($sorted.Count / 2)]
$mib = $bytes.Length / 1MB
[pscustomobject]@{
    label      = $Label
    bytes      = $bytes.Length
    runs_ms    = $times
    median_ms  = [math]::Round($median, 1)
    mib_per_s  = [math]::Round($mib / ($median / 1000), 1)
} | ConvertTo-Json -Compress | Set-Content -Path $Out -Encoding utf8
