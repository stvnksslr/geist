//! Command-line arguments — Ghostty's CLI surface, shaped for Windows.
//!
//! ```text
//! giest [<dir>] [--<config-key>=<value>...] [-e <program> [args...]]
//! giest +new-window [--working-directory=<dir>] [--command=<cmd>] [-e <program> [args...]]
//! giest +new-tab    [--working-directory=<dir>] [--command=<cmd>] [-e <program> [args...]]
//! giest +list | +focus | +action=<keybind action> | +input=<text>
//! giest +register-shell-integration | +unregister-shell-integration
//! giest --help | --version
//! ```
//!
//! Upstream semantics that are easy to get wrong, all kept:
//!
//! - **`-e` swallows everything after it** as the command's argv, including
//!   things that look like flags (`giest -e pwsh -NoLogo`). It sets the *initial*
//!   command only — later tabs and splits still run `command` — and it implies a
//!   standalone instance (upstream sets `gtk-single-instance = false`): a
//!   one-off command window must not be folded into a running terminal.
//! - **`--<key>=<value>` is a config override**, applied *after* every config
//!   file (and re-applied on a config reload), exactly as upstream replays its
//!   CLI args. `--key` with no value is `key = true`, which is what the
//!   upstream parser does for a bare boolean flag. `--config-file=<path>` is
//!   just one more such key, resolved against the invoking directory.
//! - **`+new-window` / `+new-tab` forward to the running instance.** When none
//!   is running they fall back to starting one, which is the D-Bus-activation
//!   behaviour upstream documents for GTK.
//!
//! The positional `<dir>` is a Windows addition (it is what Explorer's "Open
//! giest here" passes). A *file* opens in its parent directory, mirroring the
//! macOS app's `CommandLineOpenFileFilter`, which opens dropped files' folders.

use std::path::{Path, PathBuf};

use crate::ipc::{CommandSpec, Request};

/// What the invocation asks for, before deciding *where* it runs.
#[derive(Clone, Debug, PartialEq)]
pub enum Verb {
    /// Plain `giest`: start a terminal (or, with an instance running, open a
    /// window/tab in it).
    Launch,
    NewWindow,
    NewTab,
    /// `+list`: print the running instance's windows/tabs/panes as JSON.
    List,
    /// `+focus`: bring the running instance's last-used window forward.
    Focus,
    /// `+action=<name>`: run a keybind action in the running instance.
    Action(String),
    /// `+input=<text>`: paste `text` into the focused pane (through the paste
    /// gate, so `clipboard-paste-protection` still applies).
    Input(String),
    Help,
    Version,
    RegisterShellIntegration,
    UnregisterShellIntegration,
    /// `+register-default-terminal`: make giest the Windows default terminal
    /// (HKCU; see `handoff.rs`).
    RegisterDefaultTerminal,
    UnregisterDefaultTerminal,
}

/// A parsed command line.
#[derive(Clone, Debug, PartialEq)]
pub struct Cli {
    pub verb: Verb,
    /// The directory to start in: a positional `<dir>` or
    /// `--working-directory`, already absolute.
    pub cwd: Option<PathBuf>,
    /// Whether `cwd` came from the positional argument (Explorer's form), which
    /// forwards as a new *tab* rather than a new window.
    pub positional_dir: bool,
    /// `-e <argv...>` or, for the `+new-*` verbs, `--command=<cmd>`.
    pub command: Option<CommandSpec>,
    /// `--<key>=<value>` config overrides, in order.
    pub overrides: Vec<(String, String)>,
    /// `--restore-session`: restore the saved layout once regardless of
    /// `window-save-state`. What `RegisterApplicationRestart` relaunches with.
    pub restore_session: bool,
    /// Problems found while parsing; reported and fatal for the forwarding
    /// verbs, since sending a half-understood request is worse than none.
    pub errors: Vec<String>,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            verb: Verb::Launch,
            cwd: None,
            positional_dir: false,
            command: None,
            overrides: Vec::new(),
            restore_session: false,
            errors: Vec::new(),
        }
    }
}

