# Fetch the out-of-band ConPTY (conpty.dll + OpenConsole.exe) from Microsoft's
# NuGet package and place it next to giest.exe (and the test binaries).
#
# Why: the inbox conhost (10.0.26100 even on Windows 11 25H2) re-renders shell
# output and strips APC, which is what kitty graphics uses. The rewritten
# ConPTY shipped in this package (1.22+) forwards it. The vendored portable-pty
# loads a conpty.dll found beside the exe when `conpty-passthrough` is `auto`
# (the default) or `true`. Microsoft.Windows.Console.ConPTY is MIT-licensed
# (github.com/microsoft/terminal).
#
#   pwsh scripts/fetch-conpty.ps1                 # default version, debug+release
#   pwsh scripts/fetch-conpty.ps1 -Version 1.24.260710001
param(
    [string]$Version = "1.24.260710001",
    [string[]]$Profiles = @("debug", "release")
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
$work = Join-Path $env:TEMP "giest-conpty-$Version"
if (-not (Test-Path "$work\x")) {
    New-Item -ItemType Directory -Force $work | Out-Null
    $url = "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/$Version/microsoft.windows.console.conpty.$Version.nupkg"
    Invoke-WebRequest $url -OutFile "$work\pkg.zip"
    Expand-Archive -Force "$work\pkg.zip" "$work\x"
}
$dll = "$work\x\runtimes\win-$arch\native\conpty.dll"
$host_ = "$work\x\build\native\runtimes\$arch\OpenConsole.exe"
foreach ($p in $Profiles) {
    foreach ($dir in @("$root\target\$p", "$root\target\$p\deps")) {
        New-Item -ItemType Directory -Force $dir | Out-Null
        Copy-Item $dll, $host_ $dir -Force
        Write-Output "installed ConPTY $Version into $dir"
    }
}
