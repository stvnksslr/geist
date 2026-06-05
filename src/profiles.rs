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

    /// The full arg list to spawn this shell with. For the built-in PowerShell
    /// and cmd profiles we prepend a small startup hook that makes the shell
    /// report its working directory via OSC 7 — so a new split can open in the
    /// parent pane's directory. Shells we don't recognise (WSL, a user's custom
    /// command, or any profile that already carries args) are spawned untouched.
    pub fn launch_args(&self) -> Vec<String> {
        if !self.args.is_empty() {
            return self.args.clone();
        }
        let prog = self.program.trim_end_matches(".exe").to_ascii_lowercase();
        let prog = prog.rsplit(['\\', '/']).next().unwrap_or(&prog);
        match prog {
            "pwsh" | "powershell" => {
                // `-EncodedCommand` runs after the user's $PROFILE loads, so the
                // hook wraps their customised prompt; `-NoExit` keeps it interactive.
                let utf16: Vec<u8> = PWSH_OSC7_HOOK
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect();
                let encoded = STANDARD.encode(utf16);
                vec!["-NoExit".into(), "-EncodedCommand".into(), encoded]
            }
            // cmd expands $E -> ESC (Win10+) and $P -> current path each render;
            // %COMPUTERNAME% expands once. Yields `file://HOST/C:\dir`, which the
            // OSC 7 parser in `session.rs` tolerates (backslashes included).
            "cmd" => vec![
                "/K".into(),
                "prompt $E]7;file://%COMPUTERNAME%/$P$E\\$P$G".into(),
            ],
            _ => Vec::new(),
        }
    }
}

/// PowerShell startup hook: wrap the existing `prompt` to also emit an OSC 7
/// working-directory report before rendering. Encoded as UTF-16LE base64 and
/// passed via `-EncodedCommand` (see [`Profile::launch_args`]).
const PWSH_OSC7_HOOK: &str = r#"$global:__giestPrompt = $function:prompt
function global:prompt {
  $p = (Get-Location).ProviderPath
  if ($p) {
    $u = 'file://' + [System.Net.Dns]::GetHostName() + '/' + ($p -replace '\\','/')
    [Console]::Write("$([char]27)]7;$u$([char]27)\")
  }
  & $global:__giestPrompt
}"#;

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

#[cfg(test)]
mod tests {
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
    fn matches_is_case_and_exe_insensitive() {
        let p = Profile::new("Command Prompt", "cmd.exe");
        assert!(p.matches("CMD"));
        assert!(p.matches("cmd.exe"));
        assert!(p.matches("command prompt"));
        assert!(!p.matches("pwsh"));
    }
}
