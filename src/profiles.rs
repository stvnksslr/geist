//! Shell profiles: the selectable shells a tab/pane can run (PowerShell 7,
//! Windows PowerShell, Command Prompt, WSL, or a user-configured command),
//! Windows-Terminal style. Detected at startup from what's installed.

use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// One launchable shell: a display name plus the program and args to spawn.
#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
}

impl Profile {
    fn new(name: &str, program: &str) -> Self {
        Self {
            name: name.to_string(),
            program: program.to_string(),
            args: Vec::new(),
        }
    }

    /// Whether `s` names this profile (by display name or program, ignoring
    /// case and any `.exe`).
    fn matches(&self, s: &str) -> bool {
        let norm = |x: &str| x.trim_end_matches(".exe").to_ascii_lowercase();
        norm(&self.name) == norm(s) || norm(&self.program) == norm(s)
    }

    /// The full arg list to spawn this shell with under the default
    /// shell-integration settings. See [`Profile::launch`].
    pub fn launch_args(&self) -> Vec<String> {
        self.launch(&Integration::default(), None, &[]).args
    }

    /// How to spawn this shell under `si` (Ghostty `shell-integration` +
    /// `shell-integration-features`).
    ///
    /// - **pwsh / powershell / cmd** get giest's own startup hook: an OSC 7 cwd
    ///   report and OSC 133 D/A/B prompt marks, plus the `cursor` and `title`
    ///   features. Ghostty ships no scheme for these shells, so a forced
    ///   `shell-integration = bash` (etc.) leaves them on giest's hook.
    /// - **WSL** (`wsl.exe`, no args) runs Ghostty's own bash/zsh/fish/elvish/
    ///   nushell scripts, vendored in `assets/shell-integration/` and extracted
    ///   to `si_dir`: `giest-wsl.sh` picks the login shell (or the forced one)
    ///   and injects exactly as upstream's `shell_integration.zig` does.
    /// - `shell-integration = none` injects **nothing** anywhere.
    ///
    /// Shells we don't recognise, and any profile that already carries args, are
    /// spawned untouched. `base_env` is the configured `env`, which the returned
    /// `env` extends (it only ever adds keys, merging `WSLENV`).
    pub fn launch(
        &self,
        si: &Integration,
        si_dir: Option<&std::path::Path>,
        base_env: &[(String, String)],
    ) -> Launch {
        let untouched = Launch {
            args: self.args.clone(),
            env: base_env.to_vec(),
            reset_cursor_on_submit: false,
        };
        if !self.args.is_empty() || si.mode == ShellIntegration::None {
            return untouched;
        }
        let prog = self.program.trim_end_matches(".exe").to_ascii_lowercase();
        let prog = prog.rsplit(['\\', '/']).next().unwrap_or(&prog).to_string();
        // pwsh/cmd have no pre-exec hook, so the bar cursor the prompt sets is
        // reset by the session when it sends the Enter (see `Session`).
        let reset_cursor_on_submit = si.features.cursor;
        match prog.as_str() {
            "pwsh" | "powershell" => {
                // `-EncodedCommand` runs after the user's $PROFILE loads, so the
                // hook wraps their customised prompt; `-NoExit` keeps it interactive.
                let utf16: Vec<u8> = pwsh_hook(si)
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect();
                let encoded = STANDARD.encode(utf16);
                Launch {
                    args: vec!["-NoExit".into(), "-EncodedCommand".into(), encoded],
                    env: base_env.to_vec(),
                    reset_cursor_on_submit,
                }
            }
            "cmd" => Launch {
                args: vec!["/K".into(), cmd_prompt(si)],
                env: base_env.to_vec(),
                reset_cursor_on_submit,
            },
            "wsl" => {
                let Some(dir) = si_dir else { return untouched };
                let mut env = base_env.to_vec();
                let existing = base_env
                    .iter()
                    .rev()
                    .find(|(k, _)| k.eq_ignore_ascii_case("WSLENV"))
                    .map(|(_, v)| v.clone())
                    .or_else(|| std::env::var("WSLENV").ok());
                env.retain(|(k, _)| !k.eq_ignore_ascii_case("WSLENV"));
                env.push(("WSLENV".into(), wslenv(existing.as_deref())));
                env.push((
                    "GIEST_SHELL_INTEGRATION_DIR".into(),
                    dir.to_string_lossy().into_owned(),
                ));
                env.push(("GIEST_SHELL_INTEGRATION".into(), si.mode.as_str().into()));
                if let Some(f) = si.features.wsl_env_value(si.cursor_blink) {
                    env.push(("GHOSTTY_SHELL_FEATURES".into(), f));
                }
                Launch {
                    args: vec![
                        "-e".into(),
                        "/bin/sh".into(),
                        "-c".into(),
                        WSL_BOOTSTRAP.into(),
                    ],
                    env,
                    reset_cursor_on_submit: false,
                }
            }
            _ => untouched,
        }
    }
}

