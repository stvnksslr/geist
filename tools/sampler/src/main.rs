//! An out-of-process sampling profiler for geist on Windows, no admin needed.
//!
//! ETW-based profilers (WPR, samply) need an elevated shell. This one only
//! needs what any same-user process has: it suspends each thread of the target
//! every `--interval-ms`, copies its registers, walks the stack with dbghelp,
//! and resumes it. A thread is sampled only if its cycle counter advanced since
//! the last look (`QueryThreadCycleTime`), so the profile is of *CPU*, not of
//! time spent blocked in a wait.
//!
//! Symbols come from the PDB next to the exe: profile a `cargo build --profile
//! profiling` build (release codegen + debug info). Inlined frames are expanded
//! (`SymQueryInlineTrace`), which matters under fat LTO — most of geist's
//! render loop is inlined into a handful of physical functions.
//!
//! ```text
//! sampler <pid> [--secs 10] [--interval-ms 1] [--top 40] [--folded out.txt]
//! ```
//!
//! Prints, per busy thread, the top functions by *self* and *inclusive*
//! samples. `--folded` also writes collapsed stacks (`a;b;c N`), the input
//! format of inferno/flamegraph.pl.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, LocalFree, TRUE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    ADDRESS64, AddrModeFlat, CONTEXT, GetThreadContext, STACKFRAME64, SYMBOL_INFOW, StackWalk64,
    SymAddrIncludeInlineTrace, SymCleanup, SymFromAddrW, SymFromInlineContextW,
    SymFunctionTableAccess64, SymGetModuleBase64, SymInitializeW, SymQueryInlineTrace,
    SymSetOptions,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Threading::{
    GetThreadDescription, OpenProcess, OpenThread, PROCESS_NAME_WIN32, PROCESS_QUERY_INFORMATION,
    PROCESS_VM_READ, QueryFullProcessImageNameW, ResumeThread, SuspendThread, THREAD_GET_CONTEXT,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
};
use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

const IMAGE_FILE_MACHINE_AMD64: u32 = 0x8664;
const CONTEXT_FULL_AMD64: u32 = 0x0010_000B;
const SYMOPT_UNDNAME: u32 = 0x2;
const SYMOPT_DEFERRED_LOADS: u32 = 0x4;
const SYMOPT_LOAD_LINES: u32 = 0x10;
const MAX_FRAMES: usize = 96;

struct Args {
    pid: u32,
    secs: f64,
    interval: Duration,
    top: usize,
    folded: Option<String>,
    cpu_only: bool,
    only: Option<String>,
    waits: bool,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        pid: 0,
        secs: 10.0,
        interval: Duration::from_millis(1),
        top: 40,
        folded: None,
        cpu_only: false,
        only: None,
        waits: false,
    };
    while let Some(s) = it.next() {
        match s.as_str() {
            "--secs" => a.secs = it.next().and_then(|v| v.parse().ok()).expect("--secs N"),
            "--interval-ms" => {
                let ms: f64 = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--interval-ms N");
                a.interval = Duration::from_secs_f64(ms / 1000.0);
            }
            "--top" => a.top = it.next().and_then(|v| v.parse().ok()).expect("--top N"),
            "--folded" => a.folded = it.next(),
            "--cpu-only" => a.cpu_only = true,
            // Sample only threads with this name: suspending one thread
            // perturbs far less than suspending all of them.
            "--only" => a.only = it.next(),
            // Keep samples that caught a thread blocked: answers "what is it
            // waiting on?" when nothing is CPU-bound.
            "--waits" => a.waits = true,
            p => {
                a.pid = p.parse().expect(
                    "usage: sampler <pid> [--secs N] [--interval-ms N] [--top N] [--folded FILE]",
                )
            }
        }
    }
    assert!(
        a.pid != 0,
        "usage: sampler <pid> [--secs N] [--interval-ms N] [--top N] [--folded FILE]"
    );
    a
}

struct Thread {
    tid: u32,
    handle: HANDLE,
    name: String,
    last_cycles: u64,
    cycles: u64,
    stacks: Vec<Vec<u64>>,
}