/// The version string `--version` prints.
pub fn version_string() -> String {
    format!("giest {}", env!("CARGO_PKG_VERSION"))
}

pub const HELP: &str = "\
Usage: giest [<dir>] [--<config-key>=<value>...] [-e <program> [args...]]
       giest +new-window | +new-tab [--working-directory=<dir>] [--command=<cmd>] [-e ...]
       giest +list | +focus | +action=<action> | +input=<text>
       giest +register-shell-integration | +unregister-shell-integration
       giest +register-default-terminal | +unregister-default-terminal

  <dir>                     Start in this directory (a file opens its folder).
  --working-directory=<dir> Same, as a config key (also 'home' / 'inherit').
  -e <program> [args...]    Run this command in the first pane. Everything
                            after -e is the command. Starts a separate instance.
  --<key>=<value>           Override any config key (after the config files).
  --config-file=<path>      Load an extra config file.
  --restore-session         Reopen the last saved window layout once.

  +new-window / +new-tab    Open in the running giest (or start one).
  +list                     Print the running instance's windows as JSON.
  +focus                    Bring the running instance to the front.
  +action=<action>          Run a keybind action (e.g. +action=new_split:right).
  +input=<text>             Paste text into the focused pane.
  +register-shell-integration    Add \"Open giest here\" to Explorer (per user).
  +unregister-shell-integration  Remove it again.
  +register-default-terminal     Make giest the Windows default terminal (per
                                 user; needs Windows Terminal's OpenConsole).
  +unregister-default-terminal   Restore the previous default exactly.

  -h, --help                Show this help.
  --version                 Show the version.

A second launch hands its request to the running instance over a per-user
named pipe and exits; set single-instance = false to always start a new one.";

/// Expand a leading `~` against `home`.
fn expand_home(v: &str, home: Option<&Path>) -> PathBuf {
    if v == "~" {
        if let Some(h) = home {
            return h.to_path_buf();
        }
    }
    if let Some(rest) = v.strip_prefix("~/").or_else(|| v.strip_prefix(r"~\")) {
        if let Some(h) = home {
            return h.join(rest);
        }
    }
    PathBuf::from(v)
}

/// Undo the one quoting accident Explorer's verbs produce: `"%V"` on a drive
/// root is `"C:\"`, and under the MSVC argv rules the `\"` is an escaped quote —
/// so the program receives `C:"`. Strip stray quotes, and put back the
/// backslash a bare drive letter needs to mean the root rather than "the
/// current directory on that drive".
pub fn clean_path_arg(v: &str) -> String {
    let t = v.trim().trim_matches('"').trim();
    let b = t.as_bytes();
    if b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return format!("{t}\\");
    }
    t.to_string()
}

/// Make `p` absolute against `cwd` (no filesystem access, so it is testable and
/// keeps `..` for the OS to resolve).
fn absolutize(p: PathBuf, cwd: &Path) -> PathBuf {
    if p.is_absolute() { p } else { cwd.join(p) }
}

/// Parse `args` (without the program name). `cwd` resolves relative paths;
/// `home` expands `~`; `is_file` decides whether a positional path names a file
/// (whose folder is used) — injected so the parser has no filesystem access
/// and the tests don't need real files.
pub fn parse(
    args: &[String],
    cwd: &Path,
    home: Option<&Path>,
    is_file: &dyn Fn(&Path) -> bool,
) -> Cli {
    let mut cli = Cli::default();
    let mut it = args.iter().peekable();

    // A `+verb` is only recognised first, as upstream does.
    if let Some(first) = it.peek() {
        if let Some(verb) = first.strip_prefix('+') {
            let (name, value) = match verb.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (verb, None),
            };
            cli.verb = match (name, value) {
                ("new-window", None) => Verb::NewWindow,
                ("new-tab", None) => Verb::NewTab,
                ("list", None) => Verb::List,
                ("focus", None) => Verb::Focus,
                ("action", Some(a)) if !a.trim().is_empty() => Verb::Action(a.trim().to_string()),
                ("input", Some(t)) => Verb::Input(t),
                ("help", None) => Verb::Help,
                ("version", None) => Verb::Version,
                ("register-shell-integration", None) => Verb::RegisterShellIntegration,
                ("unregister-shell-integration", None) => Verb::UnregisterShellIntegration,
                ("register-default-terminal", None) => Verb::RegisterDefaultTerminal,
                ("unregister-default-terminal", None) => Verb::UnregisterDefaultTerminal,
                _ => {
                    cli.errors
                        .push(format!("unknown or malformed action '{first}'"));
                    Verb::Help
                }
            };
            it.next();
        }
    }

    let forwarding = matches!(cli.verb, Verb::NewWindow | Verb::NewTab);
    while let Some(arg) = it.next() {
        if arg == "-e" {
            let argv: Vec<String> = it.by_ref().cloned().collect();
            if argv.is_empty() {
                cli.errors.push("missing command after -e".into());
            } else {
                cli.command = Some(CommandSpec::Argv(argv));
            }
            break;
        }
        if arg == "-h" || arg == "--help" {
            cli.verb = Verb::Help;
            continue;
        }
        if arg == "--version" {
            cli.verb = Verb::Version;
            continue;
        }
        if arg == "--restore-session" {
            cli.restore_session = true;
            continue;
        }
        if let Some(flag) = arg.strip_prefix("--") {
            let (key, value) = match flag.split_once('=') {
                Some((k, v)) => (k.trim(), v.to_string()),
                None => (flag.trim(), "true".to_string()),
            };
            if key.is_empty() {
                cli.errors.push(format!("malformed flag '{arg}'"));
                continue;
            }
            if value.contains(['\r', '\n']) {
                cli.errors
                    .push(format!("--{key}: value must be a single line"));
                continue;
            }
            match key {
                "working-directory" => {
                    let v = clean_path_arg(&value);
                    match v.as_str() {
                        // Keywords stay keywords: they mean something to the
                        // config, not to the filesystem.
                        "home" | "inherit" | "" => cli.overrides.push((key.into(), v)),
                        _ => {
                            cli.cwd = Some(absolutize(expand_home(&v, home), cwd));
                        }
                    }
                }
                "config-file" => {
                    // Resolved against the *invoking* directory, not the
                    // config dir: that is what a user typing a relative path
                    // on a command line means. A `?` prefix is kept.
                    let (opt, p) = match value.strip_prefix('?') {
                        Some(p) => ("?", p),
                        None => ("", value.as_str()),
                    };
                    let p = absolutize(expand_home(&clean_path_arg(p), home), cwd);
                    cli.overrides
                        .push((key.into(), format!("{opt}{}", p.display())));
                }
                "command" if forwarding => {
                    if !value.trim().is_empty() {
                        cli.command = Some(CommandSpec::Line(value.trim().to_string()));
                    }
                }
                _ => cli.overrides.push((key.to_string(), value)),
            }
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            cli.errors.push(format!("unknown option '{arg}'"));
            continue;
        }
        // Positional: the directory to open.
        if cli.cwd.is_some() {
            cli.errors.push(format!("unexpected argument '{arg}'"));
            continue;
        }
        let p = absolutize(expand_home(&clean_path_arg(arg), home), cwd);
        let p = if is_file(&p) {
            p.parent().map(Path::to_path_buf).unwrap_or(p)
        } else {
            p
        };
        cli.cwd = Some(p);
        cli.positional_dir = true;
    }
    cli
}