/// What [`Profile::launch`] produced: the args and environment to spawn with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The prompt hook set a bar cursor (`cursor` feature) but the shell has no
    /// pre-exec hook to undo it, so the session restores the default cursor
    /// (`CSI 0 SP q`) when it submits a command line.
    pub reset_cursor_on_submit: bool,
}

/// Ghostty `shell-integration`: which injection scheme to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellIntegration {
    /// Inject nothing — not even giest's pwsh/cmd prompt hooks.
    None,
    #[default]
    Detect,
    Bash,
    Zsh,
    Fish,
    Elvish,
    Nushell,
}

impl ShellIntegration {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "none" => Self::None,
            "detect" => Self::Detect,
            "bash" => Self::Bash,
            "zsh" => Self::Zsh,
            "fish" => Self::Fish,
            "elvish" => Self::Elvish,
            "nushell" => Self::Nushell,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Detect => "detect",
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::Elvish => "elvish",
            Self::Nushell => "nushell",
        }
    }
}

/// Ghostty `shell-integration-features`. Defaults match upstream's
/// `ShellIntegrationFeatures` (cursor, title, path on; the rest off).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellFeatures {
    pub cursor: bool,
    pub sudo: bool,
    pub title: bool,
    pub ssh_env: bool,
    pub ssh_terminfo: bool,
    pub path: bool,
}

impl Default for ShellFeatures {
    fn default() -> Self {
        Self {
            cursor: true,
            sudo: false,
            title: true,
            ssh_env: false,
            ssh_terminfo: false,
            path: true,
        }
    }
}

impl ShellFeatures {
    /// Parse a value the way upstream parses a packed struct: `true`/`false`
    /// set every flag; otherwise a comma list of `name` / `no-name`, starting
    /// from the defaults (an omitted feature keeps its *default*, not its
    /// previous value). An unknown name rejects the whole value.
    pub fn parse(v: &str) -> Option<Self> {
        let v = v.trim();
        let all = |on: bool| Self {
            cursor: on,
            sudo: on,
            title: on,
            ssh_env: on,
            ssh_terminfo: on,
            path: on,
        };
        match v.to_ascii_lowercase().as_str() {
            "true" => return Some(all(true)),
            "false" => return Some(all(false)),
            _ => {}
        }
        let mut f = Self::default();
        for part in v.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let lower = part.to_ascii_lowercase();
            let (name, on) = match lower.strip_prefix("no-") {
                Some(n) => (n.to_string(), false),
                None => (lower.clone(), true),
            };
            let slot = match name.as_str() {
                "cursor" => &mut f.cursor,
                "sudo" => &mut f.sudo,
                "title" => &mut f.title,
                "ssh-env" => &mut f.ssh_env,
                "ssh-terminfo" => &mut f.ssh_terminfo,
                "path" => &mut f.path,
                _ => return None,
            };
            *slot = on;
        }
        Some(f)
    }

    /// `GHOSTTY_SHELL_FEATURES` for the upstream scripts running under WSL, in
    /// upstream's sorted `setupFeatures` format (`cursor:blink,path,...`).
    ///
    /// `ssh-env` / `ssh-terminfo` are **withheld**: the scripts implement them
    /// by wrapping `ssh` in `"$GHOSTTY_BIN_DIR/ghostty" +ssh`, and there is no
    /// `ghostty` CLI inside WSL — enabling them would break `ssh` outright.
    /// `path` is passed but inert (it needs `GHOSTTY_BIN_DIR`, which we never
    /// set). `None` when nothing is on, as upstream omits the variable then.
    pub fn wsl_env_value(&self, cursor_blink: bool) -> Option<String> {
        let mut parts = Vec::new();
        if self.cursor {
            parts.push(if cursor_blink { "cursor:blink" } else { "cursor:steady" });
        }
        if self.path {
            parts.push("path");
        }
        if self.sudo {
            parts.push("sudo");
        }
        if self.title {
            parts.push("title");
        }
        (!parts.is_empty()).then(|| parts.join(","))
    }
}

