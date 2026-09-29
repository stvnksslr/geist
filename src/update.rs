//! `auto-update` / `auto-update-channel` / `check_for_updates`: a
//! GitHub-releases self-updater with the macOS `UpdatePill`'s states.
//!
//! Ghostty does this with Sparkle and an appcast; geist has no Sparkle, so the
//! moving parts are spelled out here:
//!
//! 1. **Check.** `GET` the releases feed (GitHub's `/releases` API — a JSON
//!    array), pick the newest non-draft release above the running version that
//!    the channel admits ([`select_release`]: `stable` skips pre-releases, `tip`
//!    takes them). The release must carry a `geist-manifest.json` asset
//!    ([`Manifest`]) naming each package with its SHA-256; a release without one
//!    is not an update (it is how an un-updatable release is published).
//! 2. **Download** (`download`, or a click on the pill) the zip for this
//!    architecture to `%LOCALAPPDATA%\geist\updates`, **verify its SHA-256
//!    against the manifest before anything else touches it**, and extract it
//!    into a staging directory with a `pending.json` marker.
//! 3. **Apply on the next launch** ([`startup_apply`]), never in place while
//!    running: Windows lets a running exe be *renamed* but not overwritten or
//!    deleted, so every file the update replaces is renamed to `*.old` first
//!    (the running image keeps working from the renamed file) and the new file
//!    copied in; the process then relaunches the new exe and exits. `*.old`
//!    files are swept by a later launch once nothing runs from them. The
//!    running exe is never deleted. "Restart to apply" is the same path, reached
//!    by relaunching with `geist_UPDATE_WAIT_PID` so the new process waits for
//!    this one to exit first.
//!
//! What SHA-256 does and does not buy: it catches a truncated or corrupted
//! download and a swapped *asset*, but the manifest comes from the same release,
//! so it cannot stand in for code signing — that is a release-engineering step
//! (Authenticode on geist.exe / a signed MSIX) that needs a certificate.
//!
//! An MSIX-installed geist never self-updates: the package is read-only and App
//! Installer owns updates (`packaging/geist.appinstaller`). [`is_packaged`]
//! detects that and the updater stays idle.
//!
//! All network access goes through the [`Http`] trait so the pipeline is
//! tested against a mock; the real implementation shells out to the `curl.exe`
//! that ships with Windows 10 1803+, which keeps a TLS stack out of the binary.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The release feed, overridable at build time (`geist_UPDATE_FEED`) and at
/// run time (the geist-specific `auto-update-feed` config key).
pub const DEFAULT_FEED: &str = match option_env!("geist_UPDATE_FEED") {
    Some(f) => f,
    None => "https://api.github.com/repos/stvnksslr/geist/releases?per_page=30",
};

/// The manifest asset every updatable release must carry.
pub const MANIFEST_NAME: &str = "geist-manifest.json";

/// How often a running geist re-checks (Sparkle's default interval is a day).
const RECHECK: Duration = Duration::from_secs(24 * 60 * 60);

/// The error a cancelled transfer returns. The worker recognizes it and goes
/// quietly back to idle instead of showing "Update Failed".
const CANCELLED: &str = "cancelled";

/// How long "No Updates Available" stays before the pill hides itself
/// (upstream `UpdatePill`'s reset task). Errors do not time out: upstream keeps
/// them until acknowledged.
const NOT_FOUND_SECS: u64 = 5;

/// Ghostty `auto-update`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoUpdate {
    Off,
    Check,
    Download,
}

impl AutoUpdate {
    pub fn parse(v: &str) -> Option<Self> {
        match v.to_ascii_lowercase().as_str() {
            "off" | "false" => Some(Self::Off),
            "check" => Some(Self::Check),
            "download" => Some(Self::Download),
            _ => None,
        }
    }

    /// Upstream leaves the key unset and defers to Sparkle's stored preference.
    /// geist has no such preference: a release build checks, a debug build
    /// (a developer's tree) never phones home unless asked.
    pub fn default_for_build() -> Self {
        if cfg!(debug_assertions) {
            Self::Off
        } else {
            Self::Check
        }
    }
}

/// Ghostty `auto-update-channel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Tip,
}

impl Channel {
    pub fn parse(v: &str) -> Option<Self> {
        match v.to_ascii_lowercase().as_str() {
            "stable" => Some(Self::Stable),
            "tip" => Some(Self::Tip),
            _ => None,
        }
    }

    /// Upstream's default: the channel of the running build — a pre-release
    /// version means `tip`.
    pub fn of_version(v: &str) -> Self {
        match Version::parse(v) {
            Some(v) if !v.pre.is_empty() => Self::Tip,
            _ => Self::Stable,
        }
    }
}

// ---------------------------------------------------------------- versions --

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ident {
    Num(u64),
    Alpha(String),
}

/// A SemVer 2 version (build metadata ignored for ordering, as the spec says).
/// A leading `v` (git tags) is accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pre: Vec<Ident>,
}

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut it = core.split('.');
        let num = |p: Option<&str>| -> Option<u64> {
            let p = p?;
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).then(|| p.parse().ok())?
        };
        let (major, minor, patch) = (num(it.next())?, num(it.next())?, num(it.next())?);
        if it.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(p) => p
                .split('.')
                .map(|id| {
                    if id.is_empty() {
                        None
                    } else if id.bytes().all(|b| b.is_ascii_digit()) {
                        id.parse().ok().map(Ident::Num)
                    } else {
                        Some(Ident::Alpha(id.to_string()))
                    }
                })
                .collect::<Option<Vec<_>>>()?,
        };
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl Ord for Version {
    fn cmp(&self, o: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(o.major, o.minor, o.patch))
            .then_with(|| match (self.pre.is_empty(), o.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater, // 1.0.0 > 1.0.0-tip
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&o.pre) {
                        let c = match (a, b) {
                            (Ident::Num(x), Ident::Num(y)) => x.cmp(y),
                            (Ident::Num(_), Ident::Alpha(_)) => Ordering::Less,
                            (Ident::Alpha(_), Ident::Num(_)) => Ordering::Greater,
                            (Ident::Alpha(x), Ident::Alpha(y)) => x.cmp(y),
                        };
                        if c != Ordering::Equal {
                            return c;
                        }
                    }
                    self.pre.len().cmp(&o.pre.len())
                }
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

// ------------------------------------------------------------ feed + manifest --

/// One entry of GitHub's `/releases` response (only the fields used).
#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub html_url: String,
    /// GitHub's RFC 3339 timestamp, for the popover's "Released:" row.
    #[serde(default)]
    pub published_at: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// `geist-manifest.json`, written by `scripts/package.ps1`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub channel: Option<String>,
    pub files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ManifestFile {
    pub name: String,
    /// `x64` / `arm64`.
    pub arch: String,
    /// `zip` (the self-updater's input) or `msix` (App Installer's).
    pub kind: String,
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
}

/// Parse and validate a manifest: every hash must be 64 hex digits and the
/// version must parse. A malformed manifest is an error, never "no update".
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, String> {
    let m: Manifest = serde_json::from_slice(bytes).map_err(|e| format!("bad manifest: {e}"))?;
    Version::parse(&m.version).ok_or_else(|| format!("bad manifest version {:?}", m.version))?;
    for f in &m.files {
        if f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("bad sha256 for {}", f.name));
        }
    }
    Ok(m)
}

