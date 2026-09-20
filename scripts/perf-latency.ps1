# Keystroke-to-pixel latency, measured from OUTSIDE the terminal, so it can be
# pointed at any of them (giest, WezTerm, Windows Terminal) on equal terms.
#
# Method (a camera-free typometer): focus the window, snapshot its client area
# with PrintWindow, send one key with SendInput, then re-snapshot in a tight
# loop until the pixels change. The delta is what a user sees: input stack +
# ConPTY + shell echo + the terminal's own read/render/present.
#
# Two traps, both from CLAUDE.md, both silently wrong if ignored:
#   - the probing process must be per-monitor DPI aware, or Windows virtualises
#     GetClientRect/PrintWindow and every coordinate is off by the scale factor;
#   - PrintWindow needs PW_CLIENTONLY|PW_RENDERFULLCONTENT (3): with 2 alone it
#     renders the whole window, shifting client coordinates by the frame.
#
# What it does NOT measure: the compositor's scan-out. PrintWindow sees the
# window's own surface, so this is "terminal has drawn it", a lower bound on
# what a camera would show. That bias is identical for every terminal, which is
# the point - the comparison is fair even though the absolute number is optimistic.
#
# !! INVOKE IT IN YOUR SHELL, NOT AS `pwsh -File` !!
#
#   & ./scripts/perf-latency.ps1 -Exe target/release/giest.exe -TermArgs @('--window-width=120','--window-height=40')
#
# Run as a child `pwsh -File ...`, every terminal it launches dies within ~10 ms
# and the probe reports "no window" as though the terminal had crashed. Measured
# both ways on 2026-09-20; in-process works, child-process does not. Also note
# `-TermArgs a,b` collapses into one comma-joined string when crossing `-File`
# (handled below, but the `@(...)` form above avoids it entirely).
#
# The display must be awake and the session interactive. With the monitor
# asleep a screen-DC grab returns black, and in a context with no desktop
# `SendInput` delivers nothing at all - the preflights below turn both into a
# loud failure instead of a plausible-looking zero.
param(
    [Parameter(Mandatory)][string]$Exe,
    [string[]]$TermArgs = @(),
    [string]$ProcName = "",
    [int]$Samples = 30,
    [int]$WarmupMs = 6000,
    # Every terminal is resized to the same client pixels before timing. The
    # poll loop's PrintWindow cost scales with window area and is part of the
    # measured interval, so comparing terminals at their own default sizes
    # measures the window, not the terminal: WezTerm's default is 1.6x giest's
    # area here, which alone accounted for ~8 ms of its first result.
    [int]$ClientW = 1200,
    [int]$ClientH = 800,
    [string]$Label = "",
    [string]$OutDir = "$PSScriptRoot/../perf/latency"
)
$ErrorActionPreference = 'Stop'
# `pwsh -File x.ps1 -TermArgs 'a','b'` hands the script ONE comma-joined string,
# so split it back. Getting this wrong is silent and brutal: the terminal is
# launched with one bogus argument, exits instantly, and the probe reports
# "no window" as though the terminal were broken.
if ($TermArgs.Count -eq 1 -and $TermArgs[0] -match ',') { $TermArgs = $TermArgs[0] -split ',' }
if (-not $Label) { $Label = [IO.Path]::GetFileNameWithoutExtension($Exe) }
if (-not $ProcName) { $ProcName = [IO.Path]::GetFileNameWithoutExtension($Exe) }
New-Item -ItemType Directory -Force $OutDir | Out-Null