/// Everything [`Profile::launch`] needs from the config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Integration {
    pub mode: ShellIntegration,
    pub features: ShellFeatures,
    /// `cursor-style-blink`, defaulting to on as upstream does (`orelse true`):
    /// picks the blinking (`5`) or steady (`6`) bar.
    pub cursor_blink: bool,
}

impl Default for Integration {
    fn default() -> Self {
        Self {
            mode: ShellIntegration::default(),
            features: ShellFeatures::default(),
            cursor_blink: true,
        }
    }
}

impl Integration {
    /// The integration settings `config` selects.
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            mode: config.shell_integration,
            features: config.shell_integration_features,
            cursor_blink: config.cursor_style_blink.unwrap_or(true),
        }
    }
}

/// The `sh -c` script `wsl.exe` runs. The dir arrives already translated to a
/// Linux path by WSLENV's `/p` flag, which also honours a custom automount
/// root — so giest never has to guess `/mnt/<drive>`.
const WSL_BOOTSTRAP: &str = r#"exec /bin/sh "$GIEST_SHELL_INTEGRATION_DIR/giest-wsl.sh""#;

/// Append giest's variables to an existing `WSLENV` (colon-separated,
/// `NAME[/flags]`), without duplicating any already listed.
pub fn wslenv(existing: Option<&str>) -> String {
    let mut parts: Vec<String> = existing
        .unwrap_or("")
        .split(':')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    for want in [
        "GIEST_SHELL_INTEGRATION_DIR/p",
        "GIEST_SHELL_INTEGRATION",
        "GHOSTTY_SHELL_FEATURES",
    ] {
        let name = want.split('/').next().unwrap();
        parts.retain(|p| !p.split('/').next().unwrap().eq_ignore_ascii_case(name));
        parts.push(want.to_string());
    }
    parts.join(":")
}

/// The DECSCUSR bar the `cursor` feature sets at the prompt: `5` blinking,
/// `6` steady — the same choice upstream's scripts make.
fn bar_cursor(si: &Integration) -> char {
    if si.cursor_blink { '5' } else { '6' }
}

/// The PowerShell hook with `si`'s features spliced in.
fn pwsh_hook(si: &Integration) -> String {
    let mut extra = String::new();
    if si.features.title {
        extra.push_str(PWSH_TITLE);
    }
    if si.features.cursor {
        extra.push_str(&format!(
            "  [Console]::Write(\"$([char]27)[{} q\")\n",
            bar_cursor(si)
        ));
    }
    PWSH_SHELL_HOOK.replace("  #FEATURES#\n", &extra)
}