/// The newest release the channel admits that is newer than `current`.
pub fn select_release<'a>(
    releases: &'a [Release],
    channel: Channel,
    current: &Version,
) -> Option<(&'a Release, Version)> {
    releases
        .iter()
        .filter(|r| !r.draft)
        .filter_map(|r| Some((r, Version::parse(&r.tag_name)?)))
        .filter(|(r, v)| channel == Channel::Tip || (!r.prerelease && !v.is_prerelease()))
        .filter(|(_, v)| v > current)
        .max_by(|a, b| a.1.cmp(&b.1))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Streamed SHA-256 of a file, compared case-insensitively against `want`.
pub fn verify_file(path: &Path, want: &str) -> Result<(), String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got.eq_ignore_ascii_case(want) {
        Ok(())
    } else {
        Err(format!("checksum mismatch (expected {want}, got {got})"))
    }
}

pub fn current_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    }
}

/// A resolved update: what to fetch and what it must hash to.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub notes_url: String,
    /// The release date, `YYYY-MM-DD`, or empty when the feed has none.
    pub released: String,
}

/// `2026-09-19` as upstream's abbreviated date, `Sep 19, 2026`; anything that
/// isn't a date comes back as it was.
pub fn format_release_date(ymd: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut it = ymd.splitn(3, '-');
    let parsed = (|| {
        let y: u32 = it.next()?.parse().ok()?;
        let m: usize = it.next()?.parse().ok()?;
        let d: u32 = it.next()?.parse().ok()?;
        Some(format!("{} {d}, {y}", MONTHS.get(m.checked_sub(1)?)?))
    })();
    parsed.unwrap_or_else(|| ymd.to_string())
}

// -------------------------------------------------------------------- HTTP --

/// The network seam. Tests substitute a map of canned responses.
pub trait Http: Send + Sync {
    fn get(&self, url: &str) -> Result<Vec<u8>, String>;
    /// Stream `url` to `dest`, reporting bytes written so far. `progress`
    /// returns whether to keep going: `false` (the pill's Cancel) must stop the
    /// transfer and return an error.
    fn download(
        &self,
        url: &str,
        dest: &Path,
        progress: &dyn Fn(u64) -> bool,
    ) -> Result<(), String>;
}

/// `curl.exe` from System32: present on every supported Windows, speaks TLS
/// through Schannel, follows GitHub's asset redirects.
pub struct Curl;

impl Curl {
    fn cmd() -> std::process::Command {
        let exe = std::env::var_os("SystemRoot")
            .map(|r| PathBuf::from(r).join("System32").join("curl.exe"))
            .filter(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from("curl.exe"));
        let mut c = std::process::Command::new(exe);
        c.args(["-fsSL", "--proto", "=https", "--max-time", "600", "-A"])
            .arg(format!("geist/{}", crate::about::VERSION));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        c
    }
}

impl Http for Curl {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        let out = Self::cmd()
            .args(["-H", "Accept: application/vnd.github+json", url])
            .output()
            .map_err(|e| format!("curl: {e}"))?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(format!(
                "fetch failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn download(
        &self,
        url: &str,
        dest: &Path,
        progress: &dyn Fn(u64) -> bool,
    ) -> Result<(), String> {
        let mut child = Self::cmd()
            .arg("-o")
            .arg(dest)
            .arg(url)
            .spawn()
            .map_err(|e| format!("curl: {e}"))?;
        loop {
            if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
                return if st.success() {
                    Ok(())
                } else {
                    Err(format!("download failed ({st})"))
                };
            }
            let done = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
            if !progress(done) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CANCELLED.into());
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}

/// Resolve the update for this build, or `None` when up to date.
pub fn find_update(
    http: &dyn Http,
    feed: &str,
    channel: Channel,
    current: &str,
) -> Result<Option<Plan>, String> {
    let cur =
        Version::parse(current).ok_or_else(|| format!("unparseable current version {current}"))?;
    let releases: Vec<Release> =
        serde_json::from_slice(&http.get(feed)?).map_err(|e| format!("bad release feed: {e}"))?;
    let Some((rel, ver)) = select_release(&releases, channel, &cur) else {
        return Ok(None);
    };
    let man_asset = rel
        .asset(MANIFEST_NAME)
        .ok_or_else(|| format!("release {} has no {MANIFEST_NAME}", rel.tag_name))?;
    let man = parse_manifest(&http.get(&man_asset.browser_download_url)?)?;
    if Version::parse(&man.version) != Some(ver.clone()) {
        return Err(format!(
            "manifest version {} does not match release {}",
            man.version, rel.tag_name
        ));
    }
    let file = man
        .files
        .iter()
        .find(|f| f.kind == "zip" && f.arch == current_arch())
        .ok_or_else(|| format!("release {} has no {} zip", rel.tag_name, current_arch()))?;
    let asset = rel
        .asset(&file.name)
        .ok_or_else(|| format!("manifest names {} but the release lacks it", file.name))?;
    Ok(Some(Plan {
        version: man.version.trim_start_matches(['v', 'V']).to_string(),
        url: asset.browser_download_url.clone(),
        sha256: file.sha256.to_ascii_lowercase(),
        size: if file.size > 0 { file.size } else { asset.size },
        notes_url: rel.html_url.clone(),
        released: rel.published_at.get(..10).unwrap_or_default().to_string(),
    }))
}

// ----------------------------------------------------------------- staging --

/// Written beside the staged files once they are verified and extracted.
#[derive(Clone, Debug, serde::Serialize, Deserialize, PartialEq)]
pub struct Pending {
    pub version: String,
    pub dir: PathBuf,
}

pub fn updates_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("geist").join("updates"))
}

pub type Extract<'a> = &'a dyn Fn(&Path, &Path) -> Result<(), String>;

