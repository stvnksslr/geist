//! Shell profiles: the selectable shells a tab/pane can run (PowerShell 7,
//! Windows PowerShell, Command Prompt, WSL, or a user-configured command),
//! Windows-Terminal style. Detected at startup from what's installed.

use std::path::PathBuf;

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