/// `title`: the cwd, with the home dir shortened to `~` (as upstream's bash
/// `title` does at the prompt). There is no pre-exec hook, so unlike upstream
/// the running command never becomes the title.
const PWSH_TITLE: &str = r#"  if ($p) {
    $t = $p
    if ($HOME -and $t.StartsWith($HOME)) { $t = '~' + $t.Substring($HOME.Length) }
    [Console]::Write("$([char]27)]2;$t$([char]27)\")
  }
"#;

/// cmd's `prompt` string. cmd expands `$E` -> ESC (Win10+) and `$P` -> current
/// path on each render; `%COMPUTERNAME%` expands once. Emits OSC 133 D (the
/// *previous* command ended — cmd has no post-exec hook, so the next prompt is
/// the only place to say so, and it can't report an exit code there), then A
/// (prompt start), the OSC 7 cwd report (`file://HOST/C:\dir`, which the
/// session's OSC 7 parser tolerates), the optional title / bar cursor, the
/// visible `path>` prompt, then B (prompt end / input start).
fn cmd_prompt(si: &Integration) -> String {
    let mut s = String::from("prompt $E]133;D$E\\$E]133;A;cl=line$E\\$E]7;file://%COMPUTERNAME%/$P$E\\");
    if si.features.title {
        s.push_str("$E]2;$P$E\\");
    }
    if si.features.cursor {
        s.push_str(&format!("$E[{} q", bar_cursor(si)));
    }
    s.push_str("$P$G$E]133;B$E\\");
    s
}

/// PowerShell startup hook: wrap the existing `prompt` so each prompt also emits
/// an OSC 7 working-directory report and OSC 133 D/A/B semantic-prompt marks.
/// Encoded as UTF-16LE base64 and passed via `-EncodedCommand` (see
/// [`Profile::launch`]). OSC 133 A is written at the prompt's start column;
/// B is appended to the returned prompt string (where user input begins).
/// `#FEATURES#` is replaced by the enabled `shell-integration-features` lines
/// ([`pwsh_hook`]); it is also a harmless comment if left in.
///
/// `D` (the *previous* command ended, with its exit code) has to come first,
/// because a prompt function is the only post-execution hook PowerShell gives
/// us. Two consequences:
///
/// - **`$?` must be read as the very first statement.** It reflects only the
///   immediately preceding command, so any statement above it — even an
///   assignment — clobbers it and every command would report success.
/// - The exit code is `$?`-then-`$LASTEXITCODE`, not either alone: `$?` is
///   PowerShell's own success flag and is the only thing that catches a failed
///   cmdlet, while `$LASTEXITCODE` is the one that carries a *native* program's
///   real code. A prompt fires before any command has run, so the first `D` of a
///   session is unmatched; `Session` ignores a `D` with no start, which is also
///   what makes cmd's codeless `D` safe.
///
/// There is deliberately **no `C` mark**: emitting one needs a pre-execution
/// hook, which on PowerShell means overriding a PSReadLine key handler (absent
/// in plenty of setups) and on cmd is impossible. giest derives the command
/// start from the Enter keystroke it sent instead — see
/// `Session::note_command_submitted`.
const PWSH_SHELL_HOOK: &str = r#"$global:__giestPrompt = $function:prompt
function global:prompt {
  $code = if ($?) { 0 } elseif ($null -ne $global:LASTEXITCODE) { $global:LASTEXITCODE } else { 1 }
  [Console]::Write("$([char]27)]133;D;$code$([char]27)\")
  $p = (Get-Location).ProviderPath
  if ($p) {
    $u = 'file://' + [System.Net.Dns]::GetHostName() + '/' + ($p -replace '\\','/')
    [Console]::Write("$([char]27)]7;$u$([char]27)\")
  }
  [Console]::Write("$([char]27)]133;A;cl=line$([char]27)\")
  #FEATURES#
  $base = & $global:__giestPrompt
  "$base$([char]27)]133;B$([char]27)\"
}"#;

/// Ghostty's shell-integration scripts (plus giest's WSL bootstrap), embedded
/// so a bare `giest.exe` needs no install layout. Paths are relative to the
/// `shell-integration` dir; upstream license headers travel with the files.
const SI_FILES: &[(&str, &str)] = &[
    ("giest-wsl.sh", include_str!("../assets/shell-integration/giest-wsl.sh")),
    ("LICENSE-ghostty", include_str!("../assets/shell-integration/LICENSE-ghostty")),
    ("bash/ghostty.bash", include_str!("../assets/shell-integration/bash/ghostty.bash")),
    ("bash/bash-preexec.sh", include_str!("../assets/shell-integration/bash/bash-preexec.sh")),
    ("zsh/.zshenv", include_str!("../assets/shell-integration/zsh/.zshenv")),
    ("zsh/ghostty-integration", include_str!("../assets/shell-integration/zsh/ghostty-integration")),
    (
        "fish/vendor_conf.d/ghostty-shell-integration.fish",
        include_str!("../assets/shell-integration/fish/vendor_conf.d/ghostty-shell-integration.fish"),
    ),
    (
        "elvish/lib/ghostty-integration.elv",
        include_str!("../assets/shell-integration/elvish/lib/ghostty-integration.elv"),
    ),
    (
        "nushell/vendor/autoload/ghostty.nu",
        include_str!("../assets/shell-integration/nushell/vendor/autoload/ghostty.nu"),
    ),
];

/// Extract the embedded scripts under `root/shell-integration` (rewriting only
/// files whose contents differ) and return that dir. `None` on any I/O error,
/// in which case WSL simply launches without integration.
pub fn extract_shell_integration(root: &std::path::Path) -> Option<PathBuf> {
    let dir = root.join("shell-integration");
    for (rel, body) in SI_FILES {
        // Force LF even if a checkout converted the sources to CRLF: a `\r`
        // at the end of every line breaks every script under sh/bash/zsh.
        let body = body.replace("\r\n", "\n");
        let path = dir.join(rel);
        if std::fs::read(&path).ok().as_deref() == Some(body.as_bytes()) {
            continue;
        }
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, body).ok()?;
    }
    Some(dir)
}

/// [`extract_shell_integration`] into `%LOCALAPPDATA%\giest`, once per process.
pub fn shell_integration_dir() -> Option<PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let local = std::env::var_os("LOCALAPPDATA")?;
        extract_shell_integration(&PathBuf::from(local).join("giest"))
    })
    .clone()
}