/// `tar.exe` (bsdtar, in System32 since Windows 10 1803) reads zip.
pub fn extract_with_tar(zip: &Path, into: &Path) -> Result<(), String> {
    let mut c = std::process::Command::new("tar.exe");
    c.arg("-xf").arg(zip).arg("-C").arg(into);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let st = c.status().map_err(|e| format!("tar: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("extract failed ({st})"))
    }
}

/// Download, verify, extract, mark pending. Nothing is extracted from a file
/// whose hash does not match; a mismatching download is deleted.
///
/// `progress` returns whether to keep going (the pill's Cancel). It is asked
/// once more just before `pending.json` is written — a cancel that lands during
/// extraction must not leave an update staged, or it would install itself on
/// the next launch after the user said no.
pub fn download_and_stage(
    http: &dyn Http,
    plan: &Plan,
    root: &Path,
    extract: Extract,
    progress: &dyn Fn(u64) -> bool,
) -> Result<Pending, String> {
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let part = root.join(format!("geist-{}.zip.part", plan.version));
    let _ = std::fs::remove_file(&part);
    if let Err(e) = http.download(&plan.url, &part, progress) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    if let Err(e) = verify_file(&part, &plan.sha256) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    let staged = root.join(format!("staged-{}", plan.version));
    let _ = std::fs::remove_dir_all(&staged);
    std::fs::create_dir_all(&staged).map_err(|e| e.to_string())?;
    let r = extract(&part, &staged);
    let _ = std::fs::remove_file(&part);
    r?;
    // A zip with a single top-level folder stages that folder's contents.
    let dir = single_subdir(&staged).unwrap_or_else(|| staged.clone());
    if !dir.join("geist.exe").is_file() {
        return Err("update package has no geist.exe".into());
    }
    if !progress(plan.size) {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(CANCELLED.into());
    }
    let p = Pending {
        version: plan.version.clone(),
        dir,
    };
    std::fs::write(root.join("pending.json"), serde_json::to_vec(&p).unwrap())
        .map_err(|e| e.to_string())?;
    Ok(p)
}

fn single_subdir(dir: &Path) -> Option<PathBuf> {
    let entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    match entries.as_slice() {
        [e] if e.path().is_dir() => Some(e.path()),
        _ => None,
    }
}

pub fn read_pending(root: &Path) -> Option<Pending> {
    serde_json::from_slice(&std::fs::read(root.join("pending.json")).ok()?).ok()
}

/// Copy `staged` over `install`: each file that already exists is first
/// renamed aside (`name.old`, or `name.old-N` if an older one is still locked
/// by a running process), which is the only way to replace a running exe or a
/// loaded DLL. Returns the paths renamed aside. On a copy failure, the renames
/// done so far are rolled back.
pub fn apply_staged(staged: &Path, install: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_files(staged, staged, &mut files).map_err(|e| e.to_string())?;
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut result = Ok(());
    for rel in &files {
        let dst = install.join(rel);
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if dst.exists() {
            match aside_name(&dst).and_then(|aside| {
                std::fs::rename(&dst, &aside)
                    .map(|_| aside)
                    .map_err(|e| e.to_string())
            }) {
                Ok(aside) => moved.push((dst.clone(), aside)),
                Err(e) => {
                    result = Err(format!("cannot move {} aside: {e}", dst.display()));
                    break;
                }
            }
        }
        if let Err(e) = std::fs::copy(staged.join(rel), &dst) {
            result = Err(format!("cannot install {}: {e}", dst.display()));
            break;
        }
    }
    if let Err(e) = result {
        for (orig, aside) in moved.iter().rev() {
            let _ = std::fs::remove_file(orig);
            let _ = std::fs::rename(aside, orig);
        }
        return Err(e);
    }
    Ok(moved.into_iter().map(|(_, a)| a).collect())
}

fn aside_name(p: &Path) -> Result<PathBuf, String> {
    let name = p
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    for n in 0..100 {
        let cand = if n == 0 {
            p.with_file_name(format!("{name}.old"))
        } else {
            p.with_file_name(format!("{name}.old-{n}"))
        };
        if !cand.exists() || std::fs::remove_file(&cand).is_ok() {
            return Ok(cand);
        }
    }
    Err("too many locked .old files".into())
}

fn collect_files(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if p.is_dir() {
            collect_files(base, &p, out)?;
        } else {
            out.push(p.strip_prefix(base).unwrap().to_path_buf());
        }
    }
    Ok(())
}

/// Best-effort sweep of `*.old` / `*.old-N` left by an earlier apply. A file
/// still mapped by a running process fails to delete and is kept for later.
pub fn sweep_old(install: &Path) {
    let Ok(rd) = std::fs::read_dir(install) else {
        return;
    };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.ends_with(".old")
            || n.rsplit_once(".old-")
                .is_some_and(|(_, k)| k.bytes().all(|b| b.is_ascii_digit()))
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

// ------------------------------------------------------------ process-level --

/// Whether this process runs from an MSIX package (then App Installer owns
/// updates and the install directory is read-only).
pub fn is_packaged() -> bool {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentPackageFullName(len: *mut u32, name: *mut u16) -> i32;
        }
        let mut len = 0u32;
        // SAFETY: a zero-length query; 15700 = APPMODEL_ERROR_NO_PACKAGE.
        let r = unsafe { GetCurrentPackageFullName(&mut len, std::ptr::null_mut()) };
        r != 15700
    }
    #[cfg(not(windows))]
    false
}

/// Run first thing in `main`. Waits for a predecessor that asked for a restart,
/// sweeps old files, and — if a newer verified update is staged — installs it
/// over the exe's directory, relaunches the new exe with the same arguments,
/// and returns `true` (the caller exits).
pub fn startup_apply() -> bool {
    if let Some(pid) = std::env::var("geist_UPDATE_WAIT_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
    {
        // SAFETY: plain env mutation before any other thread exists.
        unsafe { std::env::remove_var("geist_UPDATE_WAIT_PID") };
        wait_for_pid(pid, Duration::from_secs(15));
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(install) = exe.parent() else {
        return false;
    };
    sweep_old(install);
    if is_packaged() {
        return false;
    }
    let Some(root) = updates_root() else {
        return false;
    };
    let Some(p) = read_pending(&root) else {
        return false;
    };
    let cur = Version::parse(crate::about::VERSION);
    let newer = matches!((Version::parse(&p.version), cur), (Some(n), Some(c)) if n > c);
    let _ = std::fs::remove_file(root.join("pending.json"));
    if !newer || !p.dir.join("geist.exe").is_file() {
        let _ = std::fs::remove_dir_all(&p.dir);
        return false;
    }
    match apply_staged(&p.dir, install) {
        Ok(_) => {
            let _ = std::fs::remove_dir_all(&p.dir);
            let args: Vec<_> = std::env::args_os().skip(1).collect();
            std::process::Command::new(&exe).args(args).spawn().is_ok()
        }
        Err(e) => {
            // Leave the running (old) version usable; say why once.
            let _ = std::fs::write(root.join("last-error.txt"), e);
            false
        }
    }
}

fn wait_for_pid(pid: u32, max: Duration) {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
            fn WaitForSingleObject(h: *mut std::ffi::c_void, ms: u32) -> u32;
            fn CloseHandle(h: *mut std::ffi::c_void) -> i32;
        }
        // SAFETY: SYNCHRONIZE-only handle, closed below; 0 = no such process.
        unsafe {
            let h = OpenProcess(0x0010_0000, 0, pid);
            if !h.is_null() {
                WaitForSingleObject(h, max.as_millis() as u32);
                CloseHandle(h);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (pid, max);
}

// ------------------------------------------------------------------ state --

/// The pill's states (macOS `UpdateState`, minus `permissionRequest` — the
/// config key is the permission — and with extraction folded into download).
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    Checking,
    NotFound,
    Available(Plan),
    Downloading {
        version: String,
        done: u64,
        total: u64,
    },
    Ready {
        version: String,
    },
    Error(String),
}

/// What the pill draws before its label (upstream `UpdateBadge`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Badge {
    /// A static icon.
    Icon(char),
    /// An icon that turns — upstream spins `checking`'s once every 2.5 s.
    Spinning(char),
    /// A progress ring, `0.0..=1.0`.
    Ring(f32),
}

/// The pill's colour role (upstream `UpdateViewModel.backgroundColor` and
/// `foregroundColor`), resolved against the chrome palette by the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// A control-coloured capsule with ordinary text.
    Neutral,
    /// The accent fill: an update is waiting.
    Accent,
    /// The accent darkened toward black: "No Updates Available".
    Info,
    /// A faint warning fill with warning-coloured text.
    Warning,
}

/// What a click on the pill does (upstream `UpdatePill.pillButton`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PillClick {
    /// Open or close the popover.
    Popover,
    /// "No Updates Available": the click just acknowledges it.
    Dismiss,
    /// "Restart to Complete Update": ask, then restart into the new version.
    Restart,
}