/// Parse the real process arguments.
pub fn from_env() -> Cli {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let cwd = std::env::current_dir().unwrap_or_default();
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    parse(&args, &cwd, home.as_deref(), &|p| p.is_file())
}

/// Where an invocation runs.
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// Hand this request to a running instance; if none answers, start one.
    Forward(Request),
    /// Only meaningful with a running instance (`+list`, `+action`, …).
    ForwardOnly(Request),
    /// Start a terminal in this process. `serve` says whether it may become the
    /// single instance others forward to.
    Local { serve: bool },
}

impl Cli {
    /// Decide where this invocation runs. `single_instance` is the config's
    /// `single-instance` (after this command line's own overrides).
    pub fn plan(&self, single_instance: bool) -> Plan {
        self.plan_with(single_instance, crate::config::DropBehavior::NewTab)
    }

    /// [`Self::plan`] with the configured `macos-dock-drop-behavior`, which
    /// decides what a positional path opens in the running instance.
    pub fn plan_with(&self, single_instance: bool, drop: crate::config::DropBehavior) -> Plan {
        let cwd = self.cwd.as_ref().map(|p| p.display().to_string());
        match &self.verb {
            Verb::NewWindow => Plan::Forward(Request::NewWindow {
                cwd,
                command: self.command.clone(),
            }),
            Verb::NewTab => Plan::Forward(Request::NewTab {
                cwd,
                command: self.command.clone(),
                window: None,
            }),
            Verb::List => Plan::ForwardOnly(Request::List),
            Verb::Focus => Plan::ForwardOnly(Request::Focus {
                window: None,
                tab: None,
                pane: None,
            }),
            Verb::Action(a) => Plan::ForwardOnly(Request::RunAction {
                action: a.clone(),
                window: None,
            }),
            Verb::Input(t) => Plan::ForwardOnly(Request::InputText {
                text: t.clone(),
                window: None,
                pane: None,
            }),
            Verb::Launch => {
                // `-e` implies a standalone instance (upstream), and so do
                // config overrides: they can't be applied to a process that
                // has already loaded its config, and silently dropping them
                // would be the worst outcome. A restart relaunch restores its
                // own layout and so never forwards either.
                let standalone = matches!(self.command, Some(CommandSpec::Argv(_)))
                    || !self.overrides.is_empty()
                    || self.restore_session;
                if !single_instance || standalone {
                    return Plan::Local {
                        serve: single_instance
                            && !matches!(self.command, Some(CommandSpec::Argv(_))),
                    };
                }
                // Explorer's "Open giest here" (a positional dir) is a new tab
                // in the window you already have; a bare relaunch is a new
                // window, like clicking a pinned taskbar icon.
                if self.positional_dir && drop == crate::config::DropBehavior::NewTab {
                    Plan::Forward(Request::NewTab {
                        cwd,
                        command: None,
                        window: None,
                    })
                } else {
                    Plan::Forward(Request::NewWindow { cwd, command: None })
                }
            }
            Verb::Help
            | Verb::Version
            | Verb::RegisterShellIntegration
            | Verb::UnregisterShellIntegration
            | Verb::RegisterDefaultTerminal
            | Verb::UnregisterDefaultTerminal => Plan::Local { serve: false },
        }
    }

