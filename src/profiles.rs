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
    /// report its working directory via OSC 7 (so a new split can open in the
    /// parent pane's directory) and mark its prompt via OSC 133 A/B (so
    /// `jump_to_prompt` can navigate between prompts). Shells we don't recognise
    /// (WSL, a user's custom command, or any profile that already carries args)
    /// are spawned untouched.
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
                let utf16: Vec<u8> = PWSH_SHELL_HOOK
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect();
                let encoded = STANDARD.encode(utf16);
                vec!["-NoExit".into(), "-EncodedCommand".into(), encoded]
            }
            // cmd expands $E -> ESC (Win10+) and $P -> current path each render;
            // %COMPUTERNAME% expands once. Emits OSC 133 D (the *previous*
            // command ended — cmd has no post-exec hook, so the next prompt is
            // the only place to say so, and it can't report an exit code there),
            // then A (prompt start), the OSC 7 cwd report (`file://HOST/C:\dir`,
            // which the session's OSC 7 parser tolerates), the visible `path>`
            // prompt, then B (prompt end / input start).
            "cmd" => vec![
                "/K".into(),
                "prompt $E]133;D$E\\$E]133;A$E\\$E]7;file://%COMPUTERNAME%/$P$E\\$P$G$E]133;B$E\\"
                    .into(),
            ],
            _ => Vec::new(),
        }
    }
}

/// PowerShell startup hook: wrap the existing `prompt` so each prompt also emits
/// an OSC 7 working-directory report and OSC 133 D/A/B semantic-prompt marks.
/// Encoded as UTF-16LE base64 and passed via `-EncodedCommand` (see
/// [`Profile::launch_args`]). OSC 133 A is written at the prompt's start column;
/// B is appended to the returned prompt string (where user input begins).
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
  [Console]::Write("$([char]27)]133;A$([char]27)\")
  $base = & $global:__giestPrompt
  "$base$([char]27)]133;B$([char]27)\"
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

    #[test]
    fn matches_is_case_and_exe_insensitive() {
        let p = Profile::new("Command Prompt", "cmd.exe");
        assert!(p.matches("CMD"));
        assert!(p.matches("cmd.exe"));
        assert!(p.matches("command prompt"));
        assert!(!p.matches("pwsh"));
    }
}
