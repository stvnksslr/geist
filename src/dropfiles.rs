//! Files dropped onto a pane become shell-quoted paths typed at the prompt
//! (the macOS app's `SurfaceView` drag-and-drop). The quoting has to match the
//! shell actually running, which on Windows is three different languages:
//! PowerShell, cmd, and a POSIX shell inside WSL — where a `C:\` path is also
//! meaningless until it is rewritten to `/mnt/c/`.
//!
//! The text is sent through `Session::paste_str`, so `clipboard-paste-*`
//! protection and bracketed paste apply exactly as for a clipboard paste.

use std::path::Path;

/// Which quoting language the pane's shell speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellKind {
    PowerShell,
    Cmd,
    /// A Linux shell under WSL: POSIX quoting *and* `/mnt/<drive>/` paths.
    Wsl,
    /// A POSIX shell on Windows (Git Bash, MSYS2): POSIX quoting, Windows
    /// paths with forward slashes, which those shells accept.
    Posix,
}

impl ShellKind {
    /// Classify a profile's program. Unknown programs get PowerShell quoting —
    /// its single quotes are also inert in most other Windows shells, and
    /// PowerShell is the default profile.
    pub fn from_program(program: &str) -> Self {
        let lower = program.to_ascii_lowercase();
        let base = lower.rsplit(['\\', '/']).next().unwrap_or(&lower);
        let base = base.trim_end_matches(".exe");
        match base {
            "cmd" => Self::Cmd,
            "wsl" | "bash" if lower.contains("system32") || base == "wsl" => Self::Wsl,
            b if b.starts_with("ubuntu") || b.starts_with("debian") || b == "kali" => Self::Wsl,
            "bash" | "sh" | "zsh" | "fish" | "dash" => Self::Posix,
            _ => Self::PowerShell,
        }
    }
}

/// `C:\Users\me` -> `/mnt/c/Users/me`, and a `\\wsl$\<distro>\...` or
/// `\\wsl.localhost\<distro>\...` share (a file dragged out of the distro's
/// own filesystem) back to its Linux path. Anything else keeps its text with
/// forward slashes.
pub fn wsl_path(p: &str) -> String {
    let lower = p.to_ascii_lowercase();
    for prefix in [r"\\wsl$\", r"\\wsl.localhost\"] {
        if lower.starts_with(prefix) {
            let rest = &p[prefix.len()..];
            // Drop the distro name; what follows is rooted at `/`.
            let after = rest.find('\\').map_or("", |i| &rest[i..]);
            let path = after.replace('\\', "/");
            return if path.is_empty() { "/".into() } else { path };
        }
    }
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let drive = (b[0] as char).to_ascii_lowercase();
        let rest = p[2..].replace('\\', "/");
        let rest = rest.trim_start_matches('/');
        return if rest.is_empty() {
            format!("/mnt/{drive}")
        } else {
            format!("/mnt/{drive}/{rest}")
        };
    }
    p.replace('\\', "/")
}

fn posix_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:,@%=".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn pwsh_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || r"\/._-:+=".contains(c));
    if plain {
        s.to_string()
    } else {
        // Inside single quotes only `'` is special, and it is doubled.
        format!("'{}'", s.replace('\'', "''"))
    }
}

fn cmd_quote(s: &str) -> String {
    // `"` cannot occur in a Windows path, so wrapping is enough; `%` would
    // still expand inside quotes but is legal in paths and cannot be escaped
    // on an interactive cmd line, so it is left alone (as Explorer's own
    // drag-to-console does).
    let needs = s.is_empty()
        || s.chars()
            .any(|c| c.is_whitespace() || "&()[]{}^=;!'+,`~<>|".contains(c));
    if needs {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}

/// The text to type for `paths` dropped onto a `kind` shell: each path quoted
/// for that shell, space-separated, with a trailing space so the next word
/// the user types is a new argument (Ghostty does the same).
pub fn quote_paths<P: AsRef<Path>>(kind: ShellKind, paths: &[P]) -> String {
    let mut out = String::new();
    for p in paths {
        let raw = p.as_ref().to_string_lossy();
        let quoted = match kind {
            ShellKind::PowerShell => pwsh_quote(&raw),
            ShellKind::Cmd => cmd_quote(&raw),
            ShellKind::Wsl => posix_quote(&wsl_path(&raw)),
            ShellKind::Posix => posix_quote(&raw.replace('\\', "/")),
        };
        out.push_str(&quoted);
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_classify() {
        assert_eq!(ShellKind::from_program("pwsh.exe"), ShellKind::PowerShell);
        assert_eq!(ShellKind::from_program("powershell"), ShellKind::PowerShell);
        assert_eq!(
            ShellKind::from_program(r"C:\Windows\System32\cmd.exe"),
            ShellKind::Cmd
        );
        assert_eq!(ShellKind::from_program("wsl.exe"), ShellKind::Wsl);
        assert_eq!(
            ShellKind::from_program(r"C:\Windows\System32\bash.exe"),
            ShellKind::Wsl
        );
        assert_eq!(ShellKind::from_program("ubuntu2204.exe"), ShellKind::Wsl);
        assert_eq!(
            ShellKind::from_program(r"C:\Program Files\Git\bin\bash.exe"),
            ShellKind::Posix
        );
        assert_eq!(ShellKind::from_program("nu"), ShellKind::PowerShell);
    }

    #[test]
    fn wsl_paths() {
        assert_eq!(wsl_path(r"C:\Users\me\a b.txt"), "/mnt/c/Users/me/a b.txt");
        assert_eq!(wsl_path(r"d:\"), "/mnt/d");
        assert_eq!(wsl_path(r"\\wsl$\Ubuntu\home\me\x"), "/home/me/x");
        assert_eq!(wsl_path(r"\\wsl.localhost\Debian\etc"), "/etc");
        assert_eq!(wsl_path(r"\\server\share\f"), "//server/share/f");
    }

    #[test]
    fn powershell_quoting() {
        assert_eq!(
            quote_paths(ShellKind::PowerShell, &[r"C:\tmp\a.txt"]),
            r"C:\tmp\a.txt "
        );
        assert_eq!(
            quote_paths(ShellKind::PowerShell, &[r"C:\My Files\it's.txt", r"C:\x"]),
            r"'C:\My Files\it''s.txt' C:\x "
        );
        // `$` and backtick would expand or escape unquoted.
        assert_eq!(
            quote_paths(ShellKind::PowerShell, &[r"C:\$x`y"]),
            r"'C:\$x`y' "
        );
    }

    #[test]
    fn cmd_quoting() {
        assert_eq!(
            quote_paths(ShellKind::Cmd, &[r"C:\tmp\a.txt"]),
            r"C:\tmp\a.txt "
        );
        assert_eq!(
            quote_paths(ShellKind::Cmd, &[r"C:\My Files\a&b"]),
            r#""C:\My Files\a&b" "#
        );
    }

    #[test]
    fn wsl_and_posix_quoting() {
        assert_eq!(
            quote_paths(ShellKind::Wsl, &[r"C:\Users\me\it's here.txt"]),
            r"'/mnt/c/Users/me/it'\''s here.txt' "
        );
        assert_eq!(
            quote_paths(ShellKind::Wsl, &[r"C:\src\main.rs"]),
            "/mnt/c/src/main.rs "
        );
        assert_eq!(
            quote_paths(ShellKind::Posix, &[r"C:\src\a b"]),
            "'C:/src/a b' "
        );
    }
}