/// Find `exe` on the `PATH`, returning its full path if present.
fn which(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|full| full.is_file())
}

/// Detect the available shell profiles and pick a default. PowerShell 7
/// (`pwsh`) is preferred when installed; `powershell` and `cmd` are always
/// present on Windows; WSL is offered when `wsl.exe` exists. A `config_shell`
/// override selects (or, if unknown, prepends) the default profile.
pub fn detect(config_shell: Option<&str>) -> (Vec<Profile>, usize) {
    let mut profiles = Vec::new();
    if which("pwsh.exe").is_some() {
        profiles.push(Profile::new("PowerShell", "pwsh.exe"));
    }
    profiles.push(Profile::new("Windows PowerShell", "powershell.exe"));
    profiles.push(Profile::new("Command Prompt", "cmd.exe"));
    if which("wsl.exe").is_some() {
        profiles.push(Profile::new("WSL", "wsl.exe"));
    }

    let default = match config_shell {
        Some(s) if !s.trim().is_empty() => {
            match profiles.iter().position(|p| p.matches(s)) {
                Some(i) => i,
                None => {
                    // A custom shell command from config becomes the default.
                    profiles.insert(0, Profile::new(s, s));
                    0
                }
            }
        }
        _ => 0,
    };
    (profiles, default)
}

/// The profile a `command`-style value names: a detected profile when it
/// matches one (so `initial-command = cmd` still gets cmd's prompt hooks, as
/// `command = cmd` does), else a custom profile running it verbatim.
pub fn for_command(profiles: &[Profile], cmd: &str) -> Profile {
    profiles
        .iter()
        .find(|p| p.matches(cmd))
        .cloned()
        .unwrap_or_else(|| Profile::new(cmd, cmd))
}