fn threads_of(pid: u32) -> Vec<(u32, HANDLE)> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        let mut te: THREADENTRY32 = std::mem::zeroed();
        te.dwSize = size_of::<THREADENTRY32>() as u32;
        if Thread32First(snap, &mut te) != 0 {
            loop {
                if te.th32OwnerProcessID == pid {
                    let h = OpenThread(
                        THREAD_SUSPEND_RESUME
                            | THREAD_GET_CONTEXT
                            | THREAD_QUERY_LIMITED_INFORMATION,
                        FALSE,
                        te.th32ThreadID,
                    );
                    if !h.is_null() {
                        out.push((te.th32ThreadID, h));
                    }
                }
                if Thread32Next(snap, &mut te) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

fn thread_name(h: HANDLE) -> String {
    unsafe {
        let mut p: *mut u16 = std::ptr::null_mut();
        if GetThreadDescription(h, &mut p) >= 0 && !p.is_null() {
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            LocalFree(p as *mut c_void);
            return s;
        }
    }
    String::new()
}

fn cycles(h: HANDLE) -> u64 {
    let mut c = 0u64;
    unsafe { QueryThreadCycleTime(h, &mut c) };
    c
}

fn walk(proc_: HANDLE, th: HANDLE) -> Option<Vec<u64>> {
    unsafe {
        if SuspendThread(th) == u32::MAX {
            return None;
        }
        let mut ctx: CONTEXT = std::mem::zeroed();
        ctx.ContextFlags = CONTEXT_FULL_AMD64;
        let mut frames = Vec::with_capacity(32);
        if GetThreadContext(th, &mut ctx) != 0 {
            let mut sf: STACKFRAME64 = std::mem::zeroed();
            let flat = |off: u64| ADDRESS64 {
                Offset: off,
                Segment: 0,
                Mode: AddrModeFlat,
            };
            sf.AddrPC = flat(ctx.Rip);
            sf.AddrFrame = flat(ctx.Rbp);
            sf.AddrStack = flat(ctx.Rsp);
            while frames.len() < MAX_FRAMES
                && StackWalk64(
                    IMAGE_FILE_MACHINE_AMD64,
                    proc_,
                    th,
                    &mut sf,
                    (&mut ctx as *mut CONTEXT).cast(),
                    None,
                    Some(SymFunctionTableAccess64),
                    Some(SymGetModuleBase64),
                    None,
                ) != 0
            {
                if sf.AddrPC.Offset == 0 {
                    break;
                }
                frames.push(sf.AddrPC.Offset);
            }
        }
        ResumeThread(th);
        (!frames.is_empty()).then_some(frames)
    }
}

fn exe_dir(proc_: HANDLE) -> String {
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    unsafe {
        if QueryFullProcessImageNameW(proc_, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n) == 0 {
            return String::new();
        }
    }
    let p = String::from_utf16_lossy(&buf[..n as usize]);
    match p.rfind('\\') {
        Some(i) => p[..i].to_string(),
        None => p,
    }
}

/// Kernel entry points a thread sits in while *blocked*. A sample whose
/// innermost frame is one of these caught the thread after it ran and went
/// back to sleep within the tick, so it says nothing about where CPU went.
fn is_wait(f: &str) -> bool {
    f.starts_with("NtWaitFor")
        || f.starts_with("ZwWaitFor")
        || f.starts_with("NtUserMsgWaitFor")
        || f.starts_with("NtRemoveIoCompletion")
        || f.starts_with("ZwRemoveIoCompletion")
        || f == "NtDelayExecution"
        || f == "NtReadFile"
        || f == "NtUserGetMessage"
        || f == "NtUserWaitMessage"
        || f.starts_with("NtGdiDdDDIWaitFor")
}

/// Reference cycles per second as `QueryThreadCycleTime` counts them, by
/// spinning this thread for 200 ms.
fn cycles_per_sec() -> f64 {
    unsafe {
        let me = windows_sys::Win32::System::Threading::GetCurrentThread();
        let (c0, t0) = (cycles(me), Instant::now());
        while t0.elapsed() < Duration::from_millis(200) {
            std::hint::spin_loop();
        }
        (cycles(me) - c0) as f64 / t0.elapsed().as_secs_f64()
    }
}

/// `--cpu-only`: per-thread CPU use over the window, read from cycle
/// counters alone. Nothing is suspended, so unlike stack sampling this does
/// not perturb the pipeline it measures (a suspended UI thread is a PTY that
/// is not being drained).
fn cpu_only(threads: &mut [Thread], secs: f64) {
    let hz = cycles_per_sec();
    for t in threads.iter_mut() {
        t.last_cycles = cycles(t.handle);
    }
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(secs));
    let wall = start.elapsed().as_secs_f64();
    for t in threads.iter_mut() {
        t.cycles = cycles(t.handle) - t.last_cycles;
    }
    threads.sort_by_key(|t| std::cmp::Reverse(t.cycles));
    for t in threads.iter().take(8) {
        println!(
            "{:6.1}% of a core  thread {} {:?}",
            t.cycles as f64 / hz / wall * 100.0,
            t.tid,
            t.name
        );
    }
}

/// `SYMBOL_INFOW` followed by room for the name.
#[repr(C)]
struct SymBuf {
    info: SYMBOL_INFOW,
    _name: [u16; 512],
}

fn sym_name(info: &SYMBOL_INFOW) -> String {
    let n = info.NameLen.min(511) as usize;
    let s = unsafe { std::slice::from_raw_parts(info.Name.as_ptr(), n) };
    String::from_utf16_lossy(s)
}

/// Names for the frames at `addr`, innermost (most deeply inlined) first,
/// ending with the physical function.
fn resolve(proc_: HANDLE, addr: u64) -> Vec<String> {
    unsafe {
        let mut out = Vec::new();
        // A return address points just past the call; step back into it so a
        // call that is the last instruction of an inlined body resolves there.
        let a = addr.saturating_sub(1);
        let mut buf: SymBuf = std::mem::zeroed();
        buf.info.SizeOfStruct = size_of::<SYMBOL_INFOW>() as u32;
        buf.info.MaxNameLen = 511;
        let n = SymAddrIncludeInlineTrace(proc_, a);
        if n > 0 {
            let (mut ctx, mut idx) = (0u32, 0u32);
            if SymQueryInlineTrace(proc_, a, 0, a, a, &mut ctx, &mut idx) != 0 {
                for i in 0..n {
                    let mut disp = 0u64;
                    if SymFromInlineContextW(proc_, a, ctx + i, &mut disp, &mut buf.info) != 0 {
                        out.push(sym_name(&buf.info));
                    }
                }
            }
        }
        let mut disp = 0u64;
        if SymFromAddrW(proc_, a, &mut disp, &mut buf.info) != 0 {
            out.push(sym_name(&buf.info));
        } else {
            out.push(format!("0x{addr:x}"));
        }
        out
    }
}

/// Strip the `::h0123abcd` hash and generic noise rustc leaves in names.
fn tidy(name: &str) -> String {
    let n = match name.rfind("::h") {
        Some(i) if name.len() - i == 19 => &name[..i],
        _ => name,
    };
    n.to_string()
}

fn main() {
    let args = parse_args();
    unsafe {
        let proc_ = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, FALSE, args.pid);
        assert!(!proc_.is_null(), "OpenProcess({}) failed", args.pid);
        SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_LINES);
        // The exe's own directory first: that is where cargo puts the PDB.
        let mut sympath: Vec<u16> = exe_dir(proc_).encode_utf16().collect();
        sympath.push(0);
        assert!(
            SymInitializeW(proc_, sympath.as_ptr(), TRUE) != 0,
            "SymInitialize failed"
        );

        let mut threads: Vec<Thread> = threads_of(args.pid)
            .into_iter()
            .map(|(tid, handle)| Thread {
                tid,
                handle,
                name: thread_name(handle),
                last_cycles: cycles(handle),
                cycles: 0,
                stacks: Vec::new(),
            })
            .filter(|t: &Thread| args.only.as_ref().is_none_or(|n| &t.name == n))
            .collect();
        eprintln!(
            "sampling {} threads of pid {} for {}s",
            threads.len(),
            args.pid,
            args.secs
        );

        if args.cpu_only {
            cpu_only(&mut threads, args.secs);
            return;
        }
        let start = Instant::now();
        let mut ticks = 0u64;
        while start.elapsed().as_secs_f64() < args.secs {
            let tick = Instant::now();
            for t in &mut threads {
                let c = cycles(t.handle);
                if c == t.last_cycles && !args.waits {
                    continue;
                }
                t.cycles += c - t.last_cycles;
                t.last_cycles = c;
                if let Some(s) = walk(proc_, t.handle) {
                    t.stacks.push(s);
                }
            }
            ticks += 1;
            if let Some(rest) = args.interval.checked_sub(tick.elapsed()) {
                std::thread::sleep(rest);
            }
        }
        let wall = start.elapsed().as_secs_f64();
        eprintln!("{ticks} ticks in {wall:.2}s ({:.0}/s)", ticks as f64 / wall);

        let mut names: HashMap<u64, Vec<String>> = HashMap::new();
        let mut folded = String::new();
        threads.sort_by_key(|t| std::cmp::Reverse(t.stacks.len()));
        for t in threads.iter().filter(|t| t.stacks.len() >= 20) {
            if t.cycles as f64 / wall < 2e7 {
                continue; // under ~1% of a core: not worth a section
            }
            let mut idle = 0usize;
            let mut busy: Vec<Vec<String>> = Vec::new();
            for s in &t.stacks {
                // Outermost-last list of logical frames for this sample.
                let mut logical: Vec<String> = Vec::new();
                for &a in s {
                    let n = names.entry(a).or_insert_with(|| resolve(proc_, a));
                    logical.extend(n.iter().map(|x| tidy(x)));
                }
                if !args.waits && logical.first().is_some_and(|f| is_wait(f)) {
                    idle += 1;
                } else {
                    busy.push(logical);
                }
            }
            let total = busy.len().max(1);
            println!(
                "\n=== thread {} {:?}: {} busy samples (+{} caught waiting), {:.2} Gcycles ({:.2} Gcycles/s)",
                t.tid,
                t.name,
                busy.len(),
                idle,
                t.cycles as f64 / 1e9,
                t.cycles as f64 / 1e9 / wall
            );
            let mut selfc: HashMap<String, usize> = HashMap::new();
            let mut incl: HashMap<String, usize> = HashMap::new();
            let mut stacks: HashMap<String, usize> = HashMap::new();
            for logical in &busy {
                if let Some(first) = logical.first() {
                    *selfc.entry(first.clone()).or_default() += 1;
                }
                let mut seen = HashSet::new();
                for f in logical {
                    if seen.insert(f.as_str()) {
                        *incl.entry(f.clone()).or_default() += 1;
                    }
                }
                if args.folded.is_some() {
                    let key: Vec<&str> = logical.iter().rev().map(String::as_str).collect();
                    *stacks.entry(key.join(";")).or_default() += 1;
                }
            }
            let print = |title: &str, m: &HashMap<String, usize>| {
                let mut v: Vec<_> = m.iter().collect();
                v.sort_by(|a, b| b.1.cmp(a.1));
                println!("--- {title}");
                for (name, n) in v.into_iter().take(args.top) {
                    let short: String = name.chars().take(150).collect();
                    println!(
                        "{:6.2}% {:6}  {}",
                        *n as f64 * 100.0 / total as f64,
                        n,
                        short
                    );
                }
            };
            print("self", &selfc);
            print("inclusive", &incl);
            for (k, n) in stacks {
                folded.push_str(&format!("{}-{};{} {}\n", t.name, t.tid, k, n));
            }
        }
        if let Some(path) = &args.folded {
            std::fs::write(path, folded).expect("write --folded");
        }
        for t in &threads {
            CloseHandle(t.handle);
        }
        SymCleanup(proc_);
        CloseHandle(proc_);
    }
}