// Material Design Icons, which the embedded Nerd Font carries at their MDI code
// points (`badge_glyphs_are_in_the_ui_font` pins that). Upstream's SF Symbol in
// each comment.
const ICON_SYNC: char = '\u{F04E6}'; // arrow.triangle.2.circlepath
const ICON_PACKAGE: char = '\u{F03D7}'; // shippingbox.fill
const ICON_DOWNLOAD: char = '\u{F01DA}'; // arrow.down.circle
const ICON_POWER: char = '\u{F0425}'; // power.circle
const ICON_INFO: char = '\u{F02FD}'; // info.circle
const ICON_ALERT: char = '\u{F0026}'; // exclamationmark.triangle.fill

impl State {
    /// The pill label (upstream `UpdateViewModel.text`). An error keeps the
    /// short "Update Failed" here and puts the message in the popover: geist's
    /// errors are curl's stderr, far too long for a titlebar pill.
    pub fn text(&self) -> String {
        match self {
            State::Idle => String::new(),
            State::Checking => "Checking for Updates\u{2026}".into(),
            State::NotFound => "No Updates Available".into(),
            State::Available(p) => format!("Update Available: {}", p.version),
            State::Downloading { total, done, .. } if *total > 0 => {
                format!(
                    "Downloading: {:.0}%",
                    (*done as f64 / *total as f64).min(1.0) * 100.0
                )
            }
            State::Downloading { .. } => "Downloading\u{2026}".into(),
            State::Ready { .. } => "Restart to Complete Update".into(),
            State::Error(_) => "Update Failed".into(),
        }
    }

    /// The label the pill is sized for, so a percentage ticking up doesn't
    /// resize it every frame (upstream `maxWidthText`).
    pub fn max_width_text(&self) -> String {
        match self {
            State::Downloading { total, .. } if *total > 0 => "Downloading: 100%".into(),
            _ => self.text(),
        }
    }

    pub fn badge(&self) -> Option<Badge> {
        Some(match self {
            State::Idle => return None,
            State::Checking => Badge::Spinning(ICON_SYNC),
            State::NotFound => Badge::Icon(ICON_INFO),
            State::Available(_) => Badge::Icon(ICON_PACKAGE),
            State::Downloading { done, total, .. } if *total > 0 => {
                Badge::Ring((*done as f32 / *total as f32).clamp(0.0, 1.0))
            }
            State::Downloading { .. } => Badge::Icon(ICON_DOWNLOAD),
            State::Ready { .. } => Badge::Icon(ICON_POWER),
            State::Error(_) => Badge::Icon(ICON_ALERT),
        })
    }

    pub fn tone(&self) -> Tone {
        match self {
            State::Available(_) => Tone::Accent,
            State::NotFound => Tone::Info,
            State::Error(_) => Tone::Warning,
            _ => Tone::Neutral,
        }
    }

    /// Upstream `UpdateState.isHidden`: the pill exists in every state but idle.
    pub fn is_hidden(&self) -> bool {
        matches!(self, State::Idle)
    }

    pub fn click(&self) -> PillClick {
        match self {
            State::NotFound => PillClick::Dismiss,
            State::Ready { .. } => PillClick::Restart,
            _ => PillClick::Popover,
        }
    }

    /// Upstream's `.help(model.text)`: the label itself, except that an error's
    /// hover shows the message the short "Update Failed" label stands for.
    pub fn tooltip(&self) -> String {
        match self {
            State::Error(e) => e.clone(),
            _ => self.text(),
        }
    }
}

struct Inner {
    state: State,
    busy: bool,
    last_check: Option<Instant>,
    shown_at: Option<Instant>,
    /// Bumped by every dismissal (Cancel, Later, Skip, OK). A worker takes the
    /// generation when it starts and only publishes its result if it still
    /// matches, so a cancelled check's answer or download's completion lands
    /// nowhere instead of resurrecting the pill.
    generation: u64,
    /// "Install and Relaunch": restart as soon as the download is staged.
    relaunch: bool,
    /// Debug builds' `geist_UPDATE_SIMULATE`: never touch the network.
    simulated: bool,
    /// `check_for_updates` arrived while a silent scheduled check was already
    /// out: that check reports as if it had been asked for by hand.
    manual_waiting: bool,
}

/// The process-wide updater (one per process, like the pill's model).
pub struct Updater {
    inner: Mutex<Inner>,
    http: Arc<dyn Http>,
    wake: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

static UPDATER: OnceLock<Updater> = OnceLock::new();

pub fn global() -> &'static Updater {
    UPDATER.get_or_init(|| Updater::new(Arc::new(Curl)))
}

/// The effective settings, resolved from config at each tick.
#[derive(Clone, Debug)]
pub struct Settings {
    pub mode: AutoUpdate,
    pub channel: Channel,
    pub feed: String,
}

impl Settings {
    pub fn from_config(c: &crate::config::Config) -> Self {
        Self {
            mode: c.auto_update,
            channel: c
                .auto_update_channel
                .unwrap_or_else(|| Channel::of_version(crate::about::VERSION)),
            feed: c.auto_update_feed.clone(),
        }
    }
}

/// Where Skip remembers the version it skipped (Sparkle keeps the same).
fn skip_file(root: &Path) -> PathBuf {
    root.join("skipped-version")
}

/// The version the user chose to Skip, if any. Only an *automatic* check
/// honours it; asking by hand still offers the update, as Sparkle does.
pub fn skipped_version(root: &Path) -> Option<String> {
    let v = std::fs::read_to_string(skip_file(root)).ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

pub fn skip_version(root: &Path, version: &str) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    std::fs::write(skip_file(root), version).map_err(|e| e.to_string())
}