    /// The config body this command line layers over the config files, in
    /// `key = value` lines. A local start also folds `cwd` in as
    /// `working-directory` and a `--command` as `initial-command`, so the one
    /// config path handles both.
    pub fn override_body(&self) -> String {
        let mut out = String::new();
        for (k, v) in &self.overrides {
            out.push_str(&format!("{k} = {v}\n"));
        }
        if let Some(cwd) = &self.cwd {
            out.push_str(&format!("working-directory = {}\n", cwd.display()));
        }
        if let Some(CommandSpec::Line(c)) = &self.command {
            out.push_str(&format!("initial-command = {c}\n"));
        }
        out
    }

    /// The `-e` argv, if one was given.
    pub fn initial_argv(&self) -> Option<&[String]> {
        match &self.command {
            Some(CommandSpec::Argv(v)) => Some(v),
            _ => None,
        }
    }
}

static INITIAL_ARGV: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
static RESTORE_SESSION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record the process's `-e` command for the first window to run.
pub fn set_initial_argv(argv: Vec<String>) {
    let _ = INITIAL_ARGV.set(argv);
}

/// The `-e` command, if this process was started with one.
pub fn initial_argv() -> Option<&'static [String]> {
    INITIAL_ARGV.get().map(Vec::as_slice)
}