#[cfg(test)]
mod tests {
    #[test]
    fn for_command_prefers_a_detected_profile() {
        let (profiles, _) = detect(None);
        assert_eq!(for_command(&profiles, "cmd").program, "cmd.exe");
        let custom = for_command(&profiles, r"C:\tools\x.exe");
        assert_eq!(custom.program, r"C:\tools\x.exe");
        assert!(custom.args.is_empty());
    }

    use super::*;

    #[test]
    fn detect_lists_builtin_shells_with_a_default() {
        let (profiles, default) = detect(None);
        // powershell + cmd are always present.
        assert!(profiles.iter().any(|p| p.program == "powershell.exe"));
        assert!(profiles.iter().any(|p| p.program == "cmd.exe"));
        assert!(default < profiles.len());
    }

    #[test]
    fn config_override_selects_existing_profile() {
        let (profiles, default) = detect(Some("cmd"));
        assert!(profiles[default].matches("cmd.exe"));
    }

    #[test]
    fn unknown_config_shell_is_prepended_as_default() {
        let (profiles, default) = detect(Some("C:\\msys64\\usr\\bin\\bash.exe"));
        assert_eq!(default, 0);
        assert_eq!(profiles[0].program, "C:\\msys64\\usr\\bin\\bash.exe");
    }

    #[test]
    fn shell_hooks_emit_osc133_prompt_marks() {
        // The PowerShell hook wraps the prompt with OSC 133 A (start) and B (end).
        assert!(PWSH_SHELL_HOOK.contains("]133;A"));
        assert!(PWSH_SHELL_HOOK.contains("]133;B"));
        // The cmd prompt argument carries the same marks (alongside OSC 7).
        let args = Profile::new("Command Prompt", "cmd.exe").launch_args();
        let prompt = args.iter().find(|a| a.contains("prompt")).expect("cmd prompt arg");
        assert!(prompt.contains("]133;A"), "cmd prompt missing OSC 133 A: {prompt}");
        assert!(prompt.contains("]133;B"), "cmd prompt missing OSC 133 B: {prompt}");
        assert!(prompt.contains("]7;"), "cmd prompt should still emit OSC 7");
        // Both opt in to `cursor-click-to-move` (line editors that take arrows).
        assert!(PWSH_SHELL_HOOK.contains("]133;A;cl=line"));
        assert!(prompt.contains("]133;A;cl=line"));
    }

    #[test]
    fn shell_hooks_emit_the_osc133_command_end_mark() {
        // `D` is what `notify-on-command-finish` fires on. PowerShell can carry
        // an exit code; cmd cannot.
        assert!(PWSH_SHELL_HOOK.contains("]133;D;$code"));
        let args = Profile::new("Command Prompt", "cmd.exe").launch_args();
        let prompt = args.iter().find(|a| a.contains("prompt")).expect("cmd prompt arg");
        assert!(prompt.contains("]133;D"), "cmd prompt missing OSC 133 D: {prompt}");
    }

    #[test]
    fn the_pwsh_hook_reads_the_success_flag_first() {
        // `$?` reflects only the immediately preceding command, so *any*
        // statement above it resets it and every command would report success.
        // Pin the ordering rather than the exact text.
        let body = PWSH_SHELL_HOOK
            .split_once("function global:prompt {")
            .expect("prompt function")
            .1;
        let first = body
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .expect("a first statement");
        assert!(
            first.contains("$?"),
            "the success flag must be read first, but the first statement is: {first}"
        );
        // And the exit code must consult *both* signals — `$?` alone misses a
        // native program's real code, `$LASTEXITCODE` alone misses a failed
        // cmdlet.
        assert!(first.contains("LASTEXITCODE"), "{first}");
    }