impl Updater {
    pub fn new(http: Arc<dyn Http>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                state: State::Idle,
                busy: false,
                last_check: None,
                shown_at: None,
                generation: 0,
                relaunch: false,
                simulated: false,
                manual_waiting: false,
            }),
            http,
            wake: OnceLock::new(),
        }
    }

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state.clone()
    }

    /// Whether this is `geist_UPDATE_SIMULATE`'s pretend updater, whose
    /// "restart" must not actually restart anything.
    pub fn simulated(&self) -> bool {
        self.inner.lock().unwrap().simulated
    }

    fn wake(&self) {
        if let Some(w) = self.wake.get() {
            w();
        }
    }

    fn set(&self, s: State) {
        {
            let mut i = self.inner.lock().unwrap();
            i.state = s;
            i.shown_at = Some(Instant::now());
        }
        self.wake();
    }

    /// Publish `s` from a worker that started at `generation`, unless the user
    /// dismissed it since. Returns whether it was published.
    fn set_if(&self, generation: u64, s: State) -> bool {
        {
            let mut i = self.inner.lock().unwrap();
            if i.generation != generation {
                return false;
            }
            i.state = s;
            i.shown_at = Some(Instant::now());
        }
        self.wake();
        true
    }

    /// Called every UI pass: install the repaint hook, run the startup check
    /// and the daily re-check, and let "No Updates Available" fade.
    pub fn tick(&'static self, wake: impl Fn() + Send + Sync + 'static, s: &Settings) {
        let _ = self.wake.set(Box::new(wake));
        #[cfg(debug_assertions)]
        self.simulate_from_env();
        let simulated = self.simulated();
        if is_packaged() && !simulated {
            return;
        }
        {
            let mut i = self.inner.lock().unwrap();
            if i.state == State::NotFound
                && i.shown_at
                    .is_some_and(|t| t.elapsed() > Duration::from_secs(NOT_FOUND_SECS))
            {
                i.state = State::Idle;
            }
        }
        if s.mode == AutoUpdate::Off || simulated {
            return;
        }
        let due = {
            let i = self.inner.lock().unwrap();
            !i.busy && i.state == State::Idle && i.last_check.is_none_or(|t| t.elapsed() > RECHECK)
        };
        // The first check waits a few seconds so it never competes with startup.
        static FIRST_TICK: OnceLock<Instant> = OnceLock::new();
        let settled = FIRST_TICK.get_or_init(Instant::now).elapsed() > Duration::from_secs(5);
        if due && settled {
            self.check(s.clone(), false);
        }
    }

    /// `check_for_updates`, or the scheduled check (`manual = false`, which
    /// stays silent when there is nothing new or the network is down, and
    /// passes over a version the user chose to Skip).
    pub fn check(&'static self, s: Settings, manual: bool) {
        if is_packaged() && !self.simulated() {
            if manual {
                self.set(State::Error(
                    "This copy is installed as a package; App Installer / the Store updates it."
                        .into(),
                ));
            }
            return;
        }
        let generation = {
            let mut i = self.inner.lock().unwrap();
            if matches!(i.state, State::Ready { .. } | State::Downloading { .. }) {
                return; // the pill already says what is happening
            }
            if i.busy {
                // A silent scheduled check is already out. Asking by hand must
                // not just do nothing: show "Checking…" and let that check
                // report as a manual one would.
                if manual && i.state == State::Idle {
                    i.manual_waiting = true;
                    i.state = State::Checking;
                    i.shown_at = Some(Instant::now());
                    drop(i);
                    self.wake();
                }
                return;
            }
            i.busy = true;
            i.last_check = Some(Instant::now());
            i.generation
        };
        if manual {
            self.set(State::Checking);
        }
        let simulated = self.simulated();
        std::thread::spawn(move || {
            let r = if simulated {
                std::thread::sleep(Duration::from_millis(1200));
                Ok(None)
            } else {
                find_update(&*self.http, &s.feed, s.channel, crate::about::VERSION)
            };
            let manual = {
                let mut i = self.inner.lock().unwrap();
                if i.generation != generation {
                    return; // cancelled; `dismiss` already cleared `busy`
                }
                i.busy = false;
                manual || std::mem::take(&mut i.manual_waiting)
            };
            let skipped = (!manual)
                .then(updates_root)
                .flatten()
                .and_then(|root| skipped_version(&root));
            match r {
                Ok(Some(plan)) if skipped.as_deref() == Some(plan.version.as_str()) => {
                    self.set_if(generation, State::Idle);
                }
                // `auto-update = download` fetches unattended; someone who asked
                // gets the popover and chooses, as upstream's user driver shows.
                Ok(Some(plan)) if s.mode == AutoUpdate::Download && !manual => {
                    self.download(plan, false)
                }
                Ok(Some(plan)) => {
                    self.set_if(generation, State::Available(plan));
                }
                Ok(None) if manual => {
                    self.set_if(generation, State::NotFound);
                }
                Err(e) if manual => {
                    self.set_if(generation, State::Error(e));
                }
                _ => {
                    self.set_if(generation, State::Idle);
                }
            }
        });
    }

    /// Fetch, verify and stage `plan` on a worker thread. `attended`: someone
    /// asked for it (Install and Relaunch), so a failure shows "Update Failed".
    /// An unattended one (`auto-update = download`) fails silently, as the
    /// automatic check does and as upstream's automatic driver never reports.
    pub fn download(&'static self, plan: Plan, attended: bool) {
        let simulated = self.simulated();
        let root = if simulated {
            PathBuf::new()
        } else if let Some(root) = updates_root() {
            root
        } else {
            if attended {
                self.set(State::Error("LOCALAPPDATA is not set".into()));
            }
            return;
        };
        let generation = {
            let mut i = self.inner.lock().unwrap();
            if i.busy {
                return;
            }
            i.busy = true;
            i.generation
        };
        let total = plan.size;
        self.set_if(
            generation,
            State::Downloading {
                version: plan.version.clone(),
                done: 0,
                total,
            },
        );
        std::thread::spawn(move || {
            let v = plan.version.clone();
            // `false` once the user cancels: curl is killed, and nothing is
            // staged for the next launch.
            let progress = |done| {
                self.set_if(
                    generation,
                    State::Downloading {
                        version: v.clone(),
                        done,
                        total,
                    },
                )
            };
            let r = if simulated {
                simulate_download(&plan, &progress)
            } else {
                download_and_stage(&*self.http, &plan, &root, &extract_with_tar, &progress)
            };
            // Published under the same lock that checks the generation, so a
            // Cancel can't slip between the check and the publish. A Cancel
            // that beat us after the last progress report found the state
            // still `Downloading` and won: undo the staging, or the update the
            // user just cancelled would install itself on the next launch.
            let published = {
                let mut i = self.inner.lock().unwrap();
                if i.generation == generation {
                    i.busy = false;
                    i.state = match &r {
                        Ok(p) => State::Ready {
                            version: p.version.clone(),
                        },
                        Err(e) if e == CANCELLED || !attended => State::Idle,
                        Err(e) => State::Error(e.clone()),
                    };
                    if !matches!(i.state, State::Ready { .. }) {
                        i.relaunch = false;
                    }
                    i.shown_at = Some(Instant::now());
                    true
                } else {
                    false
                }
            };
            if published {
                self.wake();
            } else if let Ok(p) = &r
                && !simulated
            {
                discard_staged(&root, &p.version);
            }
        });
    }

    /// Back to idle: the popover's Cancel (while checking or downloading),
    /// Later, OK, and a click on "No Updates Available". Bumping the generation
    /// is what makes the in-flight worker's result land nowhere; a download
    /// sees it on its next progress report and kills curl.
    pub fn dismiss(&self) {
        {
            let mut i = self.inner.lock().unwrap();
            if matches!(i.state, State::Ready { .. }) {
                return; // staged: only a restart (or the next launch) clears it
            }
            i.generation += 1;
            i.busy = false;
            i.relaunch = false;
            i.manual_waiting = false;
            i.state = State::Idle;
            i.shown_at = Some(Instant::now());
            // A dismissal ends this round, so the next scheduled check is a
            // full interval away. Otherwise a pill left up past the daily
            // re-check would come straight back the moment it was dismissed.
            i.last_check = Some(Instant::now());
        }
        self.wake();
    }

    /// "Install and Relaunch": download, then restart as soon as it is staged
    /// (the app still asks first, because a restart ends every shell).
    pub fn install(&'static self) {
        let State::Available(plan) = self.state() else {
            return;
        };
        self.inner.lock().unwrap().relaunch = true;
        self.download(plan, true);
    }

    /// "Skip": never offer this version again on an automatic check.
    pub fn skip(&self) {
        if let State::Available(plan) = self.state()
            && !self.simulated()
            && let Some(root) = updates_root()
        {
            let _ = skip_version(&root, &plan.version);
        }
        self.dismiss();
    }

    /// The error popover's Retry: check again, by hand, with the settings as
    /// they are now (the failure may have been a feed URL since corrected).
    pub fn retry(&'static self, s: Settings) {
        self.dismiss();
        self.check(s, true);
    }

    /// Whether "Install and Relaunch" is due: the download it started is now
    /// staged. Consumed, so the restart is asked for exactly once.
    pub fn take_relaunch(&self) -> bool {
        let mut i = self.inner.lock().unwrap();
        let due = i.relaunch && matches!(i.state, State::Ready { .. });
        if due {
            i.relaunch = false;
        }
        due
    }

    /// Debug builds: `geist_UPDATE_SIMULATE=checking|available|downloading|
    /// ready|notfound|error` puts the pill in that state without touching the
    /// network, and the popover's actions then play out against a stand-in (a
    /// download ticks to 100% over three seconds, a check finds nothing, and a
    /// restart only says so). The counterpart of upstream's `UpdateSimulator`:
    /// the only way to look at every state without publishing a release.
    #[cfg(debug_assertions)]
    fn simulate_from_env(&self) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let Ok(which) = std::env::var("geist_UPDATE_SIMULATE") else {
                return;
            };
            let plan = Plan {
                version: "9.9.9".into(),
                url: String::new(),
                sha256: String::new(),
                size: 48 * 1024 * 1024,
                notes_url: "https://github.com/stvnksslr/geist/releases".into(),
                released: "2026-09-19".into(),
            };
            let state = match which.as_str() {
                "checking" => State::Checking,
                "available" => State::Available(plan.clone()),
                "downloading" => State::Downloading {
                    version: plan.version.clone(),
                    done: plan.size * 2 / 5,
                    total: plan.size,
                },
                "ready" => State::Ready {
                    version: plan.version.clone(),
                },
                "notfound" => State::NotFound,
                "error" => State::Error(
                    "curl: (6) Could not resolve host: api.github.com (simulated)".into(),
                ),
                _ => return,
            };
            let mut i = self.inner.lock().unwrap();
            i.simulated = true;
            i.state = state;
            i.shown_at = Some(Instant::now());
            // "checking" is a pretend check that never finishes; Cancel is the
            // way out of it, which is what the state is there to show.
        });
    }
}

