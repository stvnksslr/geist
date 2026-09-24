# Build geistHandoffProxy.dll: the COM proxy/stub for ITerminalHandoff3 and
# IConsoleHandoff, which default-terminal handoff needs (see GAP.md,
# "Default-terminal handoff").
#
# Why a native DLL: the handoff call crosses processes (OpenConsole.exe ->
# geist.exe -Embedding) and its parameters are `system_handle`s. Only a
# MIDL-generated NDR proxy marshals those; there is no typelib path. Windows
# Terminal ships the same thing as OpenConsoleProxy.dll, but registers it only
# in its *package* COM catalog, which an unpackaged process cannot see
# (`CoGetPSClsid` returns REGDB_E_IIDNOTREG for both IIDs here even with WT
# installed).
#
# Inputs: vendor/terminal-handoff/*.idl are verbatim copies of
# microsoft/terminal src/host/proxy/*.idl (MIT). Do not edit them: the wire
# format must match OpenConsole's. dlldata.c/proxy.def are ours.
#
# Needs Visual Studio (C++ workload) + the Windows SDK (midl.exe). Output goes
# beside geist.exe for each profile, like scripts/fetch-conpty.ps1.
#
#   pwsh scripts/build-handoff-proxy.ps1
param([string[]]$Profiles = @("debug", "release"))
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$src = "$root\vendor\terminal-handoff"
$out = "$root\target\handoff-proxy"
New-Item -ItemType Directory -Force $out | Out-Null

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw "Visual Studio with the C++ tools was not found" }
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
$vcvars = "$vs\VC\Auxiliary\Build\vcvarsall.bat"

# One cmd session: vcvars sets PATH/INCLUDE/LIB for midl and cl.
$cmds = @(
    "`"$vcvars`" $arch >nul",
    "cd /d `"$out`"",
    "midl /nologo /target NT100 /env $(if ($arch -eq 'x64') {'x64'} else {'arm64'}) /h ITerminalHandoff.h /proxy ITerminalHandoff_p.c /iid ITerminalHandoff_i.c /dlldata nul_1.c `"$src\ITerminalHandoff.idl`"",
    "midl /nologo /target NT100 /env $(if ($arch -eq 'x64') {'x64'} else {'arm64'}) /h IConsoleHandoff.h /proxy IConsoleHandoff_p.c /iid IConsoleHandoff_i.c /dlldata nul_2.c `"$src\IConsoleHandoff.idl`"",
    "cl /nologo /O2 /LD /MT /DREGISTER_PROXY_DLL /DWIN32_LEAN_AND_MEAN /I. ITerminalHandoff_p.c ITerminalHandoff_i.c IConsoleHandoff_p.c IConsoleHandoff_i.c `"$src\dlldata.c`" /Fe:geistHandoffProxy.dll /link /DEF:`"$src\proxy.def`" rpcrt4.lib oleaut32.lib ole32.lib"
) -join " && "
cmd /c $cmds
if ($LASTEXITCODE -ne 0) { throw "proxy build failed ($LASTEXITCODE)" }

foreach ($p in $Profiles) {
    foreach ($dir in @("$root\target\$p", "$root\target\$p\deps")) {
        New-Item -ItemType Directory -Force $dir | Out-Null
        Copy-Item "$out\geistHandoffProxy.dll" $dir -Force
        Write-Output "installed geistHandoffProxy.dll into $dir"
    }
}