    #[test]
    fn the_pwsh_hook_marks_a_full_command_cycle() {
        // D (previous command ended) must precede A (this prompt starts).
        let d = PWSH_SHELL_HOOK.find("]133;D").expect("D mark");
        let a = PWSH_SHELL_HOOK.find("]133;A").expect("A mark");
        let b = PWSH_SHELL_HOOK.find("]133;B").expect("B mark");
        assert!(d < a && a < b, "marks out of order: D={d} A={a} B={b}");
    }

    fn si(f: impl FnOnce(&mut Integration)) -> Integration {
        let mut i = Integration::default();
        f(&mut i);
        i
    }

    /// Decode a pwsh `-EncodedCommand` launch back to the hook text.
    fn decoded_hook(l: &Launch) -> String {
        let bytes = STANDARD.decode(&l.args[2]).unwrap();
        let utf16: Vec<u16> = bytes.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16(&utf16).unwrap()
    }

    #[test]
    fn none_disables_every_injected_hook() {
        let off = si(|i| i.mode = ShellIntegration::None);
        let dir = std::path::Path::new(r"C:\si");
        for prog in ["pwsh.exe", "powershell.exe", "cmd.exe", "wsl.exe"] {
            let l = Profile::new("x", prog).launch(&off, Some(dir), &[]);
            assert!(l.args.is_empty(), "{prog} still got args: {:?}", l.args);
            assert!(l.env.is_empty(), "{prog} still got env: {:?}", l.env);
            assert!(!l.reset_cursor_on_submit);
        }
    }

    #[test]
    fn the_featured_pwsh_hook_still_reads_the_success_flag_first() {
        // The `$?`-first invariant must hold for the hook as actually shipped,
        // with every feature spliced in, not just the template.
        let all = si(|i| i.features = ShellFeatures::parse("true").unwrap());
        let l = Profile::new("p", "pwsh.exe").launch(&all, None, &[]);
        let hook = decoded_hook(&l);
        let first = hook
            .split_once("function global:prompt {")
            .unwrap()
            .1
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap();
        assert!(first.starts_with("$code = if ($?)"), "{first}");
        assert!(!hook.contains("#FEATURES#"));
    }

    #[test]
    fn pwsh_features_cursor_and_title() {
        let l = Profile::new("p", "pwsh.exe").launch(&Integration::default(), None, &[]);
        let hook = decoded_hook(&l);
        assert!(hook.contains("[5 q"), "default blink bar");
        assert!(hook.contains("]2;$t"), "title at prompt");
        assert!(l.reset_cursor_on_submit);

        let steady = si(|i| i.cursor_blink = false);
        let hook = decoded_hook(&Profile::new("p", "pwsh.exe").launch(&steady, None, &[]));
        assert!(hook.contains("[6 q"));

        let bare = si(|i| i.features = ShellFeatures::parse("no-cursor,no-title").unwrap());
        let l = Profile::new("p", "powershell.exe").launch(&bare, None, &[]);
        let hook = decoded_hook(&l);
        assert!(!hook.contains(" q\")") && !hook.contains("]2;"));
        assert!(hook.contains("]133;A") && hook.contains("]7;"), "marks stay on");
        assert!(!l.reset_cursor_on_submit);
    }

    #[test]
    fn cmd_features_cursor_and_title() {
        let l = Profile::new("c", "cmd.exe").launch(&Integration::default(), None, &[]);
        assert!(l.args[1].contains("$E]2;$P$E\\") && l.args[1].contains("$E[5 q"));
        assert!(l.args[1].ends_with("$P$G$E]133;B$E\\"));
        let bare = si(|i| i.features = ShellFeatures::parse("false").unwrap());
        let l = Profile::new("c", "cmd.exe").launch(&bare, None, &[]);
        assert!(!l.args[1].contains("]2;") && !l.args[1].contains(" q"));
    }

