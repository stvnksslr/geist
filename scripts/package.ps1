# Build the release artifacts for one version: a portable zip, an MSIX, the
# App Installer file, and giest-manifest.json (the self-updater's input: the
# SHA-256 of each package, see src/update.rs). Nothing is uploaded or signed
# unless asked.
#
#   mise package                                     # everything, unsigned
#   pwsh scripts/package.ps1 -SkipBuild              # reuse target\release
#   pwsh scripts/package.ps1 -CertPath c.pfx -CertPassword ... -Publisher "CN=..."
#
# Output: dist\<version>\
#   giest-<v>-windows-<arch>.zip    portable: exe + ConPTY + licenses
#   giest-<v>-windows-<arch>.msix   unsigned unless -CertPath is given
#   giest.appinstaller              for -BaseUri hosting (MSIX auto-update)
#   giest-manifest.json             attach to the GitHub release
#
# Reproducibility: every staged file's mtime is pinned to the HEAD commit's
# time, the file list is sorted, and the build is `cargo build --release`
# from the lockfile; the same commit and toolchain give the same zip bytes.
param(
    [string]$Version,
    [string]$Channel,
    [switch]$SkipBuild,
    [switch]$NoMsix,
    [string]$Publisher = "CN=giest-unsigned",
    [string]$PublisherDisplay = "giest",
    [string]$BaseUri = "https://github.com/stvnksslr/giest/releases/latest/download",
    [string]$CertPath,
    [string]$CertPassword
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $Version) {
    $Version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
}
if (-not $Channel) { $Channel = if ($Version -match '-') { "tip" } else { "stable" } }
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "arm64" } else { "x64" }
# MSIX needs a four-part numeric version; a pre-release keeps its core.
$core = ($Version -split '[-+]')[0]
$version4 = "$core.0"

if (-not $SkipBuild) {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}
& "$PSScriptRoot\fetch-conpty.ps1" -Profiles release | Out-Null
# The default-terminal handoff proxy/stub (MIDL + MSVC; see src/handoff.rs).
& "$PSScriptRoot\build-handoff-proxy.ps1" -Profiles release | Out-Null

$rel = Join-Path $root "target\release"
foreach ($f in "giest.exe", "conpty.dll", "OpenConsole.exe", "giestHandoffProxy.dll") {
    if (-not (Test-Path "$rel\$f")) { throw "missing $rel\$f" }
}

$out = Join-Path $root "dist\$Version"
$name = "giest-$Version-windows-$arch"
$stage = Join-Path $out $name
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force "$stage\licenses" | Out-Null

Copy-Item "$rel\giest.exe", "$rel\conpty.dll", "$rel\OpenConsole.exe", "$rel\giestHandoffProxy.dll" $stage
Copy-Item "assets\icon.ico" $stage
Copy-Item "assets\shell-integration\LICENSE-ghostty" "$stage\licenses\LICENSE-ghostty.txt"
Copy-Item "packaging\licenses\LICENSE-conpty.txt" "$stage\licenses\"
Copy-Item "assets\fonts\OFL.txt" "$stage\licenses\LICENSE-JetBrainsMono-OFL.txt"
Copy-Item "assets\ICON_LICENSE.txt" "$stage\licenses\LICENSE-icon.txt"
if (Test-Path "LICENSE") { Copy-Item "LICENSE" "$stage\licenses\LICENSE-giest.txt" }
@"
giest $Version ($Channel, $arch)

Run giest.exe. conpty.dll and OpenConsole.exe must stay beside it: they are
the out-of-band ConPTY that carries kitty graphics (set conpty-passthrough =
false to use the inbox console host instead). giestHandoffProxy.dll is what
lets giest be the Windows default terminal (giest +register-default-terminal).

Third-party licenses are in licenses\.
"@ | Set-Content -Encoding ascii "$stage\README.txt"

# Pin mtimes for reproducible archives.
$epoch = [DateTimeOffset]::FromUnixTimeSeconds([int64](git log -1 --format=%ct)).UtcDateTime
@(Get-Item $stage) + @(Get-ChildItem -Recurse $stage) | ForEach-Object {
    $_.LastWriteTimeUtc = $epoch; $_.LastAccessTimeUtc = $epoch; $_.CreationTimeUtc = $epoch
}