/// Record `--restore-session`.
pub fn set_restore_session(on: bool) {
    RESTORE_SESSION.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether this process was relaunched to restore its layout.
pub fn restore_session() -> bool {
    RESTORE_SESSION.load(std::sync::atomic::Ordering::Relaxed)
}

static EXPLICIT_START: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record that the command line named a directory or command to start with.
pub fn set_explicit_start(on: bool) {
    EXPLICIT_START.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the command line asked for a specific start (so no state restore).
pub fn explicit_start() -> bool {
    EXPLICIT_START.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Cli {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse(
            &args,
            Path::new(r"C:\work"),
            Some(Path::new(r"C:\Users\me")),
            &|p| p.extension().is_some_and(|e| e == "txt"),
        )
    }

    #[test]
    fn no_arguments_is_a_plain_launch() {
        assert_eq!(p(&[]), Cli::default());
    }

    #[test]
    fn dash_e_swallows_everything_after_it_including_flags() {
        let c = p(&[
            "--font-size=14",
            "-e",
            "pwsh",
            "-NoLogo",
            "--version",
            "+new-tab",
        ]);
        assert_eq!(
            c.command,
            Some(CommandSpec::Argv(vec![
                "pwsh".into(),
                "-NoLogo".into(),
                "--version".into(),
                "+new-tab".into()
            ]))
        );
        assert_eq!(c.verb, Verb::Launch, "--version after -e is the command's");
        assert_eq!(c.overrides, vec![("font-size".into(), "14".into())]);
        assert!(c.errors.is_empty());
    }

    #[test]
    fn dash_e_with_nothing_after_it_is_an_error() {
        assert!(!p(&["-e"]).errors.is_empty());
    }

    #[test]
    fn config_overrides_keep_their_order_and_a_bare_flag_is_true() {
        let c = p(&["--background=#000", "--maximize", "--title=a = b"]);
        assert_eq!(
            c.overrides,
            vec![
                ("background".into(), "#000".into()),
                ("maximize".into(), "true".into()),
                ("title".into(), "a = b".into()),
            ]
        );
        assert_eq!(
            c.override_body(),
            "background = #000\nmaximize = true\ntitle = a = b\n"
        );
    }

    #[test]
    fn a_multi_line_override_is_refused_rather_than_injecting_a_second_key() {
        let c = p(&["--title=x\nshell = evil"]);
        assert!(c.overrides.is_empty());
        assert!(!c.errors.is_empty());
    }

    #[test]
    fn working_directory_is_made_absolute_and_expands_home() {
        assert_eq!(
            p(&["--working-directory=src"]).cwd,
            Some(PathBuf::from(r"C:\work\src"))
        );
        assert_eq!(
            p(&["--working-directory=~/x"]).cwd,
            Some(PathBuf::from(r"C:\Users\me\x"))
        );
        // Keywords stay config values.
        let c = p(&["--working-directory=home"]);
        assert_eq!(c.cwd, None);
        assert_eq!(
            c.overrides,
            vec![("working-directory".into(), "home".into())]
        );
    }

    #[test]
    fn config_file_resolves_against_the_invoking_directory() {
        let c = p(&["--config-file=extra.conf", "--config-file=?opt.conf"]);
        assert_eq!(
            c.overrides,
            vec![
                ("config-file".into(), r"C:\work\extra.conf".into()),
                ("config-file".into(), r"?C:\work\opt.conf".into()),
            ]
        );
    }

    #[test]
    fn a_positional_dir_is_absolute_and_a_file_opens_its_folder() {
        let c = p(&["proj"]);
        assert_eq!(c.cwd, Some(PathBuf::from(r"C:\work\proj")));
        assert!(c.positional_dir);
        assert_eq!(
            p(&[r"D:\notes\a.txt"]).cwd,
            Some(PathBuf::from(r"D:\notes"))
        );
        assert!(
            !p(&["a", "b"]).errors.is_empty(),
            "two directories is an error"
        );
    }

    #[test]
    fn explorer_drive_root_quoting_accident_is_repaired() {
        // `"%V"` on `C:\` arrives as `C:"` under the MSVC argv rules.
        assert_eq!(clean_path_arg("C:\""), r"C:\");
        assert_eq!(clean_path_arg(r#""D:\dir""#), r"D:\dir");
        assert_eq!(p(&["C:\""]).cwd, Some(PathBuf::from(r"C:\")));
    }

    #[test]
    fn plus_verbs_are_recognised_only_first() {
        assert_eq!(p(&["+new-window"]).verb, Verb::NewWindow);
        assert_eq!(p(&["+new-tab"]).verb, Verb::NewTab);
        assert_eq!(
            p(&["+action=new_split:right"]).verb,
            Verb::Action("new_split:right".into())
        );
        assert_eq!(p(&["+input=ls"]).verb, Verb::Input("ls".into()));
        assert_eq!(
            p(&["+register-shell-integration"]).verb,
            Verb::RegisterShellIntegration
        );
        assert_eq!(
            p(&["+register-default-terminal"]).verb,
            Verb::RegisterDefaultTerminal
        );
        assert_eq!(
            p(&["+unregister-default-terminal"]).verb,
            Verb::UnregisterDefaultTerminal
        );
        // Not first: a positional path.
        assert_eq!(p(&["x", "+new-tab"]).verb, Verb::Launch);
        let bad = p(&["+frobnicate"]);
        assert_eq!(bad.verb, Verb::Help);
        assert!(!bad.errors.is_empty());
    }

    #[test]
    fn help_and_version_flags() {
        assert_eq!(p(&["--help"]).verb, Verb::Help);
        assert_eq!(p(&["-h"]).verb, Verb::Help);
        assert_eq!(p(&["--version"]).verb, Verb::Version);
        assert!(version_string().starts_with("giest "));
    }

    #[test]
    fn command_flag_is_the_forwarded_command_only_for_new_window_and_tab() {
        let c = p(&["+new-tab", "--command=pwsh", "--working-directory=x"]);
        assert_eq!(c.command, Some(CommandSpec::Line("pwsh".into())));
        assert_eq!(
            c.plan(true),
            Plan::Forward(Request::NewTab {
                cwd: Some(r"C:\work\x".into()),
                command: Some(CommandSpec::Line("pwsh".into())),
                window: None,
            })
        );
        // On a plain launch, `--command` is the ordinary config key.
        assert_eq!(
            p(&["--command=cmd"]).overrides,
            vec![("command".into(), "cmd".into())]
        );
    }

    #[test]
    fn a_plain_launch_forwards_as_a_window_and_explorer_as_a_tab() {
        assert_eq!(
            p(&[]).plan(true),
            Plan::Forward(Request::NewWindow {
                cwd: None,
                command: None
            })
        );
        assert!(matches!(
            p(&["proj"]).plan(true),
            Plan::Forward(Request::NewTab { .. })
        ));
        // `macos-dock-drop-behavior = new-window`: the same positional dir opens
        // a window instead of a tab.
        assert!(matches!(
            p(&["proj"]).plan_with(true, crate::config::DropBehavior::NewWindow),
            Plan::Forward(Request::NewWindow { .. })
        ));
        assert_eq!(p(&[]).plan(false), Plan::Local { serve: false });
    }

    #[test]
    fn dash_e_and_overrides_never_forward_and_dash_e_never_serves() {
        assert_eq!(p(&["-e", "cmd"]).plan(true), Plan::Local { serve: false });
        assert_eq!(
            p(&["--font-size=9"]).plan(true),
            Plan::Local { serve: true }
        );
        assert_eq!(
            p(&["--restore-session"]).plan(true),
            Plan::Local { serve: true }
        );
    }

    #[test]
    fn a_local_start_folds_cwd_and_command_into_the_override_body() {
        let c = p(&["+new-window", "--command=cmd", "--working-directory=x"]);
        assert_eq!(
            c.override_body(),
            "working-directory = C:\\work\\x\ninitial-command = cmd\n"
        );
    }
}