# Raw GDI, not System.Drawing: System.Drawing.Common is not part of PowerShell 7,
# and a DIB section lets the poll loop read pixels straight from a pointer with
# no per-sample allocation.
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class Probe {
    [DllImport("user32.dll")] public static extern IntPtr GetDC(IntPtr h);
    [DllImport("user32.dll")] public static extern int ReleaseDC(IntPtr h, IntPtr dc);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] i, int cb);
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("gdi32.dll")] public static extern IntPtr CreateCompatibleDC(IntPtr dc);
    [DllImport("gdi32.dll")] public static extern bool DeleteDC(IntPtr dc);
    [DllImport("gdi32.dll")] public static extern bool DeleteObject(IntPtr o);
    [DllImport("gdi32.dll")] public static extern IntPtr SelectObject(IntPtr dc, IntPtr o);
    [DllImport("gdi32.dll")] public static extern IntPtr CreateDIBSection(IntPtr dc, ref BITMAPINFO bmi, uint usage, out IntPtr bits, IntPtr section, uint offset);

    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [StructLayout(LayoutKind.Sequential)] public struct BITMAPINFOHEADER {
        public uint biSize; public int biWidth, biHeight; public ushort biPlanes, biBitCount;
        public uint biCompression, biSizeImage; public int biXPelsPerMeter, biYPelsPerMeter;
        public uint biClrUsed, biClrImportant;
    }
    [StructLayout(LayoutKind.Sequential)] public struct BITMAPINFO { public BITMAPINFOHEADER h; public uint quad; }
    [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort vk, scan; public uint flags, time; public IntPtr extra; }
    [StructLayout(LayoutKind.Explicit, Size = 40)] public struct INPUT {
        [FieldOffset(0)] public uint type;
        [FieldOffset(8)] public KEYBDINPUT ki;
    }

    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc p, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint from, uint to, bool attach);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int ht, uint flags);

    /// The process's largest visible top-level window.
    ///
    /// **Not** `Process.MainWindowHandle`: winit creates a 22x22 "Winit Thread
    /// Event Target" helper window that exists *before* the real one, so polling
    /// MainWindowHandle right after launch latches onto that. Every capture then
    /// returns 22*22*4 = 1936 bytes of nothing and every synthetic key goes to a
    /// window with no terminal in it - which is exactly the false "PrintWindow
    /// cannot see this surface" result this probe first produced.
    public static IntPtr RealWindow(uint want) {
        IntPtr best = IntPtr.Zero; int area = 0;
        EnumWindows((h, l) => {
            uint pid; GetWindowThreadProcessId(h, out pid);
            if (pid != want || !IsWindowVisible(h)) return true;
            RECT r; if (!GetClientRect(h, out r)) return true;
            int a = (r.R - r.L) * (r.B - r.T);
            if (a > area) { area = a; best = h; }
            return true;
        }, IntPtr.Zero);
        return best;
    }
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, IntPtr pid);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr SetFocus(IntPtr h);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();

    public static void Dpi() { SetProcessDpiAwarenessContext(new IntPtr(-4)); }

    /// Windows refuses `SetForegroundWindow` from a process that isn't already
    /// foreground, so borrow the target's input queue for the call - the
    /// standard AttachThreadInput dance. Without this the synthetic keys land
    /// in whatever window *is* focused (silently: the probe just sees no pixels
    /// change) and, worse, could be typed into something else.
    public static bool Focus(IntPtr hwnd) {
        uint me = GetCurrentThreadId();
        uint fg = GetWindowThreadProcessId(GetForegroundWindow(), IntPtr.Zero);
        uint target = GetWindowThreadProcessId(hwnd, IntPtr.Zero);
        if (fg != me) AttachThreadInput(me, fg, true);
        if (target != me) AttachThreadInput(me, target, true);
        BringWindowToTop(hwnd);
        bool ok = SetForegroundWindow(hwnd);
        SetFocus(hwnd);
        if (target != me) AttachThreadInput(me, target, false);
        if (fg != me) AttachThreadInput(me, fg, false);
        return ok && GetForegroundWindow() == hwnd;
    }

    /// Non-zero bytes in the last capture: a DirectComposition surface that
    /// PrintWindow cannot reach comes back all zeros, which would otherwise
    /// look like "the terminal never drew anything".
    public static long NonZero() {
        long n = 0;
        unsafe { byte* p = (byte*)bits; for (int i = 0; i < cw * ch * 4; i++) if (p[i] != 0) n++; }
        return n;
    }

    public static void Key(ushort vk) {
        INPUT[] a = new INPUT[2];
        a[0].type = 1; a[0].ki.vk = vk;
        a[1].type = 1; a[1].ki.vk = vk; a[1].ki.flags = 2; // KEYEVENTF_KEYUP
        SendInput(2, a, Marshal.SizeOf(typeof(INPUT)));
    }

    static IntPtr memDc, dib, bits;
    static int cw, ch;

    // Create (once) a top-down 32bpp DIB matching the window's client area.
    public static bool Setup(IntPtr hwnd) {
        RECT r; if (!GetClientRect(hwnd, out r)) return false;
        cw = r.R - r.L; ch = r.B - r.T;
        if (cw <= 0 || ch <= 0) return false;
        IntPtr screen = GetDC(IntPtr.Zero);
        memDc = CreateCompatibleDC(screen);
        BITMAPINFO bmi = new BITMAPINFO();
        bmi.h.biSize = (uint)Marshal.SizeOf(typeof(BITMAPINFOHEADER));
        bmi.h.biWidth = cw; bmi.h.biHeight = -ch;   // negative => top-down
        bmi.h.biPlanes = 1; bmi.h.biBitCount = 32; bmi.h.biCompression = 0;
        dib = CreateDIBSection(memDc, ref bmi, 0, out bits, IntPtr.Zero, 0);
        ReleaseDC(IntPtr.Zero, screen);
        if (dib == IntPtr.Zero) return false;
        SelectObject(memDc, dib);
        return true;
    }

    public static void Teardown() {
        if (dib != IntPtr.Zero) { DeleteObject(dib); dib = IntPtr.Zero; }
        if (memDc != IntPtr.Zero) { DeleteDC(memDc); memDc = IntPtr.Zero; }
    }

    // FNV-1a over every 4th byte of the client area: cheap, and any glyph
    // appearing anywhere changes it.
    public static long Sample(IntPtr hwnd) {
        if (memDc == IntPtr.Zero) return -1;
        if (!PrintWindow(hwnd, memDc, 3)) return -1;  // PW_CLIENTONLY|PW_RENDERFULLCONTENT
        long acc = 1469598103934665603L;
        unsafe {
            byte* p = (byte*)bits;
            int n = cw * ch * 4;
            for (int i = 0; i < n; i += 4) { acc = (acc ^ p[i]) * 1099511628211L; }
        }
        return acc;
    }
}
'@ -CompilerOptions '/unsafe'