$zip = Join-Path $out "$name.zip"
# tar.exe (bsdtar) writes a zip with sorted, deterministic entries; the
# single top-level folder is what src/update.rs unwraps when staging.
Push-Location $out
tar.exe -a -c -f "$name.zip" $name
Pop-Location
if ($LASTEXITCODE -ne 0) { throw "zip failed" }

$files = @(@{ name = "$name.zip"; arch = $arch; kind = "zip"; path = $zip })

if (-not $NoMsix) {
    $kits = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\makeappx.exe" -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $kits) {
        Write-Warning "makeappx.exe not found (Windows SDK); skipping MSIX"
    } else {
        $msixDir = Join-Path $out "msix"
        Copy-Item -Recurse $stage $msixDir
        New-Item -ItemType Directory -Force "$msixDir\Assets" | Out-Null
        Add-Type -AssemblyName System.Drawing
        $src = [System.Drawing.Image]::FromFile((Resolve-Path "assets\icon.png"))
        foreach ($l in @(@("Square150x150Logo", 150), @("Square44x44Logo", 44), @("StoreLogo", 50))) {
            $bmp = New-Object System.Drawing.Bitmap $l[1], $l[1]
            $g = [System.Drawing.Graphics]::FromImage($bmp)
            $g.InterpolationMode = "HighQualityBicubic"
            $g.DrawImage($src, 0, 0, $l[1], $l[1])
            $bmp.Save("$msixDir\Assets\$($l[0]).png", [System.Drawing.Imaging.ImageFormat]::Png)
            $g.Dispose(); $bmp.Dispose()
        }
        $src.Dispose()
        $tokens = @{
            "@PUBLISHER@" = [System.Security.SecurityElement]::Escape($Publisher)
            "@PUBLISHER_DISPLAY@" = [System.Security.SecurityElement]::Escape($PublisherDisplay)
            "@VERSION4@" = $version4
            "@ARCH@" = $arch
            "@BASE_URI@" = $BaseUri.TrimEnd('/')
            "@MSIX_NAME@" = "$name.msix"
        }
        function Expand-Template($in, $outPath) {
            $t = Get-Content -Raw $in
            foreach ($k in $tokens.Keys) { $t = $t.Replace($k, $tokens[$k]) }
            [System.IO.File]::WriteAllText($outPath, $t, (New-Object System.Text.UTF8Encoding $false))
        }
        Expand-Template "packaging\AppxManifest.xml.in" "$msixDir\AppxManifest.xml"
        $msix = Join-Path $out "$name.msix"
        & $kits.FullName pack /o /h SHA256 /d $msixDir /p $msix | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "makeappx failed" }
        if ($CertPath) {
            $signtool = Join-Path $kits.DirectoryName "signtool.exe"
            & $signtool sign /fd SHA256 /f $CertPath /p $CertPassword $msix
            if ($LASTEXITCODE -ne 0) { throw "signtool failed" }
        } else {
            Write-Warning "MSIX is unsigned: Windows will not install it until it is signed with a certificate whose subject is '$Publisher'"
        }
        Expand-Template "packaging\giest.appinstaller.in" (Join-Path $out "giest.appinstaller")
        Remove-Item -Recurse -Force $msixDir
        $files += @{ name = "$name.msix"; arch = $arch; kind = "msix"; path = $msix }
    }
}

$manifest = [ordered]@{
    version = $Version
    channel = $Channel
    files = @($files | ForEach-Object {
        [ordered]@{
            name = $_.name; arch = $_.arch; kind = $_.kind
            size = (Get-Item $_.path).Length
            sha256 = (Get-FileHash -Algorithm SHA256 $_.path).Hash.ToLower()
        }
    })
}
$manifest | ConvertTo-Json -Depth 5 | Set-Content -Encoding utf8 (Join-Path $out "giest-manifest.json")
Remove-Item -Recurse -Force $stage
Get-ChildItem $out | Format-Table Name, Length