/// Undo a staging the user cancelled in the last instant: its `pending.json`
/// (only if it names this version) and its staged files.
fn discard_staged(root: &Path, version: &str) {
    if read_pending(root).is_some_and(|p| p.version == version) {
        let _ = std::fs::remove_file(root.join("pending.json"));
    }
    let _ = std::fs::remove_dir_all(root.join(format!("staged-{version}")));
}

/// `geist_UPDATE_SIMULATE`'s download: thirty ticks of fake progress, honouring
/// Cancel exactly as the real one does.
fn simulate_download(plan: &Plan, progress: &dyn Fn(u64) -> bool) -> Result<Pending, String> {
    for step in 1..=30u64 {
        std::thread::sleep(Duration::from_millis(100));
        if !progress(plan.size * step / 30) {
            return Err(CANCELLED.into());
        }
    }
    Ok(Pending {
        version: plan.version.clone(),
        dir: PathBuf::new(),
    })
}

/// "Restart to apply": launch the (still old) exe with `--restore-session`,
/// told to wait for this process; its [`startup_apply`] installs the staged
/// files and hands over to the new exe. The caller saves state and exits.
pub fn spawn_restart() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe)
        .arg("--restore-session")
        .env("geist_UPDATE_WAIT_PID", std::process::id().to_string())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Ask before killing every shell (a native message box, so it needs no egui
/// input gates). `true` = go ahead.
pub fn confirm_restart(version: &str) -> bool {
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, flags: u32) -> i32;
        }
        let w = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let text = w(&format!(
            "Restart geist to finish installing version {version}?\n\nRunning shells will be closed; the window layout is restored."
        ));
        let cap = w("geist update");
        // MB_OKCANCEL | MB_ICONQUESTION | MB_TASKMODAL; IDOK = 1.
        unsafe { MessageBoxW(0, text.as_ptr(), cap.as_ptr(), 0x1 | 0x20 | 0x2000) == 1 }
    }
    #[cfg(not(windows))]
    {
        let _ = version;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s}"))
    }

    #[test]
    fn version_ordering_follows_semver() {
        let ordered = [
            "0.1.0",
            "0.1.1-alpha",
            "0.1.1-alpha.1",
            "0.1.1-alpha.beta",
            "0.1.1-beta.2",
            "0.1.1-beta.11",
            "0.1.1-rc.1",
            "0.1.1",
            "0.2.0-tip.20260101",
            "0.2.0",
            "0.10.0",
            "1.0.0",
        ];
        for w in ordered.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v("1.2.3+build.5").cmp(&v("1.2.3")), Ordering::Equal);
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.x.3",
            "1.2.3-",
            "1.2.3-a..b",
            "01a.2.3",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn channel_parse_and_default_follow_the_running_version() {
        assert_eq!(Channel::parse("TIP"), Some(Channel::Tip));
        assert_eq!(Channel::parse("nightly"), None);
        assert_eq!(Channel::of_version("0.1.0"), Channel::Stable);
        assert_eq!(Channel::of_version("0.2.0-tip.3"), Channel::Tip);
        assert_eq!(AutoUpdate::parse("download"), Some(AutoUpdate::Download));
        assert_eq!(AutoUpdate::parse("sometimes"), None);
    }

    fn rel(tag: &str, pre: bool, draft: bool) -> Release {
        Release {
            tag_name: tag.into(),
            prerelease: pre,
            draft,
            html_url: String::new(),
            published_at: String::new(),
            assets: vec![],
        }
    }

    #[test]
    fn channel_selection() {
        let rs = vec![
            rel("v0.3.0", false, true), // draft: never
            rel("v0.2.0-tip.7", true, false),
            rel("v0.1.5", false, false),
            rel("v0.1.0", false, false),
            rel("nightly", true, false), // unparseable: skipped
        ];
        let cur = v("0.1.0");
        assert_eq!(
            select_release(&rs, Channel::Stable, &cur)
                .unwrap()
                .0
                .tag_name,
            "v0.1.5"
        );
        assert_eq!(
            select_release(&rs, Channel::Tip, &cur).unwrap().0.tag_name,
            "v0.2.0-tip.7"
        );
        assert!(select_release(&rs, Channel::Stable, &v("0.1.5")).is_none());
        // A prerelease-shaped tag not flagged prerelease is still not stable.
        let rs2 = vec![rel("v0.9.0-rc.1", false, false)];
        assert!(select_release(&rs2, Channel::Stable, &cur).is_none());
    }

    #[test]
    fn manifest_parsing_validates_hashes_and_version() {
        let good = br#"{"version":"0.2.0","channel":"stable","files":[
            {"name":"geist-0.2.0-windows-x64.zip","arch":"x64","kind":"zip","size":10,
             "sha256":"ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789"}]}"#;
        let m = parse_manifest(good).unwrap();
        assert_eq!(m.files[0].kind, "zip");
        assert!(parse_manifest(br#"{"version":"0.2.0","files":[{"name":"a","arch":"x64","kind":"zip","sha256":"abc"}]}"#).is_err());
        assert!(parse_manifest(br#"{"version":"two","files":[]}"#).is_err());
        assert!(parse_manifest(b"not json").is_err());
    }

    #[test]
    fn checksum_verification() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let p = tmp("sum").join("f.bin");
        std::fs::write(&p, b"abc").unwrap();
        assert!(
            verify_file(
                &p,
                "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
            )
            .is_ok()
        );
        assert!(
            verify_file(&p, &"0".repeat(64))
                .unwrap_err()
                .contains("mismatch")
        );
    }

    fn tmp(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("geist-update-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Canned responses; `download` writes the canned bytes. Any other URL is
    /// an error, so a test can never reach the network.
    struct Mock(HashMap<String, Vec<u8>>);
    impl Http for Mock {
        fn get(&self, url: &str) -> Result<Vec<u8>, String> {
            self.0.get(url).cloned().ok_or_else(|| format!("404 {url}"))
        }
        fn download(
            &self,
            url: &str,
            dest: &Path,
            progress: &dyn Fn(u64) -> bool,
        ) -> Result<(), String> {
            let b = self.get(url)?;
            std::fs::write(dest, &b).map_err(|e| e.to_string())?;
            if !progress(b.len() as u64) {
                return Err(CANCELLED.into());
            }
            Ok(())
        }
    }

    fn feed_with(payload: &[u8], manifest_hash: &str) -> Mock {
        let arch = current_arch();
        let feed = format!(
            r#"[{{"tag_name":"v9.0.0","prerelease":false,"draft":false,"html_url":"https://x/r",
               "published_at":"2026-09-19T17:04:11Z",
               "assets":[{{"name":"geist-manifest.json","browser_download_url":"https://x/m","size":1}},
                         {{"name":"geist-9.0.0-windows-{arch}.zip","browser_download_url":"https://x/z","size":3}}]}}]"#
        );
        let man = format!(
            r#"{{"version":"9.0.0","files":[{{"name":"geist-9.0.0-windows-{arch}.zip","arch":"{arch}","kind":"zip","sha256":"{manifest_hash}"}}]}}"#
        );
        Mock(HashMap::from([
            ("https://x/feed".to_string(), feed.into_bytes()),
            ("https://x/m".to_string(), man.into_bytes()),
            ("https://x/z".to_string(), payload.to_vec()),
        ]))
    }

    #[test]
    fn find_update_resolves_the_arch_zip_through_the_manifest() {
        let m = feed_with(b"zip", &sha256_hex(b"zip"));
        let p = find_update(&m, "https://x/feed", Channel::Stable, "0.1.0")
            .unwrap()
            .unwrap();
        assert_eq!(p.version, "9.0.0");
        assert_eq!(p.url, "https://x/z");
        assert_eq!(p.size, 3);
        assert_eq!(
            p.released, "2026-09-19",
            "the date part of GitHub's published_at"
        );
        assert!(
            find_update(&m, "https://x/feed", Channel::Stable, "9.0.0")
                .unwrap()
                .is_none()
        );
        assert!(find_update(&m, "https://x/other", Channel::Stable, "0.1.0").is_err());
    }

    #[test]
    fn staging_verifies_before_extracting() {
        let fake_extract = |_: &Path, into: &Path| -> Result<(), String> {
            let d = into.join("geist-9.0.0");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("geist.exe"), b"new").map_err(|e| e.to_string())
        };
        let root = tmp("stage");
        let m = feed_with(b"zip", &sha256_hex(b"zip"));
        let plan = find_update(&m, "https://x/feed", Channel::Stable, "0.1.0")
            .unwrap()
            .unwrap();
        let p = download_and_stage(&m, &plan, &root, &fake_extract, &|_| true).unwrap();
        assert!(
            p.dir.ends_with("geist-9.0.0"),
            "single top folder is unwrapped"
        );
        assert_eq!(read_pending(&root), Some(p));

        // A tampered payload: the extractor must never run.
        let root = tmp("stage-bad");
        let m = feed_with(b"evil", &sha256_hex(b"zip"));
        let plan = find_update(&m, "https://x/feed", Channel::Stable, "0.1.0")
            .unwrap()
            .unwrap();
        let never =
            |_: &Path, _: &Path| -> Result<(), String> { panic!("extracted an unverified file") };
        assert!(
            download_and_stage(&m, &plan, &root, &never, &|_| true)
                .unwrap_err()
                .contains("mismatch")
        );
        assert!(read_pending(&root).is_none());
        assert!(
            std::fs::read_dir(&root).unwrap().next().is_none(),
            "the bad download is deleted"
        );
    }

    #[test]
    fn apply_renames_aside_and_never_deletes_the_original() {
        let (staged, install) = (tmp("apply-s"), tmp("apply-i"));
        std::fs::write(staged.join("geist.exe"), b"new").unwrap();
        std::fs::write(staged.join("conpty.dll"), b"newdll").unwrap();
        std::fs::write(install.join("geist.exe"), b"old").unwrap();
        std::fs::write(install.join("geist.exe.old"), b"older").unwrap();
        let aside = apply_staged(&staged, &install).unwrap();
        assert_eq!(std::fs::read(install.join("geist.exe")).unwrap(), b"new");
        assert_eq!(
            std::fs::read(install.join("conpty.dll")).unwrap(),
            b"newdll"
        );
        assert_eq!(aside, vec![install.join("geist.exe.old")]);
        assert_eq!(
            std::fs::read(install.join("geist.exe.old")).unwrap(),
            b"old"
        );
        sweep_old(&install);
        assert!(!install.join("geist.exe.old").exists());
        assert!(install.join("geist.exe").exists());
    }

    #[test]
    fn config_keys_parse() {
        let c = crate::config::Config::from_ghostty_config(
            "auto-update = download\nauto-update-channel = tip\nauto-update-feed = https://example/f\n",
        );
        let s = Settings::from_config(&c);
        assert_eq!(
            (s.mode, s.channel, s.feed.as_str()),
            (AutoUpdate::Download, Channel::Tip, "https://example/f")
        );
        let d = crate::config::Config::from_ghostty_config("auto-update = bogus\n");
        assert_eq!(d.auto_update, AutoUpdate::default_for_build());
        assert!(d.diagnostics.iter().any(|m| m.contains("auto-update")));
        assert_eq!(
            Settings::from_config(&d).channel,
            Channel::of_version(crate::about::VERSION)
        );
    }

    #[test]
    fn pill_text_matches_upstream_wording() {
        let d = State::Downloading {
            version: "1".into(),
            done: 50,
            total: 200,
        };
        assert_eq!(d.text(), "Downloading: 25%");
        assert_eq!(
            State::Ready {
                version: "1".into()
            }
            .text(),
            "Restart to Complete Update"
        );
        assert_eq!(State::Idle.text(), "");
    }

    fn plan_9() -> Plan {
        Plan {
            version: "9.0.0".into(),
            url: String::new(),
            sha256: String::new(),
            size: 100,
            notes_url: String::new(),
            released: String::new(),
        }
    }

    /// A Cancel that wins the race after the download already staged: the
    /// worker undoes the staging, but only its own.
    #[test]
    fn a_late_cancel_undoes_only_its_own_staging() {
        let root = tmp("discard");
        let staged = root.join("staged-9.0.0");
        std::fs::create_dir_all(&staged).unwrap();
        let p = Pending {
            version: "9.0.0".into(),
            dir: staged.clone(),
        };
        std::fs::write(root.join("pending.json"), serde_json::to_vec(&p).unwrap()).unwrap();
        discard_staged(&root, "9.0.0");
        assert!(read_pending(&root).is_none());
        assert!(!staged.exists());

        // Some other version's pending update is not ours to remove.
        let other = Pending {
            version: "8.0.0".into(),
            dir: root.join("staged-8.0.0"),
        };
        std::fs::write(
            root.join("pending.json"),
            serde_json::to_vec(&other).unwrap(),
        )
        .unwrap();
        discard_staged(&root, "9.0.0");
        assert_eq!(read_pending(&root), Some(other));
    }

    #[test]
    fn release_dates_read_like_upstreams() {
        assert_eq!(format_release_date("2026-09-19"), "Sep 19, 2026");
        assert_eq!(format_release_date("2026-01-02"), "Jan 2, 2026");
        assert_eq!(format_release_date("2026-13-02"), "2026-13-02");
        assert_eq!(format_release_date(""), "");
    }

    /// Asking by hand while a silent scheduled check is out shows "Checking…"
    /// and lets that check report — instead of silently doing nothing.
    #[test]
    fn a_manual_check_joins_a_silent_one() {
        let u: &'static Updater = Box::leak(Box::new(Updater::new(Arc::new(Mock(HashMap::new())))));
        u.inner.lock().unwrap().busy = true;
        let s = Settings {
            mode: AutoUpdate::Check,
            channel: Channel::Stable,
            feed: String::new(),
        };
        u.check(s, true);
        assert_eq!(u.state(), State::Checking);
        assert!(u.inner.lock().unwrap().manual_waiting);
        // Cancel clears the promotion along with the state.
        u.dismiss();
        assert!(!u.inner.lock().unwrap().manual_waiting);
    }

    /// Later / Cancel / OK end the round: the next scheduled check is a full
    /// interval away, not the next frame.
    #[test]
    fn dismissing_restarts_the_recheck_interval() {
        let u = Updater::new(Arc::new(Mock(HashMap::new())));
        u.set(State::Available(plan_9()));
        u.dismiss();
        let i = u.inner.lock().unwrap();
        assert!(i.last_check.is_some_and(|t| t.elapsed() < RECHECK));
    }

    /// The table upstream's `UpdateViewModel` and `UpdateBadge` encode: which
    /// badge, colour role and click each state gets.
    #[test]
    fn pill_presentation_matches_upstream() {
        let dl = |done| State::Downloading {
            version: "9".into(),
            done,
            total: 200,
        };
        assert_eq!(dl(50).badge(), Some(Badge::Ring(0.25)));
        // Sized for the widest percentage, so the pill never jitters.
        assert_eq!(dl(50).max_width_text(), "Downloading: 100%");
        assert_eq!(dl(50).tone(), Tone::Neutral);
        let unknown = State::Downloading {
            version: "9".into(),
            done: 7,
            total: 0,
        };
        assert_eq!(unknown.badge(), Some(Badge::Icon(ICON_DOWNLOAD)));
        assert_eq!(unknown.max_width_text(), unknown.text());

        assert_eq!(State::Checking.badge(), Some(Badge::Spinning(ICON_SYNC)));
        assert_eq!(State::Available(plan_9()).tone(), Tone::Accent);
        assert_eq!(State::NotFound.tone(), Tone::Info);
        assert_eq!(State::Error("x".into()).tone(), Tone::Warning);

        assert!(State::Idle.is_hidden() && State::Idle.badge().is_none());
        assert_eq!(State::NotFound.click(), PillClick::Dismiss);
        assert_eq!(
            State::Ready {
                version: "9".into()
            }
            .click(),
            PillClick::Restart
        );
        assert_eq!(State::Available(plan_9()).click(), PillClick::Popover);
        assert_eq!(State::Error("x".into()).click(), PillClick::Popover);
    }

    /// The badges are Material Design Icons drawn from the embedded Nerd Font
    /// (the UI's last fallback face). A code point it lacks would draw as tofu.
    #[test]
    fn badge_glyphs_are_in_the_ui_font() {
        use ab_glyph::Font;
        let font = ab_glyph::FontRef::try_from_slice(crate::render::regular_font()).unwrap();
        for c in [
            ICON_SYNC,
            ICON_PACKAGE,
            ICON_DOWNLOAD,
            ICON_POWER,
            ICON_INFO,
            ICON_ALERT,
        ] {
            assert_ne!(font.glyph_id(c).0, 0, "U+{:X} missing", c as u32);
        }
    }

    /// Cancel while downloading: nothing may be left to install itself on the
    /// next launch — no `pending.json`, no partial file.
    #[test]
    fn cancelling_a_download_stages_nothing() {
        let root = tmp("cancel-dl");
        let m = feed_with(b"zip", &sha256_hex(b"zip"));
        let plan = find_update(&m, "https://x/feed", Channel::Stable, "0.1.0")
            .unwrap()
            .unwrap();
        let never = |_: &Path, _: &Path| -> Result<(), String> { panic!("extracted") };
        let e = download_and_stage(&m, &plan, &root, &never, &|_| false).unwrap_err();
        assert_eq!(e, CANCELLED);
        assert!(read_pending(&root).is_none());
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
    }

    /// …and a cancel that lands during extraction, after the download already
    /// said "keep going", is caught before `pending.json` is written.
    #[test]
    fn cancelling_after_extraction_stages_nothing() {
        let fake_extract = |_: &Path, into: &Path| -> Result<(), String> {
            std::fs::write(into.join("geist.exe"), b"new").map_err(|e| e.to_string())
        };
        let root = tmp("cancel-extract");
        let m = feed_with(b"zip", &sha256_hex(b"zip"));
        let plan = find_update(&m, "https://x/feed", Channel::Stable, "0.1.0")
            .unwrap()
            .unwrap();
        let calls = std::cell::Cell::new(0);
        let progress = |_| {
            calls.set(calls.get() + 1);
            calls.get() == 1 // the download's report passes; the final check fails
        };
        let e = download_and_stage(&m, &plan, &root, &fake_extract, &progress).unwrap_err();
        assert_eq!(e, CANCELLED);
        assert_eq!(calls.get(), 2);
        assert!(read_pending(&root).is_none());
        assert!(!root.join("staged-9.0.0").exists());
    }

    /// A worker that started before a dismissal must not resurrect the pill.
    #[test]
    fn a_dismissed_workers_result_lands_nowhere() {
        let u = Updater::new(Arc::new(Mock(HashMap::new())));
        let started_at = u.inner.lock().unwrap().generation;
        u.set(State::Checking);
        u.dismiss();
        assert!(!u.set_if(started_at, State::NotFound));
        assert_eq!(u.state(), State::Idle);
        // A worker started after it publishes normally.
        let now = u.inner.lock().unwrap().generation;
        assert!(u.set_if(now, State::NotFound));
        assert_eq!(u.state(), State::NotFound);
    }

    /// A staged update can't be dismissed (only a restart consumes it), and
    /// "Install and Relaunch" asks for the restart exactly once.
    #[test]
    fn a_staged_update_survives_dismiss_and_relaunches_once() {
        let u = Updater::new(Arc::new(Mock(HashMap::new())));
        u.inner.lock().unwrap().relaunch = true;
        u.set(State::Ready {
            version: "9.0.0".into(),
        });
        u.dismiss();
        assert!(matches!(u.state(), State::Ready { .. }));
        assert!(u.take_relaunch());
        assert!(!u.take_relaunch());
    }

    #[test]
    fn skip_is_remembered_per_version() {
        let root = tmp("skip");
        assert_eq!(skipped_version(&root), None);
        skip_version(&root, "9.0.0").unwrap();
        assert_eq!(skipped_version(&root).as_deref(), Some("9.0.0"));
        std::fs::write(skip_file(&root), "  \n").unwrap();
        assert_eq!(skipped_version(&root), None);
    }
}