[Probe]::Dpi()
# Isolate the instance under test. Without its own pipe, a giest launched while
# any other giest is reachable on the default pipe forwards its request to that
# one and exits in ~10 ms - the probe then reports "no window" and looks like a
# giest crash. Harmless for other terminals, which ignore the variable.
$env:GIEST_IPC_PIPE = "giest-perf-latency-$PID"
$p = if ($TermArgs.Count) { Start-Process -PassThru $Exe -ArgumentList $TermArgs } else { Start-Process -PassThru $Exe }
try {
    # Wait for a window big enough to be the terminal, not a helper (see RealWindow).
    # Searched across every process of this name, not just the one we launched:
    # WezTerm's launcher hands off to another process, so the window belongs to a
    # different PID than Start-Process returned.
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $hwnd = [IntPtr]::Zero
    while ($sw.ElapsedMilliseconds -lt 20000) {
        # No StartTime filter: it is unreadable for some processes and WezTerm's
        # launcher exits and re-execs, so the surviving process can predate the
        # launch we just made. The teardown kills by name anyway.
        foreach ($proc in @(Get-Process $ProcName -ErrorAction SilentlyContinue)) {
            $h = [Probe]::RealWindow([uint]$proc.Id)
            if ($h -ne [IntPtr]::Zero) {
                $r = New-Object Probe+RECT
                if ([Probe]::GetClientRect($h, [ref]$r) -and ($r.R - $r.L) -gt 200 -and ($r.B - $r.T) -gt 200) { $hwnd = $h; break }
            }
        }
        if ($hwnd -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 20
    }
    if ($hwnd -eq [IntPtr]::Zero) {
        $seen = @(Get-Process $ProcName -ErrorAction SilentlyContinue | ForEach-Object { "pid=$($_.Id)" }) -join ' '
        throw "${Label}: no window bigger than 200x200 found for process name '${ProcName}' (${seen})"
    }
    # normalize size before anything is timed
    [void][Probe]::SetWindowPos($hwnd, [IntPtr]::Zero, 100, 100, $ClientW, $ClientH, 0x0014)
    Start-Sleep -Milliseconds $WarmupMs          # shell prompt settled
    if (-not [Probe]::Focus($hwnd)) { throw "${Label}: could not bring the window to the foreground" }
    Start-Sleep -Milliseconds 500
    if (-not [Probe]::Setup($hwnd)) { throw "${Label}: cannot capture the client area" }
    if ([Probe]::Sample($hwnd) -eq -1) { throw "${Label}: PrintWindow failed" }
    # Preflight 1: is there anything on screen at all? A GPU-composited window
    # captured from a session without a desktop yields a few stray bytes.
    $nz = [Probe]::NonZero()
    if ($nz -lt 10000) {
        throw "${Label}: capture is essentially blank ($nz non-zero bytes). Run this from an interactive desktop session, not an automation shell."
    }
    # Preflight 2: does synthetic input actually reach the window? Type a key,
    # wait generously, and require the pixels to move before timing anything.
    $probe0 = [Probe]::Sample($hwnd)
    [Probe]::Key(0x58); Start-Sleep -Milliseconds 700
    if ([Probe]::Sample($hwnd) -eq $probe0) {
        throw "${Label}: SendInput is not reaching the window (no pixels changed). Run this from an interactive desktop session, not an automation shell."
    }
    [Probe]::Key(0x08); Start-Sleep -Milliseconds 300

    $lat = @()
    for ($i = 0; $i -lt $Samples; $i++) {
        if ([Probe]::GetForegroundWindow() -ne $hwnd) { [void][Probe]::Focus($hwnd); Start-Sleep -Milliseconds 300 }
        $before = [Probe]::Sample($hwnd)
        if ($before -eq -1) { continue }
        $t = [Diagnostics.Stopwatch]::StartNew()
        [Probe]::Key(0x58)                        # 'X'
        $ms = -1
        while ($t.Elapsed.TotalMilliseconds -lt 1000) {
            if ([Probe]::Sample($hwnd) -ne $before) { $ms = $t.Elapsed.TotalMilliseconds; break }
        }
        if ($ms -ge 0) { $lat += $ms }
        Start-Sleep -Milliseconds 120
        [Probe]::Key(0x08)                        # backspace, keep the line short
        Start-Sleep -Milliseconds 180
    }
} finally {
    [Probe]::Teardown()
    Get-Process $ProcName -ErrorAction SilentlyContinue |
        Where-Object { $_.StartTime -gt (Get-Date).AddMinutes(-5) } |
        Stop-Process -Force -ErrorAction SilentlyContinue
}

if (-not $lat.Count) { throw "${Label}: no samples (window never changed - is a shell running in it?)" }
$s = $lat | Sort-Object
function Pct($a, $q) { $a[[math]::Min($a.Count - 1, [math]::Floor($a.Count * $q))] }
$res = [pscustomobject]@{
    label   = $Label
    samples = $lat.Count
    p50_ms  = [math]::Round((Pct $s 0.50), 1)
    p95_ms  = [math]::Round((Pct $s 0.95), 1)
    min_ms  = [math]::Round($s[0], 1)
    max_ms  = [math]::Round($s[-1], 1)
    mean_ms = [math]::Round(($lat | Measure-Object -Average).Average, 1)
}
$res | ConvertTo-Json -Compress | Set-Content (Join-Path $OutDir "$Label.json") -Encoding utf8
$res