    #[test]
    fn wsl_runs_the_bootstrap_with_translated_env() {
        let dir = std::path::Path::new(r"C:\Users\me\AppData\Local\giest\shell-integration");
        let base = vec![("FOO".to_string(), "1".to_string())];
        let l = Profile::new("WSL", "wsl.exe").launch(
            &si(|i| i.mode = ShellIntegration::Zsh),
            Some(dir),
            &base,
        );
        assert_eq!(l.args[..3], ["-e", "/bin/sh", "-c"]);
        assert!(l.args[3].contains("$GIEST_SHELL_INTEGRATION_DIR/giest-wsl.sh"));
        let get = |k: &str| l.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("FOO"), Some("1"));
        assert_eq!(get("GIEST_SHELL_INTEGRATION"), Some("zsh"));
        assert_eq!(get("GIEST_SHELL_INTEGRATION_DIR"), Some(dir.to_str().unwrap()));
        assert_eq!(get("GHOSTTY_SHELL_FEATURES"), Some("cursor:blink,path,title"));
        let wslenv = get("WSLENV").unwrap();
        assert!(wslenv.split(':').any(|p| p == "GIEST_SHELL_INTEGRATION_DIR/p"), "{wslenv}");
        assert!(!l.reset_cursor_on_submit, "the upstream scripts reset it themselves");

        // No extracted dir: launch plainly rather than half-integrated.
        let plain = Profile::new("WSL", "wsl.exe").launch(&Integration::default(), None, &[]);
        assert!(plain.args.is_empty());
        // A WSL profile that already carries args is left alone.
        let mut custom = Profile::new("WSL", "wsl.exe");
        custom.args = vec!["-d".into(), "Ubuntu".into()];
        assert_eq!(custom.launch(&Integration::default(), Some(dir), &[]).args, custom.args);
    }

    #[test]
    fn wslenv_merges_without_duplicates() {
        assert_eq!(
            wslenv(None),
            "GIEST_SHELL_INTEGRATION_DIR/p:GIEST_SHELL_INTEGRATION:GHOSTTY_SHELL_FEATURES"
        );
        assert_eq!(
            wslenv(Some("USERPROFILE/p:GIEST_SHELL_INTEGRATION_DIR/u:")),
            "USERPROFILE/p:GIEST_SHELL_INTEGRATION_DIR/p:GIEST_SHELL_INTEGRATION:GHOSTTY_SHELL_FEATURES"
        );
    }

    #[test]
    fn wsl_features_never_enable_the_ssh_wrapper() {
        // The upstream ssh wrapper calls `$GHOSTTY_BIN_DIR/ghostty +ssh`, which
        // does not exist inside WSL — passing ssh-* would break `ssh` outright.
        let f = ShellFeatures::parse("true").unwrap();
        assert_eq!(f.wsl_env_value(false).unwrap(), "cursor:steady,path,sudo,title");
        assert_eq!(ShellFeatures::parse("false").unwrap().wsl_env_value(true), None);
    }

    #[test]
    fn extraction_writes_lf_scripts_once() {
        let root = std::env::temp_dir().join(format!("giest-si-{}", std::process::id()));
        let dir = extract_shell_integration(&root).unwrap();
        for rel in ["giest-wsl.sh", "bash/ghostty.bash", "zsh/.zshenv"] {
            let body = std::fs::read(dir.join(rel)).unwrap();
            assert!(!body.contains(&b'\r'), "{rel} has CR — it would break under sh");
        }
        let bash = std::fs::read_to_string(dir.join("bash/ghostty.bash")).unwrap();
        assert!(bash.contains("GNU General Public License"), "license header kept");
        // Idempotent.
        assert_eq!(extract_shell_integration(&root), Some(dir));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn matches_is_case_and_exe_insensitive() {
        let p = Profile::new("Command Prompt", "cmd.exe");
        assert!(p.matches("CMD"));
        assert!(p.matches("cmd.exe"));
        assert!(p.matches("command prompt"));
        assert!(!p.matches("pwsh"));
    }
}
