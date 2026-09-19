//! Terminal configuration: fonts and the color theme. Built-in defaults that an
//! optional config file overrides.
//!
//! The file uses **Ghostty's config format** — `key = value` lines, kebab-case
//! keys, unquoted colors (`background = #1d1f21`), `#`-prefixed comment lines,
//! and a repeatable `palette` key — so values are transposable with a real
//! Ghostty config. It lives at `%APPDATA%\giest\config` (override the whole path
//! with `$GIEST_CONFIG`). Every key is optional; unset ones keep the built-in
//! default and an empty value (`key =`) resets that key to its default. Keys we
//! don't support are ignored with a warning, so a full Ghostty config can be
//! dropped in and the supported subset applies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::engine::{BoldColor, CursorShape, Rgb};

// Parse diagnostics. The setters are plain `fn` pointers with nowhere to put a
// message, and threading a sink through every one of them would be all noise —
// so problems go to a thread-local list that the load entry points clear before
// parsing and drain into `Config::diagnostics` after. Thread-local rather than
// global so parallel `cargo test` threads can't see each other's messages.
thread_local! {
    static DIAGNOSTICS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Record a config problem: still printed to stderr as before (useful from a
/// console), and kept for the config-errors dialog — the only place a GUI user
/// will ever see it.
fn report(msg: String) {
    eprintln!("{msg}");
    let msg = msg.strip_prefix("giest: ").unwrap_or(&msg).to_string();
    DIAGNOSTICS.with(|d| d.borrow_mut().push(msg));
}

fn take_diagnostics() -> Vec<String> {
    DIAGNOSTICS.with(|d| std::mem::take(&mut *d.borrow_mut()))
}

macro_rules! diag {
    ($($t:tt)*) => { report(format!($($t)*)) };
}

/// Which in-app toasts are shown. Ghostty `app-notifications`, a packed struct
/// parsed like [`BellFeatures`]; both default **on**.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppNotifications {
    /// "Copied to clipboard" after an explicit copy.
    pub clipboard_copy: bool,
    /// "Reloaded the configuration" after a reload.
    pub config_reload: bool,
}

impl Default for AppNotifications {
    fn default() -> Self {
        Self { clipboard_copy: true, config_reload: true }
    }
}

/// Parse `app-notifications` by the same packed-struct rules as
/// [`parse_bell_features`]: a strict bare boolean sets every flag, otherwise
/// start from the defaults, `no-` negates, and one unknown token rejects the
/// whole value.
fn parse_app_notifications(value: &str) -> Option<AppNotifications> {
    let v = value.trim();
    let all = |on| AppNotifications { clipboard_copy: on, config_reload: on };
    match v {
        "1" | "t" | "true" => return Some(all(true)),
        "0" | "f" | "false" => return Some(all(false)),
        _ => {}
    }
    let mut out = AppNotifications::default();
    for tok in v.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let (name, on) = match tok.strip_prefix("no-") {
            Some(rest) => (rest, false),
            None => (tok, true),
        };
        match name.to_ascii_lowercase().as_str() {
            "clipboard-copy" => out.clipboard_copy = on,
            "config-reload" => out.config_reload = on,
            _ => return None,
        }
    }
    Some(out)
}

/// What a right-click inside a terminal pane does. Ghostty `right-click-action`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightClickAction {
    /// Show a context menu (Copy/Paste/Split/Reset/Select All). The default.
    ContextMenu,
    /// Copy the current selection.
    Copy,
    /// Paste the clipboard.
    Paste,
    /// Copy if there is a selection, otherwise paste.
    CopyOrPaste,
    /// Do nothing.
    Ignore,
}

/// What a middle-click inside a terminal pane does. Ghostty `middle-click-action`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MiddleClickAction {
    /// Paste giest's emulated PRIMARY selection (`crate::primary`), falling back
    /// to the system clipboard while it is empty. The default.
    PrimaryPaste,
    /// Paste the system clipboard.
    ClipboardPaste,
    /// Do nothing.
    Ignore,
}

/// Ghostty `copy-on-select`. Windows has no PRIMARY selection, so giest keeps an
/// in-process one (`crate::primary`) for `primary`/`both` to write to, which
/// middle-click and `paste_from_selection` read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyOnSelect {
    /// Don't copy automatically. The default off Linux, as upstream.
    None,
    /// The emulated PRIMARY buffer only.
    Primary,
    /// The system clipboard only.
    Clipboard,
    /// Both.
    Both,
}

impl CopyOnSelect {
    pub fn primary(self) -> bool {
        matches!(self, Self::Primary | Self::Both)
    }
    pub fn clipboard(self) -> bool {
        matches!(self, Self::Clipboard | Self::Both)
    }
}

/// Ghostty `mouse-shift-capture`: whether Shift+click goes to a mouse-tracking
/// program (`true`/`always`) or extends the selection (`false`/`never`). The
/// `true`/`false` forms can be overridden by the program with `XTSHIFTESCAPE`
/// (`CSI > Ps s`), which giest side-scans (`crate::xtshiftescape`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseShiftCapture {
    False,
    True,
    Always,
    Never,
}

impl MouseShiftCapture {
    /// Whether shift is captured (sent to the program) given the program's
    /// latest `XTSHIFTESCAPE` request, if any. Mirrors `Surface.zig`'s
    /// `mouseShiftCapture`.
    pub fn captured(self, program: Option<bool>) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::False => program.unwrap_or(false),
            Self::True => program.unwrap_or(true),
        }
    }
}

/// One `command-palette-entry` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteEntry {
    pub title: String,
    pub description: Option<String>,
    /// The action, in keybind-action syntax (parsed when the palette opens).
    pub action: String,
}

/// Parse `title:…,description:…,action:…`. A value may be a double-quoted
/// Zig-style string literal (for commas, edge whitespace, or `\xNN` escapes).
/// `title` and `action` are required; an unknown field rejects the line.
pub fn parse_command_palette_entry(v: &str) -> Option<PaletteEntry> {
    let (mut title, mut description, mut action) = (None, None, None);
    let mut rest = v.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once(':')?;
        let key = key.trim();
        let after = after.trim_start();
        let (value, tail) = if let Some(q) = after.strip_prefix('"') {
            // The closing quote, skipping backslash escapes.
            let bytes = q.as_bytes();
            let mut i = 0;
            let mut end = None;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i += 2,
                    b'"' => {
                        end = Some(i);
                        break;
                    }
                    _ => i += 1,
                }
            }
            let end = end?;
            let lit = crate::command::decode_escapes(&q[..end])?;
            (lit, q[end + 1..].trim_start())
        } else {
            match after.find(',') {
                Some(i) => (after[..i].trim().to_string(), &after[i..]),
                None => (after.trim().to_string(), ""),
            }
        };
        match key {
            "title" => title = Some(value),
            "description" => description = Some(value),
            "action" => action = Some(value),
            _ => return None,
        }
        rest = match tail.strip_prefix(',') {
            Some(t) => t.trim_start(),
            None if tail.is_empty() => "",
            None => return None,
        };
    }
    Some(PaletteEntry {
        title: title.filter(|t| !t.is_empty())?,
        description: description.filter(|d| !d.is_empty()),
        action: action.filter(|a| !a.is_empty())?,
    })
}

/// Parse a comma-separated list of `U+XXXX` / `U+XXXX-U+YYYY` ranges.
pub fn parse_codepoint_ranges(v: &str) -> Option<Vec<(u32, u32)>> {
    let one = |s: &str| -> Option<u32> {
        let s = s.trim();
        let h = s.strip_prefix("U+").or_else(|| s.strip_prefix("u+"))?;
        let n = u32::from_str_radix(h, 16).ok()?;
        char::from_u32(n).map(|_| n)
    };
    v.split(',')
        .map(|r| {
            let r = r.trim();
            match r.split_once('-') {
                Some((a, b)) => {
                    let (a, b) = (one(a)?, one(b)?);
                    (a <= b).then_some((a, b))
                }
                None => one(r).map(|a| (a, a)),
            }
        })
        .collect()
}

/// One `clipboard-codepoint-map` line: ranges and their replacement text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardMap {
    pub ranges: Vec<(u32, u32)>,
    pub replacement: String,
}

/// Parse `U+2500=U+002D` / `U+03A3=SUM`. A replacement that is itself a single
/// `U+XXXX` is that character; anything else is literal text.
pub fn parse_clipboard_map(v: &str) -> Option<ClipboardMap> {
    let (lhs, rhs) = v.split_once('=')?;
    let ranges = parse_codepoint_ranges(lhs)?;
    let rhs = rhs.trim();
    let replacement = match parse_codepoint_ranges(rhs).as_deref() {
        Some([(a, b)]) if a == b => char::from_u32(*a)?.to_string(),
        _ => rhs.to_string(),
    };
    Some(ClipboardMap { ranges, replacement })
}

/// Apply `clipboard-codepoint-map` to copied text. Later entries win over
/// earlier ones for overlapping ranges, as upstream documents.
pub fn map_clipboard_text(maps: &[ClipboardMap], text: &str) -> String {
    if maps.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        let cp = ch as u32;
        match maps
            .iter()
            .rev()
            .find(|m| m.ranges.iter().any(|&(a, b)| a <= cp && cp <= b))
        {
            Some(m) => out.push_str(&m.replacement),
            None => out.push(ch),
        }
    }
    out
}

/// One `font-codepoint-map` line: ranges forced to a named family.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontCodepointMap {
    pub ranges: Vec<(u32, u32)>,
    pub family: String,
}

/// Window backdrop blur behind a translucent background. Ghostty
/// `background-blur`, whose grammar is `false` | `true` (the default intensity,
/// 20) | a nonnegative integer intensity | `macos-glass-regular` /
/// `macos-glass-clear` (macOS 26 glass, which off macOS just implies `true` —
/// Ghostty's own Linux path does the same).
///
/// giest maps this onto the Windows DWM backdrops, which have **no radius knob**,
/// so the intensity is only a two-bucket selector (mica below 10, acrylic at or
/// above it) rather than the true Gaussian sigma it is on macOS/KWin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundBlur {
    Off,
    /// Ghostty's `true` — the default intensity of 20.
    On,
    /// An explicit intensity. `Radius(0)` is *disabled*, matching Ghostty.
    Radius(u8),
}

impl BackgroundBlur {
    /// Ghostty's `cval`: the numeric intensity (`true` is 20, `false` is 0).
    pub fn intensity(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::On => 20,
            Self::Radius(r) => r,
        }
    }

    pub fn enabled(self) -> bool {
        self.intensity() > 0
    }
}

/// Which effects fire when a program rings the bell (BEL, `0x07`). Ghostty
/// `bell-features`, a packed-struct bitfield.
///
/// Note the defaults: `attention` and `title` are **on**, `border` is **off**.
/// giest historically flashed the pane border by default; matching Ghostty turns
/// that off, and `bell-features = border` restores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BellFeatures {
    /// Play the OS alert sound (`MessageBeep`).
    pub system: bool,
    /// Play the sound file at [`Config::bell_audio_path`].
    pub audio: bool,
    /// Request the user's attention while the window is unfocused — a taskbar
    /// flash on Windows.
    pub attention: bool,
    /// Prefix the window title with 🔔 until the window is refocused.
    pub title: bool,
    /// Flash a border around the pane that rang.
    pub border: bool,
}

impl Default for BellFeatures {
    fn default() -> Self {
        Self { system: false, audio: false, attention: true, title: true, border: false }
    }
}

impl BellFeatures {
    /// Every feature set to `on` — Ghostty's bare-boolean shorthand.
    fn all(on: bool) -> Self {
        Self { system: on, audio: on, attention: on, title: on, border: on }
    }
}

/// Parse a `bell-features` value the way Ghostty's `parsePackedStruct`
/// (`cli/args.zig`) does. Returns `None` for an invalid value, which the setter
/// turns into "keep the current value".
///
/// Three rules that are easy to get wrong, and all deliberate:
///  - a bare boolean sets **every** field. Ghostty's packed-struct parser uses a
///    *stricter* boolean set than its ordinary one — only `1/t/true/0/f/false`,
///    so `on`/`yes` are feature-name errors here, not booleans. Hence this does
///    not call [`parse_bool`].
///  - otherwise the result starts from the struct **defaults**, not from the
///    current config. So `bell-features = audio` still leaves `attention` and
///    `title` on, and a second `bell-features` line *replaces* the first rather
///    than accumulating onto it.
///  - one unrecognized token rejects the **whole** value (Ghostty returns
///    `InvalidValue` for the entry); we don't keep the features parsed so far.
fn parse_bell_features(value: &str) -> Option<BellFeatures> {
    let v = value.trim();
    match v {
        "1" | "t" | "true" => return Some(BellFeatures::all(true)),
        "0" | "f" | "false" => return Some(BellFeatures::all(false)),
        _ => {}
    }
    let mut out = BellFeatures::default();
    for tok in v.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let (name, on) = match tok.strip_prefix("no-") {
            Some(rest) => (rest, false),
            None => (tok, true),
        };
        match name.to_ascii_lowercase().as_str() {
            "system" => out.system = on,
            "audio" => out.audio = on,
            "attention" => out.attention = on,
            "title" => out.title = on,
            "border" => out.border = on,
            _ => return None,
        }
    }
    Some(out)
}

/// The user's home directory (`%USERPROFILE%`, else `%HOMEDRIVE%%HOMEPATH%`).
fn home_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(p));
    }
    let drive = std::env::var_os("HOMEDRIVE")?;
    let path = std::env::var_os("HOMEPATH")?;
    Some(PathBuf::from(drive).join(path))
}

/// Resolve one `search-*` color: empty resets, a bad value keeps the current one
/// (and says so, since a silently ignored colour looks like the key not working).
fn terminal_color(v: &str, current: TerminalColor, default: TerminalColor) -> TerminalColor {
    if v.is_empty() {
        return default;
    }
    TerminalColor::parse(v).unwrap_or_else(|| {
        diag!("giest: ignoring unparseable color '{v}'");
        current
    })
}

/// A color that may instead defer to the cell's own colors. Ghostty's
/// `TerminalColor`, used by the search-highlight keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalColor {
    Color(Rgb),
    /// The cell's foreground color.
    CellForeground,
    /// The cell's background color.
    CellBackground,
}

impl TerminalColor {
    pub fn parse(v: &str) -> Option<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "cell-foreground" => Some(Self::CellForeground),
            "cell-background" => Some(Self::CellBackground),
            _ => parse_color(v).map(Self::Color),
        }
    }

    /// Resolve against the cell's own colors.
    pub fn resolve(self, cell_fg: Rgb, cell_bg: Rgb) -> Rgb {
        match self {
            Self::Color(c) => c,
            Self::CellForeground => cell_fg,
            Self::CellBackground => cell_bg,
        }
    }
}

/// Where a new tab is inserted. Ghostty `window-new-tab-position`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NewTabPosition {
    /// Immediately after the focused tab (upstream's default).
    #[default]
    Current,
    /// At the end of the tab strip.
    End,
}

/// How leftover space is distributed around the grid. Ghostty
/// `window-padding-balance`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PaddingBalance {
    /// No balancing: the leftover space all falls to the right/bottom.
    #[default]
    None,
    /// Balance, but cap the *top* padding so text doesn't float, pushing the
    /// excess to the bottom.
    Balanced,
    /// Balance equally on all sides, with no cap on the top.
    Equal,
}

/// Split the leftover pixels on one axis into (leading, trailing) padding.
///
/// A pure function so the three modes can be table-tested. `Balanced` caps the
/// leading side at half the *cell* size, which is what stops a nearly-full extra
/// row of space from being parked above the first line — upstream's rule, and
/// the reason `Equal` exists as a separate value for people who want it split
/// down the middle regardless.
pub fn balance_padding(mode: PaddingBalance, leftover: f32, cell: f32) -> (f32, f32) {
    let leftover = leftover.max(0.0);
    match mode {
        PaddingBalance::None => (0.0, leftover),
        PaddingBalance::Equal => {
            let lead = (leftover / 2.0).floor();
            (lead, leftover - lead)
        }
        PaddingBalance::Balanced => {
            let lead = (leftover / 2.0).floor().min((cell / 2.0).floor());
            (lead, leftover - lead)
        }
    }
}

/// Parse `selection-word-chars`: every character in the value becomes a word
/// boundary.
///
/// Iterates **characters, not bytes** — Ghostty's own default list contains `│`
/// (U+2502), so a byte loop would split it into three bogus boundaries. The
/// escape `\t` is honoured (upstream accepts Zig string escapes; `\t` is the one
/// that appears in its documented default), and `\\` yields a literal backslash.
///
/// NUL is always a boundary upstream and is prepended here, since a caller that
/// passes an explicit list replaces the engine's defaults wholesale.
fn parse_word_chars(v: &str) -> Vec<char> {
    let mut out = vec!['\0'];
    let mut chars = v.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                // An unknown escape keeps both characters rather than eating one
                // silently — the user can see what happened.
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            _ => out.push(ch),
        }
    }
    out
}

/// Which styles may be *synthesized* when the configured family has no real
/// face for them. Ghostty `font-synthetic-style`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyntheticStyle {
    pub bold: bool,
    pub italic: bool,
    pub bold_italic: bool,
}

impl SyntheticStyle {
    fn all(on: bool) -> Self {
        Self {
            bold: on,
            italic: on,
            bold_italic: on,
        }
    }
}

impl Default for SyntheticStyle {
    /// All three on, like upstream.
    fn default() -> Self {
        Self::all(true)
    }
}

/// Parse `font-synthetic-style`, which has the same packed-struct grammar as
/// `bell-features`: a bare bool sets every flag, a list starts from the
/// *defaults* (so it replaces rather than accumulates), `no-` turns one off, and
/// a single unknown token rejects the whole value.
///
/// The three flags are **independent**, which upstream calls out as the easy
/// mistake: `no-bold` does not disable bold-italic. Mirrored here, and pinned by
/// a test, because "I turned bold off and bold-italic is still synthesized"
/// looks like a bug rather than the documented behaviour.
fn parse_synthetic_style(value: &str) -> Option<SyntheticStyle> {
    let v = value.trim();
    match v {
        "1" | "t" | "true" => return Some(SyntheticStyle::all(true)),
        "0" | "f" | "false" => return Some(SyntheticStyle::all(false)),
        _ => {}
    }
    let mut out = SyntheticStyle::default();
    for tok in v.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let (name, on) = match tok.strip_prefix("no-") {
            Some(rest) => (rest, false),
            None => (tok, true),
        };
        match name.to_ascii_lowercase().as_str() {
            "bold" => out.bold = on,
            "italic" => out.italic = on,
            "bold-italic" => out.bold_italic = on,
            _ => return None,
        }
    }
    Some(out)
}

/// Wheel-distance multipliers. Ghostty `mouse-scroll-multiplier`.
///
/// Two numbers because the devices are different animals: a wheel emits chunky
/// notches, a trackpad emits a stream of small deltas, and one multiplier that
/// suits either ruins the other. Ghostty defaults to `3` for discrete and `1`
/// for precision.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseScrollMultiplier {
    /// Trackpads and other smooth-scroll devices.
    pub precision: f32,
    /// Notched mouse wheels.
    pub discrete: f32,
}

impl Default for MouseScrollMultiplier {
    fn default() -> Self {
        Self {
            precision: 1.0,
            discrete: 3.0,
        }
    }
}

/// Parse `mouse-scroll-multiplier`: a bare number sets **both**, or a
/// comma-separated list of `precision:`/`discrete:` prefixes sets them
/// individually (`precision:0.1,discrete:3`).
///
/// Values are clamped to Ghostty's `0.01..=10_000`, which it calls extreme at
/// both ends — the clamp only stops a typo turning the wheel into a no-op or a
/// teleport.
fn parse_scroll_multiplier(
    v: &str,
    current: MouseScrollMultiplier,
) -> Option<MouseScrollMultiplier> {
    let clamp = |n: f32| n.clamp(0.01, 10_000.0);
    if let Ok(n) = v.trim().parse::<f32>() {
        return Some(MouseScrollMultiplier {
            precision: clamp(n),
            discrete: clamp(n),
        });
    }
    let mut out = current;
    for tok in v.split(',') {
        let (name, value) = tok.trim().split_once(':')?;
        let n = clamp(value.trim().parse::<f32>().ok()?);
        match name.trim().to_ascii_lowercase().as_str() {
            "precision" => out.precision = n,
            "discrete" => out.discrete = n,
            _ => return None,
        }
    }
    Some(out)
}

/// When the viewport jumps back to the live edge. Ghostty `scroll-to-bottom`,
/// whose default is `keystroke, no-output`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollToBottom {
    /// On a key press that sends data to the shell.
    pub keystroke: bool,
    /// On new output arriving — off by default, because it fights you while
    /// you're reading scrollback of a still-running command.
    pub output: bool,
}

impl Default for ScrollToBottom {
    fn default() -> Self {
        Self {
            keystroke: true,
            output: false,
        }
    }
}

/// Parse `scroll-to-bottom`'s flag list, same grammar as `bell-features`.
fn parse_scroll_to_bottom(v: &str) -> Option<ScrollToBottom> {
    let mut out = ScrollToBottom::default();
    for tok in v.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let (name, on) = match tok.strip_prefix("no-") {
            Some(rest) => (rest, false),
            None => (tok, true),
        };
        match name.to_ascii_lowercase().as_str() {
            "keystroke" => out.keystroke = on,
            "output" => out.output = on,
            _ => return None,
        }
    }
    Some(out)
}

/// When a custom shader's animation loop runs. Ghostty
/// `custom-shader-animation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CustomShaderAnimation {
    /// Never animate — redraw only when the terminal itself changes.
    False,
    /// Animate while the window is focused. The default.
    True,
    /// Animate even unfocused, which costs CPU on every surface.
    Always,
}

impl CustomShaderAnimation {
    /// Whether to keep requesting repaints, given the window's focus.
    pub fn animates(self, focused: bool) -> bool {
        match self {
            Self::False => false,
            Self::True => focused,
            Self::Always => true,
        }
    }
}

/// When a finished command raises a notification. Ghostty
/// `notify-on-command-finish`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyOnCommandFinish {
    /// The default — opt in explicitly.
    Never,
    /// Only when the window isn't focused, i.e. only when you've looked away.
    Unfocused,
    Always,
}

/// How a finished command tells you. Ghostty `notify-on-command-finish-action`,
/// a `bell-features`-style flag list (`no-bell,notify`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotifyOnCommandFinishAction {
    pub bell: bool,
    pub notify: bool,
}

impl Default for NotifyOnCommandFinishAction {
    /// Ghostty's defaults: ring, don't notify.
    fn default() -> Self {
        Self {
            bell: true,
            notify: false,
        }
    }
}

impl NotifyOnCommandFinishAction {
    fn all(on: bool) -> Self {
        Self {
            bell: on,
            notify: on,
        }
    }
}

/// Parse a `notify-on-command-finish-action` value. Same grammar as
/// [`parse_bell_features`] — a comma-separated list starting from the defaults,
/// each name negatable with `no-`, or a bare bool for all of them. One unknown
/// name rejects the whole value (`None`), so a typo keeps the current setting
/// rather than silently applying half of what was written.
fn parse_notify_action(value: &str) -> Option<NotifyOnCommandFinishAction> {
    let v = value.trim();
    match v {
        "1" | "t" | "true" => return Some(NotifyOnCommandFinishAction::all(true)),
        "0" | "f" | "false" => return Some(NotifyOnCommandFinishAction::all(false)),
        _ => {}
    }
    let mut out = NotifyOnCommandFinishAction::default();
    for tok in v.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let (name, on) = match tok.strip_prefix("no-") {
            Some(rest) => (rest, false),
            None => (tok, true),
        };
        match name.to_ascii_lowercase().as_str() {
            "bell" => out.bell = on,
            "notify" => out.notify = on,
            _ => return None,
        }
    }
    Some(out)
}

/// Whether a finished command should raise its notification, given the mode and
/// whether the window is focused.
pub fn should_notify_on_finish(mode: NotifyOnCommandFinish, focused: bool) -> bool {
    match mode {
        NotifyOnCommandFinish::Never => false,
        NotifyOnCommandFinish::Unfocused => !focused,
        NotifyOnCommandFinish::Always => true,
    }
}

/// When to confirm before closing a surface. Ghostty `confirm-close-surface`,
/// whose values are `false` / `true` / `always`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmClose {
    /// Never confirm (`false`).
    Never,
    /// Confirm only when something looks like it's running (`true`, the default).
    WhenBusy,
    /// Always confirm, even at an idle prompt (`always`).
    Always,
}

/// Whether closing needs confirmation.
///
/// `busy` is `None` when giest can't tell — a shell that emits no OSC 133 prompt
/// marks — and that resolves to **confirm**, the conservative side: better an
/// extra prompt than a silently discarded running command.
pub fn needs_confirm(mode: ConfirmClose, busy: Option<bool>) -> bool {
    match mode {
        ConfirmClose::Never => false,
        ConfirmClose::Always => true,
        ConfirmClose::WhenBusy => busy.unwrap_or(true),
    }
}

/// An `adjust-*` metric adjustment: Ghostty's `MetricModifier`.
///
/// The values are **deltas, not settings** — `1` means "one more pixel than the
/// font/cell implies", and `20%` means "a fifth bigger". That trips people up
/// (a `1` looks like it should set the value to 1), so it is spelled out in the
/// guide as well as here.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum MetricModifier {
    /// Unset: use the derived value unchanged.
    #[default]
    None,
    /// Add this many pixels (may be negative).
    Pixels(i32),
    /// Change by this percentage (`20%` → `0.20`, `-15%` → `-0.15`).
    Percent(f32),
}

impl MetricModifier {
    pub fn parse(v: &str) -> Option<Self> {
        let s = v.trim();
        if let Some(p) = s.strip_suffix('%') {
            return p.trim().parse::<f32>().ok().map(|v| Self::Percent(v / 100.0));
        }
        s.parse::<i32>().ok().map(Self::Pixels)
    }

    /// Apply to a derived value: exact arithmetic, **no rounding and no clamp**.
    ///
    /// Rounding belongs to the caller, because the three kinds of metric round
    /// differently and doing it here silently corrupted one of them: a cell width
    /// is `ceil`ed (a 6.36px advance is a 7px cell), so rounding first turns it
    /// into a 6px cell and every column loses a pixel. Positions round to
    /// nearest, thicknesses ceil — see [`Self::apply_thickness`].
    pub fn apply(self, value: f32) -> f32 {
        match self {
            MetricModifier::None => value,
            MetricModifier::Pixels(p) => value + p as f32,
            MetricModifier::Percent(p) => value + value * p,
        }
    }

    /// Apply to a *thickness*: `ceil`ed and clamped to at least 1, exactly as
    /// upstream's `@max(1, @ceil(...))`.
    ///
    /// The clamp is load-bearing: a thickness of zero is an invisible line,
    /// which reads as the character being missing rather than as the adjustment
    /// being too aggressive. Positions are deliberately *not* clamped this way —
    /// zero and negative are meaningful placements there.
    pub fn apply_thickness(self, value: f32) -> f32 {
        self.apply(value).ceil().max(1.0)
    }
}

/// Resolve one `adjust-*` value: empty resets to the default, a malformed value
/// is reported and keeps the previous one (Ghostty makes it a config error; giest
/// has no error UI, so it says so and carries on).
fn adjust_value(v: &str, current: MetricModifier, default: MetricModifier) -> MetricModifier {
    if v.is_empty() {
        return default;
    }
    match MetricModifier::parse(v) {
        Some(m) => m,
        None => {
            diag!("giest: ignoring adjustment '{v}' (expected a number like '1', '-2' or '20%')");
            current
        }
    }
}

/// The whole `adjust-*` family. Ghostty's `ModifierSet`, as a plain struct: the
/// set is fixed and small, and a struct makes every consumer a field access that
/// the compiler checks rather than a map lookup that can silently miss.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct MetricAdjust {
    pub cell_width: MetricModifier,
    pub cell_height: MetricModifier,
    /// Distance from the bottom of the cell to the text baseline.
    pub font_baseline: MetricModifier,
    pub underline_position: MetricModifier,
    pub underline_thickness: MetricModifier,
    pub strikethrough_position: MetricModifier,
    pub strikethrough_thickness: MetricModifier,
    pub overline_position: MetricModifier,
    pub overline_thickness: MetricModifier,
    pub cursor_thickness: MetricModifier,
    pub cursor_height: MetricModifier,
    /// Thickness of the drawn box-drawing lines (see [`crate::sprite`]).
    pub box_thickness: MetricModifier,
    /// Maximum height a Nerd Font icon is scaled to fit. Ghostty
    /// `adjust-icon-height`.
    pub icon_height: MetricModifier,
}

/// Whether the window/tab/split layout survives a quit. Ghostty
/// `window-save-state`, whose values are `default` / `never` / `always`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WindowSaveState {
    /// Restore only when the OS asks for it. On macOS that's the system's own
    /// "reopen windows on relaunch"; **Windows has no such mechanism**, so here
    /// it behaves as `never` — the same net effect as Ghostty's default on a
    /// machine with the system setting off.
    #[default]
    Default,
    /// Never save or restore.
    Never,
    /// Always save on exit and restore on launch.
    Always,
}

impl WindowSaveState {
    /// Whether giest should write a state file on exit and read it at startup.
    /// One predicate for both halves on purpose: a mode that saved but never
    /// restored would leave a file that only ever goes stale.
    pub fn restores(self) -> bool {
        matches!(self, WindowSaveState::Always)
    }
}

/// Permission for a clipboard operation the *terminal program* asks for.
/// Ghostty `clipboard-read` / `clipboard-write`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardAccess {
    /// Perform it silently.
    Allow,
    /// Refuse it silently.
    Deny,
    /// Ask the user first.
    Ask,
}

/// The paste-safety knobs, grouped so [`Session`](crate::session::Session) can
/// hold one `Copy` value instead of five loose fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardPolicy {
    pub read: ClipboardAccess,
    pub write: ClipboardAccess,
    pub trim_trailing_spaces: bool,
    pub paste_protection: bool,
    pub paste_bracketed_safe: bool,
    /// Largest OSC 52 write accepted, in bytes; `None` = `unlimited`.
    /// Ghostty `clipboard-write-limit-bytes` (default 64 MiB).
    pub write_limit: Option<usize>,
}

/// Whether `data` is unsafe to paste and so needs confirmation first.
///
/// A verbatim port of Ghostty's rule (`Surface.zig`, `completeClipboardPaste`),
/// including the order of the checks — it isn't arbitrary:
///
/// - Protection off ⇒ nothing is ever unsafe.
/// - **`ESC [ 201 ~` is never trusted while bracketed.** It closes a bracketed
///   paste early, so text after it lands as if typed. This is checked *before*
///   `paste_bracketed_safe`, which is the whole point: framing can't be relied
///   on when the payload can break out of the frame.
/// - Otherwise a bracketed paste is safe if the user trusts bracketing (the
///   default) — the running program has said it will treat the text as data.
/// - Otherwise unsafe if it contains a newline (a shell runs it immediately) or
///   that same end marker. Ghostty flags the marker even when bracketing is off:
///   its presence at all says something about who produced the data.
///
/// The `allow_unsafe` escape hatch Ghostty threads through here lives at the
/// call site instead — a confirmed paste simply skips this function.
pub fn paste_is_unsafe(policy: ClipboardPolicy, bracketed: bool, data: &str) -> bool {
    const PASTE_END: &str = "\x1b[201~";
    if !policy.paste_protection {
        return false;
    }
    if bracketed {
        if data.contains(PASTE_END) {
            return true;
        }
        if policy.paste_bracketed_safe {
            return false;
        }
    }
    data.contains('\n') || data.contains(PASTE_END)
}

/// When to show the grid-size overlay on resize. Ghostty `resize-overlay`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeOverlay {
    Always,
    Never,
    /// Show on every resize *except* a surface's first sizing. The default.
    AfterFirst,
}

/// Whether to show the scrollbar. Ghostty `scrollbar` — which has exactly these
/// two values, and no width/opacity/always knob. Don't add one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scrollbar {
    /// The platform's scrollbar. Ghostty's macOS apprt forces
    /// `scrollerStyle = .overlay` even when the OS prefers the legacy
    /// space-reserving style, so "system" means an auto-hiding overlay
    /// everywhere — which is also the Windows convention.
    System,
    Never,
}

/// How `background-image` is scaled to the terminal area. Ghostty
/// `background-image-fit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundImageFit {
    /// Uniform scale to the largest size fully inside the area. The default.
    Contain,
    /// Uniform scale to the smallest size that covers the area; the overflow is
    /// cropped.
    Cover,
    /// Fill the area exactly, ignoring the aspect ratio.
    Stretch,
    /// Leave the image at its own pixel size.
    None,
}

/// Where `background-image` sits when its fit leaves space. Ghostty
/// `background-image-position` — nine anchors, three per axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundImagePosition {
    TopLeft,
    TopCenter,
    TopRight,
    CenterLeft,
    /// Also spelled `center-center`, and the default.
    Center,
    CenterRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

/// Light/dark mode for the window chrome. Ghostty `window-theme`.
///
/// [`WindowTheme::Auto`] follows the configured `background`, so the chrome
/// matches the terminal rather than the OS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowTheme {
    #[default]
    Auto,
    Dark,
    Light,
}

/// Where the resize overlay sits within the pane. Ghostty
/// `resize-overlay-position`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeOverlayPosition {
    Center,
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

/// Whether a grid-size change should raise the overlay, given the configured
/// mode and whether this is the surface's *first* sizing.
///
/// `after-first` has to be honored explicitly here: a session is created at a
/// default size and immediately re-fit to the real window, so the resize edge
/// always fires once on the first frame and every new tab or split would flash a
/// spurious size. (Ghostty's GTK apprt happens to treat `after-first` the same as
/// `always`, because its scheduler only ever runs from a resize signal — giest
/// matches the *documented* behavior instead.)
pub fn show_resize_overlay(mode: ResizeOverlay, first: bool) -> bool {
    match mode {
        ResizeOverlay::Never => false,
        ResizeOverlay::Always => true,
        ResizeOverlay::AfterFirst => !first,
    }
}

/// Precision of an OSC color *report* (the reply to `OSC 10/11/12 ; ?`).
/// Ghostty `osc-color-report-format`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OscColorReportFormat {
    /// Don't answer color queries at all.
    None,
    /// `rgb:rr/gg/bb` — the raw 8-bit channels.
    Bits8,
    /// `rgb:rrrr/gggg/bbbb`, each channel scaled by 257. The default.
    Bits16,
}

/// User-facing configuration applied at startup.
/// giest `conpty-passthrough` (see [`Config::conpty_passthrough`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConptyPassthrough {
    Auto,
    On,
    Off,
}

impl ConptyPassthrough {
    /// Apply before the first PTY is spawned; later calls cannot change which
    /// ConPTY library is loaded.
    pub fn apply(self) {
        let on = self != ConptyPassthrough::Off;
        portable_pty::set_allow_sideload(on);
        portable_pty::set_passthrough(on);
    }
}

/// Ghostty `window-decoration`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowDecoration {
    Auto,
    Client,
    Server,
    None,
}

impl WindowDecoration {
    /// Whether the native caption/border is drawn. Windows has one decoration
    /// system, so `auto`, `client` and `server` all mean "yes".
    pub fn decorated(self) -> bool {
        self != WindowDecoration::None
    }
}

/// Ghostty `window-show-tab-bar`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShowTabBar {
    Always,
    Auto,
    Never,
}

impl ShowTabBar {
    /// Whether the strip is shown for a window with `tabs` tabs.
    pub fn visible(self, tabs: usize) -> bool {
        match self {
            ShowTabBar::Always => true,
            ShowTabBar::Auto => tabs > 1,
            ShowTabBar::Never => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Logical font size in points (scaled by the display DPI for the atlas).
    /// Ghostty `font-size`.
    pub font_points: f32,
    /// Font families (names or file paths), in order: the first that resolves is
    /// the primary and the rest form a **fallback chain** for characters it
    /// lacks, ahead of the system fonts. Empty keeps the built-in JetBrains
    /// Mono. Ghostty `font-family`, which is repeatable for the same reason.
    /// *(Applied at startup; changing it needs a restart — config reload
    /// re-applies colors/size, not the font.)*
    pub font_family: Vec<String>,
    /// Whether missing styles may be synthesized. Ghostty
    /// `font-synthetic-style`.
    pub font_synthetic_style: SyntheticStyle,
    /// Codepoints that end a word for double-click selection. Empty means the
    /// VT engine's own defaults. Ghostty `selection-word-chars`.
    pub selection_word_chars: Vec<char>,
    /// Clear the selection when the user types into the terminal. Ghostty
    /// `selection-clear-on-typing` (default true).
    pub selection_clear_on_typing: bool,
    /// Clear the selection after an explicit copy. Ghostty
    /// `selection-clear-on-copy` (default false); never applies to
    /// `copy-on-select`, which upstream exempts by name.
    pub selection_clear_on_copy: bool,
    /// Highlight colors for scrollback-search matches. Ghostty `search-*`.
    pub search_bg: TerminalColor,
    pub search_fg: TerminalColor,
    pub search_selected_bg: TerminalColor,
    pub search_selected_fg: TerminalColor,
    /// Ghostty `cursor-text`; `None` (unset) keeps the cell's background.
    pub cursor_text: Option<TerminalColor>,
    /// Color of the gutter between splits; `None` derives one from the chrome.
    /// Ghostty `split-divider-color`.
    pub split_divider_color: Option<Rgb>,
    /// Where a new tab is inserted. Ghostty `window-new-tab-position`.
    pub new_tab_position: NewTabPosition,
    /// How leftover space around the grid is distributed. Ghostty
    /// `window-padding-balance`.
    pub window_padding_balance: PaddingBalance,
    /// Directory new terminals start in when nothing is inherited. `None` =
    /// the process's own directory. Ghostty `working-directory`.
    pub working_directory: Option<PathBuf>,
    /// Per-style family overrides; each `None` falls back to `font_family`.
    /// Ghostty `font-family-bold` / `-italic` / `-bold-italic`.
    pub font_family_bold: Option<String>,
    pub font_family_italic: Option<String>,
    pub font_family_bold_italic: Option<String>,
    /// OpenType feature specs applied while shaping, e.g. `-calt` (disable
    /// ligatures/contextual alternates), `ss01`, `cv01=2`. Repeatable. Ghostty
    /// `font-feature`.
    pub font_features: Vec<String>,
    /// Named style / disabled per style slot, in the order
    /// regular / bold / italic / bold-italic. Ghostty `font-style` and its three
    /// siblings.
    pub font_styles: [FontStyle; 4],
    /// Variable-font axis settings, per style slot, in the order
    /// regular / bold / italic / bold-italic. Ghostty `font-variation` and its
    /// three `-bold` / `-italic` / `-bold-italic` siblings.
    ///
    /// One list **per slot, not inherited**: upstream hands each style
    /// descriptor only its own key's list, so `font-variation` alone leaves the
    /// bold face at the font's default weight. Surprising, and parity.
    pub font_variations: [Vec<FontVariation>; 4],
    /// Default foreground (text) color. Ghostty `foreground`.
    pub fg: Rgb,
    /// Default background color. Ghostty `background`.
    pub bg: Rgb,
    /// The 256-color palette (indices 0–15 = ANSI, 16–231 = color cube,
    /// 232–255 = grayscale ramp). Ghostty `palette`.
    pub palette: [Rgb; 256],
    /// Which `palette` indices were set explicitly (a 256-bit mask), so
    /// `palette-generate` never overwrites them.
    pub palette_set: [u64; 4],
    /// Ghostty `palette-generate` / `palette-harmonious`; see [`Config::effective_palette`].
    pub palette_generate: bool,
    pub palette_harmonious: bool,
    /// Logical-point padding on the left/right of the grid. Ghostty
    /// `window-padding-x`.
    pub padding_x: f32,
    /// Logical-point padding above/below the grid. Ghostty `window-padding-y`.
    pub padding_y: f32,
    /// Light/dark mode for the window *chrome* — the tab strip, the command
    /// palette, the overlays and the dialogs. Ghostty `window-theme`.
    ///
    /// [`WindowTheme::Auto`] derives it from [`Self::bg`], which is what makes
    /// the chrome match the terminal by default; the explicit modes override
    /// that. Note giest never follows the *OS* theme — egui's default does, and
    /// that is what used to render a light tab strip over a dark terminal.
    pub window_theme: WindowTheme,
    /// Coverage gamma applied to text antialiasing in the glyph shader; values
    /// above 1 thicken light-on-dark text that linear blending renders too thin.
    /// giest-specific (`text-gamma`); Ghostty has no equivalent.
    pub text_gamma: f32,
    /// Cursor color; `None` defers to the running program / engine default.
    /// Ghostty `cursor-color`.
    pub cursor: Option<Rgb>,
    /// Default cursor shape used until the running program overrides it with
    /// DECSCUSR. Ghostty `cursor-style` (block / bar / underline; the renderer
    /// also draws a hollow block for unfocused panes regardless of this).
    pub cursor_style: CursorShape,
    /// Whether the default cursor blinks. `None` means unset, which follows
    /// Ghostty's default of *blinking* (its stream handler uses
    /// `default_cursor_blink orelse true`); `Some(b)` forces it. Applies only
    /// while the program hasn't selected its own cursor style via DECSCUSR.
    /// Ghostty `cursor-style-blink`.
    pub cursor_style_blink: Option<bool>,
    /// Foreground policy for bold text. Ghostty `bold-color` (and the deprecated
    /// `bold-is-bright`, which maps to [`BoldColor::Bright`]).
    pub bold_color: BoldColor,
    /// Minimum fg/bg contrast ratio (WCAG, `1.0..=21.0`; `1.0` = off). Ghostty
    /// `minimum-contrast`.
    pub min_contrast: f32,
    /// Window background opacity (`0.0` transparent … `1.0` opaque). Ghostty
    /// `background-opacity`. Only cells left on the *default* background go
    /// translucent; see [`Config::background_opacity_cells`].
    ///
    /// *Whether the window can be transparent at all is fixed at startup (the
    /// framebuffer is requested opaque when this is `1.0`), so crossing the `1.0`
    /// line needs a restart — as it does on Ghostty/macOS. The value itself
    /// live-reloads.*
    pub background_opacity: f32,
    /// Extend `background-opacity` to cells that set their own background color.
    /// Ghostty `background-opacity-cells` (default false, i.e. programs that
    /// repaint their background — Neovim, tmux — stay opaque by design).
    pub background_opacity_cells: bool,
    /// Opacity of an *unfocused* split, dimmed so the focused one stands out.
    /// `1.0` disables the effect. Ghostty `unfocused-split-opacity`, including its
    /// unusual `0.15` floor (fully transparent looks broken, so it is disallowed).
    pub unfocused_split_opacity: f32,
    /// Color of the rectangle painted over an unfocused split to dim it; `None`
    /// uses the pane's background. Ghostty `unfocused-split-fill`.
    pub unfocused_split_fill: Option<Rgb>,
    /// Cursor opacity (`0.0`…`1.0`). Ghostty `cursor-opacity`. Applies only to a
    /// focused pane's solid cursor; an unfocused hollow cursor stays opaque.
    pub cursor_opacity: f32,
    /// Opacity of faint/dim (SGR 2) text. Ghostty `faint-opacity`.
    pub faint_opacity: f32,
    /// Backdrop blur behind a translucent window. Ghostty `background-blur`; on
    /// Windows this drives the DWM acrylic/mica backdrop. Has no visible effect
    /// unless `background-opacity` is below `1.0` — the blur shows *through* the
    /// window, so there must be something to see through.
    pub background_blur: BackgroundBlur,
    /// Path to a PNG or JPEG painted behind the grid. Ghostty `background-image`;
    /// a relative path resolves against the config directory. Stored raw and
    /// resolved at the use site, like [`Self::bell_audio_path`].
    pub background_image: Option<String>,
    /// Opacity of that image *relative to* `background-opacity`. Ghostty
    /// `background-image-opacity`: `1.0` lays the image over the background
    /// color and then applies `background-opacity` to the pair; below `1.0`
    /// mixes it with the color first. Values **above** `1.0` are legal and make
    /// the image more opaque than the color behind it (Ghostty's own example:
    /// `background-opacity = 0.5` with `1.5` here gives the image `0.75`).
    pub background_image_opacity: f32,
    /// Where the image sits when it doesn't fill the area. Ghostty
    /// `background-image-position`.
    pub background_image_position: BackgroundImagePosition,
    /// How the image is scaled to the terminal area. Ghostty
    /// `background-image-fit`.
    pub background_image_fit: BackgroundImageFit,
    /// Tile the image to fill the space a non-covering fit leaves. Ghostty
    /// `background-image-repeat`.
    pub background_image_repeat: bool,
    /// Maximum scrollback retained per pane, in **bytes** — the same unit as
    /// Ghostty's `scrollback-limit`, so a Ghostty config transposes exactly.
    ///
    /// This was previously documented here as a *line* count, on the strength of
    /// the C header's "Maximum number of lines to keep in scrollback history".
    /// That header is wrong: the value is passed straight to `Screen.init`,
    /// whose own comment reads "max_scrollback is the amount of scrollback to
    /// keep in **bytes**" — and it was measured, too (a limit of 10 retained
    /// ~7,200 rows). Small values are also floored: the engine keeps at least
    /// one page, so a limit below one page's worth buys nothing.
    pub scrollback_limit: usize,
    /// Ghostty `scrollback-limit-lines`; `None` (the default) is `unlimited`.
    pub scrollback_limit_lines: Option<usize>,
    /// Ghostty `scrollback-compression` (default on).
    pub scrollback_compression: bool,
    /// Ghostty `title-report`: answer `CSI 21 t`. Off by default (it can inject
    /// a program-chosen title back into the shell's input).
    pub title_report: bool,
    /// Ghostty `vt-kam-allowed`: honor ANSI mode 2 (keyboard lock).
    pub vt_kam_allowed: bool,
    /// Ghostty `grapheme-width-method`: `unicode` (true, default) or `legacy`.
    pub grapheme_unicode: bool,
    /// Total bytes of image data (kitty graphics) retained per terminal screen.
    /// Ghostty `image-storage-limit`.
    ///
    /// **Zero disables the image protocols entirely** and deletes everything
    /// already stored — that is Ghostty's documented behavior, and it is also
    /// libghostty's starting state, so inline images do not work at all until
    /// this is applied. The limit is per screen, so the effective budget per
    /// pane is double (primary + alternate).
    pub image_storage_limit: u32,
    /// **giest-specific** `conpty-passthrough = auto | true | false`: which
    /// ConPTY carries the shell. The inbox conhost re-renders output and strips
    /// APC (kitty graphics) and ENQ; a `conpty.dll` + `OpenConsole.exe` pair
    /// (1.22+) placed next to `giest.exe` forwards them. `auto`/`true` use that
    /// pair when present (and request `PSEUDOCONSOLE_PASSTHROUGH_MODE`, which
    /// only 1.17–1.21 OpenConsole builds honour); `false` forces the inbox
    /// conhost. Startup-only: the ConPTY library is loaded once per process.
    pub conpty_passthrough: ConptyPassthrough,
    /// Background color of selected cells. Ghostty `selection-background`.
    pub selection_bg: Rgb,
    /// Text color over a selection; `None` keeps each cell's own foreground.
    /// Ghostty `selection-foreground`.
    pub selection_fg: Option<Rgb>,
    /// Where a finished selection is copied automatically. Ghostty
    /// `copy-on-select`; see [`CopyOnSelect`].
    pub copy_on_select: CopyOnSelect,
    /// `key-remap` lines, in order (later wins for the same source).
    pub key_remap: Vec<crate::keyremap::Remap>,
    /// Ghostty `mouse-shift-capture`.
    pub mouse_shift_capture: MouseShiftCapture,
    /// Ghostty `click-repeat-interval`, in ms. `0` = the OS double-click time.
    pub click_repeat_interval: u32,
    /// Ghostty `command-palette-entry`: whether the built-in rows are shown
    /// (`clear` turns them off, an empty value restores them).
    pub palette_defaults: bool,
    /// Ghostty `command-palette-entry`: custom rows, after the defaults.
    pub palette_entries: Vec<PaletteEntry>,
    /// Ghostty `clipboard-codepoint-map`.
    pub clipboard_codepoint_map: Vec<ClipboardMap>,
    /// Ghostty `font-codepoint-map`.
    pub font_codepoint_map: Vec<FontCodepointMap>,
    /// Ghostty `font-shaping-break = cursor`: shape the cursor cell on its own.
    pub font_shaping_break_cursor: bool,
    /// Ghostty `font-thicken`.
    pub font_thicken: bool,
    /// Ghostty `font-thicken-strength` (0..=255).
    pub font_thicken_strength: u8,
    /// Ghostty `cursor-click-to-move`.
    pub cursor_click_to_move: bool,
    /// What a right-click in a terminal pane does. Ghostty `right-click-action`.
    pub right_click_action: RightClickAction,
    /// What a middle-click in a terminal pane does. Ghostty `middle-click-action`.
    pub middle_click_action: MiddleClickAction,
    /// Configured default shell (name or path); `None` auto-detects. Ghostty
    /// `command`.
    pub shell: Option<String>,
    /// User keybind overrides as raw `(trigger, action)` pairs, applied over the
    /// built-in keymap by the app (`crate::keybind::Keymap::from_config`).
    /// Ghostty `keybind` (repeatable).
    pub keybinds: Vec<(String, String)>,
    /// Precision of OSC color-query replies. Ghostty `osc-color-report-format`.
    pub osc_color_report_format: OscColorReportFormat,
    /// When to confirm before closing a pane/tab/window. Ghostty
    /// `confirm-close-surface`.
    pub confirm_close: ConfirmClose,
    /// Whether the window/tab/split layout is saved on exit and restored at the
    /// next launch. Ghostty `window-save-state`.
    pub window_save_state: WindowSaveState,
    /// The `adjust-*` font/cell metric adjustments.
    pub adjust: MetricAdjust,
    /// Which screen edge the quick terminal drops from. Ghostty
    /// `quick-terminal-position`.
    pub quick_terminal_position: crate::quickterm::Position,
    /// The quick terminal's size on its primary (and optionally secondary) axis.
    /// Ghostty `quick-terminal-size`.
    pub quick_terminal_size: crate::quickterm::QuickSize,
    /// Whether the quick terminal hides itself when it loses focus. Ghostty
    /// `quick-terminal-autohide`, whose default is **false** off macOS.
    pub quick_terminal_autohide: bool,
    /// Duration of the quick terminal's slide in/out, in seconds; 0 disables
    /// it. Ghostty `quick-terminal-animation-duration`.
    pub quick_terminal_animation_duration: f64,
    /// Ghostty `window-decoration`. `none` removes the native caption and
    /// border; every other value (Windows has one decoration system) keeps it.
    pub window_decoration: WindowDecoration,
    /// Ghostty `window-titlebar-background` / `-foreground`. Applied through
    /// DWM (`DWMWA_CAPTION_COLOR` / `DWMWA_TEXT_COLOR`), which is Windows 11
    /// only; older systems ignore them.
    pub window_titlebar_background: Option<Rgb>,
    pub window_titlebar_foreground: Option<Rgb>,
    /// Ghostty `window-show-tab-bar`.
    pub window_show_tab_bar: ShowTabBar,
    /// Start new windows maximized / fullscreen. Ghostty `maximize` /
    /// `fullscreen` (every `non-native*` value behaves as `true`, as upstream
    /// documents for non-macOS platforms).
    pub maximize: bool,
    pub fullscreen: bool,
    /// A fixed window title that overrides everything the shell sets. Ghostty
    /// `title`.
    pub title: Option<String>,
    /// Ghostty `window-subtitle`: `true` means `working-directory`.
    pub window_subtitle: bool,
    /// Font family for the tab strip's titles. Ghostty
    /// `window-title-font-family`.
    pub window_title_font_family: Option<String>,
    /// Resize the window in whole-cell steps. Ghostty `window-step-resize`.
    pub window_step_resize: bool,
    /// Present with vsync. Ghostty `window-vsync`; startup-only in giest.
    pub window_vsync: bool,
    /// Ghostty `quit-after-last-window-closed` (+ `-delay`, in milliseconds).
    pub quit_after_last_window_closed: bool,
    pub quit_after_last_window_closed_delay_ms: Option<u64>,
    /// Ghostty `initial-window`.
    pub initial_window: bool,
    /// Ghostty `split-preserve-zoom = navigation`.
    pub split_preserve_zoom_navigation: bool,
    /// When to show the grid-size overlay on resize. Ghostty `resize-overlay`.
    pub resize_overlay: ResizeOverlay,
    /// Where that overlay sits in the pane. Ghostty `resize-overlay-position`.
    pub resize_overlay_position: ResizeOverlayPosition,
    /// How long the overlay stays up, in milliseconds. Ghostty
    /// `resize-overlay-duration`; clamped to a range that's actually perceivable.
    pub resize_overlay_duration_ms: u64,
    /// Whether panes show a scrollbar. Ghostty `scrollbar`.
    pub scrollbar: Scrollbar,
    /// Clipboard permissions and paste protection. Ghostty `clipboard-read`,
    /// `clipboard-write`, `clipboard-trim-trailing-spaces`,
    /// `clipboard-paste-protection`, `clipboard-paste-bracketed-safe`.
    pub clipboard: ClipboardPolicy,
    /// Whether programs may raise desktop notifications (`OSC 9`, `OSC 777`).
    /// Ghostty `desktop-notifications`; `true` by default there and here.
    pub desktop_notifications: bool,
    /// Ghostty `link-osc8`: OSC 8 hyperlinks are clickable.
    pub link_osc8: bool,
    /// Ghostty `link-url`: bare URLs in the text are clickable.
    pub link_url: bool,
    /// Initial window size in terminal **cells**; `0` means "let the OS decide".
    /// Ghostty `window-width` / `window-height`, including its 10×4 minimum.
    /// Applies to a new window only — resizing later is the user's business.
    pub window_width: u32,
    pub window_height: u32,
    /// Initial window position in pixels from the primary monitor's top-left.
    /// Ghostty `window-position-x` / `-y`; **both** must be set or neither
    /// applies, which is upstream's rule.
    pub window_position_x: Option<i16>,
    pub window_position_y: Option<i16>,
    /// Hide the pointer while typing, until the mouse moves again. Ghostty
    /// `mouse-hide-while-typing`; `false` by default there and here.
    pub mouse_hide_while_typing: bool,
    /// Whether programs may receive mouse events at all. Ghostty
    /// `mouse-reporting`; `false` makes the mouse always select, whatever the
    /// program asks for. Runtime-toggleable via `toggle_mouse_reporting`.
    pub mouse_reporting: bool,
    /// Wheel-distance multipliers. Ghostty `mouse-scroll-multiplier`.
    pub mouse_scroll_multiplier: MouseScrollMultiplier,
    /// When the viewport snaps back to the live edge. Ghostty
    /// `scroll-to-bottom`.
    pub scroll_to_bottom: ScrollToBottom,
    /// Focus the split under the pointer without clicking. Ghostty
    /// `focus-follows-mouse`.
    pub focus_follows_mouse: bool,
    /// Shadertoy-format GLSL post-process shaders, in the order they run.
    /// Ghostty `custom-shader` (repeatable); relative paths resolve against the
    /// config directory.
    pub custom_shaders: Vec<String>,
    /// Whether a custom shader animates. Ghostty `custom-shader-animation`.
    pub custom_shader_animation: CustomShaderAnimation,
    /// Whether programs may drive a progress indicator with ConEmu's `OSC 9;4`.
    /// Ghostty `progress-style` (a bool despite the name); `true` by default.
    pub progress_style: bool,
    /// When a finished command notifies. Ghostty `notify-on-command-finish`;
    /// `never` by default there and here, so this is opt-in.
    pub notify_on_command_finish: NotifyOnCommandFinish,
    /// How it notifies. Ghostty `notify-on-command-finish-action`.
    pub notify_on_command_finish_action: NotifyOnCommandFinishAction,
    /// How long a command must have run to be worth reporting. Ghostty
    /// `notify-on-command-finish-after`, default 5 s.
    pub notify_on_command_finish_after_ms: u64,
    /// How long an undoable operation stays undoable, in milliseconds. Ghostty
    /// `undo-timeout`, default 5 s. **Zero disables undo**, which is upstream's
    /// documented meaning rather than a giest shortcut — and it matters here
    /// because an undo entry holds a live shell open until it expires.
    pub undo_timeout_ms: u64,
    /// Keep a pane open after its shell exits, until a key is pressed.
    /// Ghostty `wait-after-command`.
    pub wait_after_command: bool,
    /// Ghostty `shell-integration`: which injection scheme; `none` disables every
    /// hook giest injects (pwsh/cmd prompt hooks and the WSL scripts).
    pub shell_integration: crate::profiles::ShellIntegration,
    /// Ghostty `shell-integration-features`.
    pub shell_integration_features: crate::profiles::ShellFeatures,
    /// A non-zero exit at or under this many ms is "abnormal": the pane stays
    /// open with an error bar even without `wait_after_command`, so a bad
    /// `command` is visible instead of a pane flashing shut. Ghostty
    /// `abnormal-command-exit-runtime`.
    pub abnormal_command_exit_runtime_ms: u32,
    /// Extra environment for spawned shells, in insertion order (a re-set key
    /// keeps its slot). Ghostty `env`.
    pub env: Vec<(String, String)>,
    /// Data written to each new shell at startup, concatenated. Ghostty `input`.
    pub input: Vec<InputSource>,
    /// Like `command` (`shell` here), but only for the first surface created at
    /// startup. Ghostty `initial-command`.
    pub initial_command: Option<String>,
    /// Which bell effects fire on BEL. Ghostty `bell-features`.
    pub bell: BellFeatures,
    /// Which in-app toasts are shown. Ghostty `app-notifications`.
    pub app_notifications: AppNotifications,
    /// Problems met while loading (unknown keys, bad values, unreadable
    /// includes, a missing theme), in order. Not a config key: it is what the
    /// config-errors dialog lists. Empty for a clean load.
    pub diagnostics: Vec<String>,
    /// Sound file played when `bell-features` includes `audio`. Ghostty
    /// `bell-audio-path`; a relative path resolves against the config directory.
    pub bell_audio_path: Option<String>,
    /// Ghostty `bell-audio-volume`. **Parsed and stored but not honored**: the
    /// Windows playback path (`PlaySoundW`) has no volume parameter, so honoring
    /// this needs a real audio backend. Kept so a Ghostty config neither warns
    /// nor silently loses the value.
    pub bell_audio_volume: f32,
    /// Whether a new tab inherits the focused pane's working directory (via OSC
    /// 7). Ghostty `tab-inherit-working-directory`.
    pub tab_inherit_working_directory: bool,
    /// Same, for a new split. Ghostty `split-inherit-working-directory`.
    pub split_inherit_working_directory: bool,
    /// Same, for a new window. Ghostty `window-inherit-working-directory`.
    pub window_inherit_working_directory: bool,
    /// Raw `config-file` specs collected from the body currently being parsed —
    /// a *staging* list, not a setting. [`Config::apply_body`] drains it after
    /// every file, so it is empty in a fully loaded config. Ghostty
    /// `config-file`; see [`Config::load_from_file`] for the traversal.
    pub config_file: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_points: 16.0,
            font_family: Vec::new(),
            font_synthetic_style: SyntheticStyle::default(),
            // Empty = "use the engine's defaults", which are Ghostty's own list.
            // Storing the default list here instead would duplicate it, and the
            // two copies would drift the first time upstream changed one.
            selection_word_chars: Vec::new(),
            selection_clear_on_typing: true,
            selection_clear_on_copy: false,
            // Upstream's defaults: an amber match, a warmer current match, both
            // with black text.
            search_bg: TerminalColor::Color(Rgb::new(0xFF, 0xE0, 0x82)),
            search_fg: TerminalColor::Color(Rgb::new(0, 0, 0)),
            search_selected_bg: TerminalColor::Color(Rgb::new(0xF2, 0xA5, 0x7E)),
            search_selected_fg: TerminalColor::Color(Rgb::new(0, 0, 0)),
            cursor_text: None,
            split_divider_color: None,
            new_tab_position: NewTabPosition::default(),
            window_padding_balance: PaddingBalance::default(),
            working_directory: None,
            font_family_bold: None,
            font_family_italic: None,
            font_family_bold_italic: None,
            font_features: Vec::new(),
            font_styles: Default::default(),
            font_variations: Default::default(),
            fg: Rgb::new(0xc5, 0xc8, 0xc6),
            bg: Rgb::new(0x10, 0x12, 0x18),
            palette: xterm_palette(GIEST_ANSI16),
            palette_set: [0; 4],
            palette_generate: false,
            palette_harmonious: false,
            // Ghostty's own defaults. giest used to ship 20 here so the 12pt
            // scrollbar could sit entirely inside the padding gutter, but that
            // put a lopsided 20/2 frame around every pane. The bar is an overlay
            // now (see the scrollbar metrics in `app.rs`), which is what Ghostty
            // does too, so the padding no longer has to pay for it.
            padding_x: 2.0,
            padding_y: 2.0,
            window_theme: WindowTheme::Auto,
            text_gamma: 1.3,
            cursor: None,
            cursor_style: CursorShape::Block,
            cursor_style_blink: None,
            bold_color: BoldColor::None,
            min_contrast: 1.0,
            background_opacity: 1.0,
            background_opacity_cells: false,
            unfocused_split_opacity: 0.7,
            unfocused_split_fill: None,
            cursor_opacity: 1.0,
            faint_opacity: 0.5,
            background_blur: BackgroundBlur::Off,
            background_image: None,
            background_image_opacity: 1.0,
            background_image_position: BackgroundImagePosition::Center,
            background_image_fit: BackgroundImageFit::Contain,
            background_image_repeat: false,
            scrollback_limit: 10_000,
            scrollback_limit_lines: None,
            scrollback_compression: true,
            title_report: false,
            vt_kam_allowed: false,
            grapheme_unicode: true,
            // Ghostty's default: 320 MB (decimal), per screen.
            image_storage_limit: 320 * 1000 * 1000,
            conpty_passthrough: ConptyPassthrough::Auto,
            selection_bg: Rgb::new(0x38, 0x5a, 0x9c),
            selection_fg: None,
            copy_on_select: CopyOnSelect::None,
            key_remap: Vec::new(),
            mouse_shift_capture: MouseShiftCapture::False,
            click_repeat_interval: 0,
            palette_defaults: true,
            palette_entries: Vec::new(),
            clipboard_codepoint_map: Vec::new(),
            font_codepoint_map: Vec::new(),
            font_shaping_break_cursor: true,
            font_thicken: false,
            font_thicken_strength: 255,
            cursor_click_to_move: true,
            right_click_action: RightClickAction::ContextMenu,
            middle_click_action: MiddleClickAction::PrimaryPaste,
            shell: None,
            keybinds: Vec::new(),
            osc_color_report_format: OscColorReportFormat::Bits16,
            confirm_close: ConfirmClose::WhenBusy,
            window_save_state: WindowSaveState::Default,
            adjust: MetricAdjust::default(),
            quick_terminal_position: crate::quickterm::Position::Top,
            quick_terminal_size: crate::quickterm::QuickSize::default(),
            // Ghostty's non-macOS default. A global hotkey is the only way back
            // to a hidden quick terminal, so hiding it on every focus change is
            // the more surprising behaviour of the two.
            quick_terminal_autohide: false,
            quick_terminal_animation_duration: 0.2,
            window_decoration: WindowDecoration::Auto,
            window_titlebar_background: None,
            window_titlebar_foreground: None,
            // Divergence: upstream's default is `auto`. giest's tab strip also
            // carries the new-tab profile picker, so hiding it with one tab would
            // hide the only mouse route to cmd / WSL / ... -- keep it by default.
            window_show_tab_bar: ShowTabBar::Always,
            maximize: false,
            fullscreen: false,
            title: None,
            window_subtitle: false,
            window_title_font_family: None,
            window_step_resize: false,
            window_vsync: true,
            // Upstream's default is `builtin.os.tag == .linux`, i.e. "the
            // platform convention"; the Windows convention is to quit.
            quit_after_last_window_closed: true,
            quit_after_last_window_closed_delay_ms: None,
            initial_window: true,
            split_preserve_zoom_navigation: false,
            resize_overlay: ResizeOverlay::AfterFirst,
            resize_overlay_position: ResizeOverlayPosition::Center,
            resize_overlay_duration_ms: 750,
            scrollbar: Scrollbar::System,
            // Ghostty's defaults exactly: reads are asked for (they leak the
            // clipboard *out*), writes are allowed (they only overwrite it).
            clipboard: ClipboardPolicy {
                read: ClipboardAccess::Ask,
                write: ClipboardAccess::Allow,
                trim_trailing_spaces: true,
                paste_protection: true,
                paste_bracketed_safe: true,
                write_limit: Some(64 << 20),
            },
            desktop_notifications: true,
            link_osc8: true,
            link_url: true,
            window_width: 0,
            window_height: 0,
            window_position_x: None,
            window_position_y: None,
            mouse_hide_while_typing: false,
            mouse_reporting: true,
            mouse_scroll_multiplier: MouseScrollMultiplier::default(),
            scroll_to_bottom: ScrollToBottom::default(),
            focus_follows_mouse: false,
            custom_shaders: Vec::new(),
            custom_shader_animation: CustomShaderAnimation::True,
            progress_style: true,
            notify_on_command_finish: NotifyOnCommandFinish::Never,
            notify_on_command_finish_action: NotifyOnCommandFinishAction::default(),
            notify_on_command_finish_after_ms: 5_000,
            undo_timeout_ms: 5_000,
            wait_after_command: false,
            shell_integration: crate::profiles::ShellIntegration::Detect,
            shell_integration_features: crate::profiles::ShellFeatures::default(),
            abnormal_command_exit_runtime_ms: 250,
            env: Vec::new(),
            input: Vec::new(),
            initial_command: None,
            bell: BellFeatures::default(),
            app_notifications: AppNotifications::default(),
            diagnostics: Vec::new(),
            bell_audio_path: None,
            bell_audio_volume: 0.5,
            tab_inherit_working_directory: true,
            split_inherit_working_directory: true,
            window_inherit_working_directory: true,
            config_file: Vec::new(),
        }
    }
}

impl Config {
    /// Load configuration, applying the config file's overrides over the
    /// defaults. A missing file is fine; unreadable lines are logged and skipped.
    /// The palette the terminal should use: `palette` as configured, or with
    /// indices 16-255 generated from the base 16 when `palette-generate` is on.
    /// Uses libghostty's own generator (CIELAB cube + bg-to-fg ramp), so the
    /// result matches Ghostty exactly; explicitly set entries are kept.
    pub fn effective_palette(&self) -> [Rgb; 256] {
        if !self.palette_generate {
            return self.palette;
        }
        use libghostty_vt::style::{Palette, PaletteIndex, PaletteMask, RgbColor};
        let c = |x: Rgb| RgbColor { r: x.r, g: x.g, b: x.b };
        let mut skip = PaletteMask::new();
        for i in 0..256usize {
            if self.palette_set[i / 64] & (1 << (i % 64)) != 0 {
                skip.set(PaletteIndex(i as u8));
            }
        }
        let base = Palette(self.palette.map(c));
        let out = Palette::generate(Some(&base), Some(&skip), c(self.bg), c(self.fg), self.palette_harmonious);
        out.0.map(|x| Rgb { r: x.r, g: x.g, b: x.b })
    }

    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        Self::load_from_file(&path)
    }

    /// Load `root` plus every file it pulls in with `config-file`, and the files
    /// *those* pull in, over the built-in defaults.
    ///
    /// The traversal is Ghostty's, which is subtle in two ways worth stating:
    /// an included file is loaded **after the whole file that named it** (so its
    /// keys win over that file's, not just over the lines above the
    /// `config-file` line), and nested includes join the *end* of one shared
    /// queue — i.e. breadth-first, not depth-first. Both fall out of upstream's
    /// `loadRecursiveFiles`, which walks a single growing list.
    ///
    /// A path is resolved against the directory of the file that named it. A
    /// `?` prefix makes a missing file silent. A file already loaded is skipped
    /// with a message, so a cycle terminates instead of hanging.
    pub fn load_from_file(root: &Path) -> Self {
        take_diagnostics();
        let mut cfg = Self::load_files(root);
        cfg.diagnostics = take_diagnostics();
        cfg
    }

    fn load_files(root: &Path) -> Self {
        let mut cfg = Self::default();
        let mut queue: std::collections::VecDeque<(PathBuf, bool)> = Default::default();
        let mut seen: std::collections::HashSet<PathBuf> = Default::default();
        seen.insert(load_key(root));

        // A missing *root* config is normal (no file yet) and stays silent;
        // a missing *included* one is a typo the user asked for by name.
        let Ok(text) = std::fs::read_to_string(root) else {
            return cfg;
        };
        queue.extend(cfg.apply_body(&text, root.parent()));

        while let Some((path, optional)) = queue.pop_front() {
            if !seen.insert(load_key(&path)) {
                diag!(
                    "giest: config-file {}: already loaded (cycle), ignoring",
                    path.display()
                );
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let more = cfg.apply_body(&text, path.parent());
                    queue.extend(more);
                }
                Err(e) if optional && e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => diag!("giest: error reading config-file {}: {e}", path.display()),
            }
        }
        cfg
    }

    /// Build a [`Config`] by applying a Ghostty-format config body over the
    /// built-in defaults. This is the entry point [`load`](Self::load) uses after
    /// reading the file; it is public so integration tests and tooling can drive
    /// the parser directly without touching the filesystem.
    pub fn from_ghostty_config(text: &str) -> Self {
        let mut cfg = Self::default();
        // No path, so nothing to resolve a relative `config-file` against: the
        // includes this returns are dropped. `load_from_file` is the entry point
        // that follows them.
        take_diagnostics();
        let _ = cfg.apply_body(text, config_dir().as_deref());
        cfg.diagnostics = take_diagnostics();
        cfg
    }

    /// Apply one config body over this config and return the files it asks for
    /// via `config-file`, resolved against `base` (the directory of the file the
    /// body came from).
    fn apply_body(&mut self, text: &str, base: Option<&Path>) -> Vec<(PathBuf, bool)> {
        // Resolve `theme = ...` first so the theme's colors/palette form a base
        // that the user's own keys then override, regardless of line order
        // (matching Ghostty, where an explicit `background` wins over the theme).
        if let Some(spec) = config_value(text, "theme") {
            self.apply_theme_spec(&spec);
        }
        self.config_file.clear();
        self.parse(text);
        std::mem::take(&mut self.config_file)
            .iter()
            .filter_map(|spec| parse_include(spec, base))
            .collect()
    }

    /// Apply a `theme = ...` spec: resolve it to a theme file and parse that
    /// file's keys into this config as a base. Supports a bare theme name (looked
    /// up in the config dir's `themes/`), an explicit path, and Ghostty's
    /// `light:Foo,dark:Bar` dual form (the dark variant is used; system
    /// appearance switching is a follow-up). Unresolvable themes are ignored.
    fn apply_theme_spec(&mut self, spec: &str) {
        let name = select_theme_variant(spec);
        if name.is_empty() {
            return;
        }
        let Some(path) = resolve_theme_path(&name) else {
            diag!("giest: theme '{name}' not found (looked in <config-dir>/themes/)");
            return;
        };
        match std::fs::read_to_string(&path) {
            // Theme files are giest/Ghostty config bodies (palette/fg/bg/…). A
            // nested `theme` key inside a theme file is skipped by `apply`, so
            // this cannot recurse.
            Ok(body) => self.parse(&body),
            Err(e) => diag!("giest: could not read theme file {}: {e}", path.display()),
        }
    }

    /// Parse a Ghostty-format config body, applying each `key = value` line.
    /// Blank lines and `#` comment lines are skipped; malformed lines and
    /// unknown keys are logged and ignored (the rest of the file still applies).
    fn parse(&mut self, text: &str) {
        let defaults = Config::default();
        // Strip a UTF-8 BOM. Notepad and `Set-Content -Encoding utf8` on Windows
        // PowerShell both write one, and it would otherwise glue itself to the
        // first key — producing "unsupported config key" for a line that looks
        // perfectly correct on screen.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                diag!("giest: ignoring malformed config line {}: {raw}", i + 1);
                continue;
            };
            self.apply(key.trim(), unquote(value.trim()), &defaults);
        }
    }

    /// Apply a single `key`/`value` pair via the declarative [`SETTERS`] table.
    /// An empty `value` resets the key to its `defaults` value (Ghostty
    /// semantics); an unparseable value keeps the current setting. `theme` is
    /// resolved out-of-band in [`Self::from_ghostty_config`], so it is skipped
    /// here (this also stops a theme file's own `theme` key from recursing).
    fn apply(&mut self, key: &str, value: &str, defaults: &Config) {
        if key == "theme" {
            return;
        }
        match SETTERS.iter().find(|(k, _)| *k == key) {
            Some((_, set)) => set(self, value, defaults),
            None => diag!("giest: ignoring unsupported config key '{key}'"),
        }
    }
}

/// Applies one `key = value` to a [`Config`]. `value` is already trimmed and
/// unquoted; an empty value resets the key to its default (read from the
/// `defaults` config) per Ghostty semantics.
type Setter = fn(&mut Config, &str, &Config);

/// Declarative key → setter table. Adding a config key is one entry here plus a
/// field on [`Config`] and its default — the parser, reset-on-empty, and the
/// "unsupported key" handling all come for free. Each closure is non-capturing
/// so it coerces to a plain `fn` pointer.
const SETTERS: &[(&str, Setter)] = &[
    ("font-size", |c, v, d| {
        if v.is_empty() {
            c.font_points = d.font_points;
        } else if let Ok(n) = v.parse::<f32>() {
            if n > 0.0 {
                c.font_points = n;
            }
        }
    }),
    // Repeatable, like `palette` and `keybind`: each line appends to the
    // fallback chain. An empty value resets the whole list, which is the only
    // way to undo an earlier line (Ghostty's RepeatableString does the same).
    ("font-family", |c, v, _d| {
        if v.is_empty() {
            c.font_family.clear();
        } else {
            c.font_family.push(v.to_string());
        }
    }),
    ("selection-clear-on-typing", |c, v, d| {
        c.selection_clear_on_typing = parse_bool(v, d.selection_clear_on_typing)
    }),
    ("selection-clear-on-copy", |c, v, d| {
        c.selection_clear_on_copy = parse_bool(v, d.selection_clear_on_copy)
    }),
    ("search-background", |c, v, d| {
        c.search_bg = terminal_color(v, c.search_bg, d.search_bg)
    }),
    ("search-foreground", |c, v, d| {
        c.search_fg = terminal_color(v, c.search_fg, d.search_fg)
    }),
    ("search-selected-background", |c, v, d| {
        c.search_selected_bg = terminal_color(v, c.search_selected_bg, d.search_selected_bg)
    }),
    ("cursor-text", |c, v, d| {
        c.cursor_text = if v.is_empty() { d.cursor_text } else { TerminalColor::parse(v).or(c.cursor_text) };
    }),
    ("search-selected-foreground", |c, v, d| {
        c.search_selected_fg = terminal_color(v, c.search_selected_fg, d.search_selected_fg)
    }),
    ("split-divider-color", |c, v, d| {
        c.split_divider_color = match v {
            "" => d.split_divider_color,
            _ => parse_color(v).or(c.split_divider_color),
        }
    }),
    ("window-new-tab-position", |c, v, d| {
        c.new_tab_position = match v.to_ascii_lowercase().as_str() {
            "" => d.new_tab_position,
            "current" => NewTabPosition::Current,
            "end" => NewTabPosition::End,
            _ => c.new_tab_position,
        }
    }),
    ("window-padding-balance", |c, v, d| {
        c.window_padding_balance = match v.to_ascii_lowercase().as_str() {
            "" => d.window_padding_balance,
            "false" | "no" | "off" | "0" => PaddingBalance::None,
            "true" | "yes" | "on" | "1" => PaddingBalance::Balanced,
            "equal" => PaddingBalance::Equal,
            _ => c.window_padding_balance,
        }
    }),
    ("working-directory", |c, v, d| {
        c.working_directory = match v {
            "" => d.working_directory.clone(),
            // `inherit` is the launching process's directory, which is exactly
            // what `None` already means here.
            "inherit" => None,
            "home" => home_dir(),
            _ => match v.strip_prefix("~/").or_else(|| v.strip_prefix(r"~\")) {
                Some(rest) => home_dir().map(|h| h.join(rest)),
                None => Some(PathBuf::from(v)),
            },
        }
    }),
    ("selection-word-chars", |c, v, d| {
        c.selection_word_chars = match v {
            "" => d.selection_word_chars.clone(),
            _ => parse_word_chars(v),
        }
    }),
    ("font-synthetic-style", |c, v, d| {
        c.font_synthetic_style = match v {
            "" => d.font_synthetic_style,
            _ => parse_synthetic_style(v).unwrap_or_else(|| {
                diag!("giest: ignoring invalid font-synthetic-style '{v}'");
                c.font_synthetic_style
            }),
        }
    }),
    ("font-family-bold", |c, v, d| {
        c.font_family_bold = opt_string(v, &d.font_family_bold)
    }),
    ("font-family-italic", |c, v, d| {
        c.font_family_italic = opt_string(v, &d.font_family_italic)
    }),
    ("font-family-bold-italic", |c, v, d| {
        c.font_family_bold_italic = opt_string(v, &d.font_family_bold_italic)
    }),
    ("font-feature", |c, v, d| {
        // Repeatable: each value is a *comma*-separated list (Ghostty splits only
        // on commas, never whitespace, so a feature's own value syntax may contain
        // spaces, e.g. `liga off`). An empty value resets to the built-in defaults.
        if v.is_empty() {
            c.font_features = d.font_features.clone();
        } else {
            for tok in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                c.font_features.push(tok.to_string());
            }
        }
    }),
    ("font-style", |c, v, d| set_font_style(c, d, v, 0)),
    ("font-style-bold", |c, v, d| set_font_style(c, d, v, 1)),
    ("font-style-italic", |c, v, d| set_font_style(c, d, v, 2)),
    ("font-style-bold-italic", |c, v, d| set_font_style(c, d, v, 3)),
    ("font-variation", |c, v, d| set_font_variation(c, d, v, 0)),
    ("font-variation-bold", |c, v, d| set_font_variation(c, d, v, 1)),
    ("font-variation-italic", |c, v, d| set_font_variation(c, d, v, 2)),
    ("font-variation-bold-italic", |c, v, d| set_font_variation(c, d, v, 3)),
    ("foreground", |c, v, d| c.fg = color(v, d.fg, c.fg)),
    ("background", |c, v, d| c.bg = color(v, d.bg, c.bg)),
    ("cursor-color", |c, v, d| c.cursor = opt_color(v, d.cursor, c.cursor)),
    ("cursor-style", |c, v, d| {
        c.cursor_style = match v.to_ascii_lowercase().as_str() {
            "" => d.cursor_style,
            "block" => CursorShape::Block,
            "bar" => CursorShape::Bar,
            "underline" => CursorShape::Underline,
            // Ghostty accepts `block_hollow` as an explicit style too.
            "block_hollow" | "block-hollow" => CursorShape::HollowBlock,
            _ => c.cursor_style,
        }
    }),
    ("cursor-style-blink", |c, v, d| {
        c.cursor_style_blink = match v.to_ascii_lowercase().as_str() {
            "" => d.cursor_style_blink,
            "true" | "1" | "on" | "yes" => Some(true),
            "false" | "0" | "off" | "no" => Some(false),
            _ => c.cursor_style_blink,
        }
    }),
    ("bold-color", |c, v, d| {
        c.bold_color = match v.to_ascii_lowercase().as_str() {
            "" => d.bold_color,
            "bright" => BoldColor::Bright,
            _ => parse_color(v).map(BoldColor::Color).unwrap_or(c.bold_color),
        }
    }),
    ("bold-is-bright", |c, v, d| {
        // Deprecated Ghostty alias merged into `bold-color`: true ⇒ bright.
        c.bold_color = match v.to_ascii_lowercase().as_str() {
            "" => d.bold_color,
            "true" | "1" | "on" | "yes" => BoldColor::Bright,
            "false" | "0" | "off" | "no" => BoldColor::None,
            _ => c.bold_color,
        }
    }),
    ("minimum-contrast", |c, v, d| {
        c.min_contrast = ratio(v, d.min_contrast, c.min_contrast, 1.0, 21.0)
    }),
    ("background-opacity", |c, v, d| {
        c.background_opacity = ratio(v, d.background_opacity, c.background_opacity, 0.0, 1.0)
    }),
    ("background-opacity-cells", |c, v, d| {
        c.background_opacity_cells = if v.is_empty() {
            d.background_opacity_cells
        } else {
            parse_bool(v, c.background_opacity_cells)
        }
    }),
    ("unfocused-split-opacity", |c, v, d| {
        // Ghostty's floor is 0.15, not 0: a fully transparent split "looks very
        // weird", so it clamps up rather than allowing it.
        c.unfocused_split_opacity =
            ratio(v, d.unfocused_split_opacity, c.unfocused_split_opacity, 0.15, 1.0)
    }),
    ("unfocused-split-fill", |c, v, d| {
        c.unfocused_split_fill = opt_color(v, d.unfocused_split_fill, c.unfocused_split_fill)
    }),
    ("cursor-opacity", |c, v, d| {
        c.cursor_opacity = ratio(v, d.cursor_opacity, c.cursor_opacity, 0.0, 1.0)
    }),
    ("faint-opacity", |c, v, d| {
        c.faint_opacity = ratio(v, d.faint_opacity, c.faint_opacity, 0.0, 1.0)
    }),
    ("background-blur", |c, v, d| {
        // Ghostty's parse order — bool first, then the macOS glass names, then an
        // integer intensity. Its `parseBool` accepts only `1`/`t`/`true` (and the
        // `0`/`f`/`false` forms), so `1` means *true* (intensity 20) and only `2`
        // and up reach the numeric branch.
        c.background_blur = match v.to_ascii_lowercase().as_str() {
            "" => d.background_blur,
            "1" | "t" | "true" => BackgroundBlur::On,
            "0" | "f" | "false" => BackgroundBlur::Off,
            // macOS 26 glass effects: no Windows equivalent, and Ghostty itself
            // treats them as plain `true` off macOS since both imply some blur.
            "macos-glass-regular" | "macos-glass-clear" => BackgroundBlur::On,
            _ => v.parse::<u8>().map(BackgroundBlur::Radius).unwrap_or(c.background_blur),
        }
    }),
    ("background-image", |c, v, d| {
        c.background_image = opt_string(v, &d.background_image)
    }),
    ("background-image-opacity", |c, v, d| {
        // *Not* a 0..1 ratio: Ghostty documents values above 1.0 as meaningful
        // (they make the image more opaque than the color it sits on). Only the
        // negative side is nonsense, so that's the only end clamped — the upper
        // bound is a sanity cap, not a semantic one.
        c.background_image_opacity = ratio(
            v,
            d.background_image_opacity,
            c.background_image_opacity,
            0.0,
            16.0,
        )
    }),
    ("background-image-position", |c, v, d| {
        c.background_image_position = match v.to_ascii_lowercase().as_str() {
            "" => d.background_image_position,
            "top-left" => BackgroundImagePosition::TopLeft,
            "top-center" => BackgroundImagePosition::TopCenter,
            "top-right" => BackgroundImagePosition::TopRight,
            "center-left" => BackgroundImagePosition::CenterLeft,
            // Ghostty's enum has both spellings and treats them identically.
            "center" | "center-center" => BackgroundImagePosition::Center,
            "center-right" => BackgroundImagePosition::CenterRight,
            "bottom-left" => BackgroundImagePosition::BottomLeft,
            "bottom-center" => BackgroundImagePosition::BottomCenter,
            "bottom-right" => BackgroundImagePosition::BottomRight,
            _ => c.background_image_position,
        }
    }),
    ("background-image-fit", |c, v, d| {
        c.background_image_fit = match v.to_ascii_lowercase().as_str() {
            "" => d.background_image_fit,
            "contain" => BackgroundImageFit::Contain,
            "cover" => BackgroundImageFit::Cover,
            "stretch" => BackgroundImageFit::Stretch,
            "none" => BackgroundImageFit::None,
            _ => c.background_image_fit,
        }
    }),
    ("background-image-repeat", |c, v, d| {
        c.background_image_repeat = if v.is_empty() {
            d.background_image_repeat
        } else {
            parse_bool(v, c.background_image_repeat)
        }
    }),
    ("window-padding-x", |c, v, d| {
        c.padding_x = padding(v, d.padding_x, c.padding_x)
    }),
    ("window-padding-y", |c, v, d| {
        c.padding_y = padding(v, d.padding_y, c.padding_y)
    }),
    ("window-theme", |c, v, d| {
        c.window_theme = match v.to_ascii_lowercase().as_str() {
            "" => d.window_theme,
            // Ghostty's `auto` means "match the terminal background", which is
            // exactly what giest derives. `system` is accepted as its documented
            // alias but treated the same: following the OS is what produced a
            // light tab strip over a dark terminal.
            "auto" | "system" => WindowTheme::Auto,
            "dark" => WindowTheme::Dark,
            "light" => WindowTheme::Light,
            _ => c.window_theme,
        }
    }),
    ("text-gamma", |c, v, d| {
        c.text_gamma = ratio(v, d.text_gamma, c.text_gamma, 0.5, 3.0)
    }),
    ("palette", |c, v, d| {
        if v.is_empty() {
            c.palette = d.palette;
            c.palette_set = [0; 4];
        } else if let Some((idx, col)) = parse_palette_entry(v) {
            c.palette[idx as usize] = col;
            c.palette_set[idx as usize / 64] |= 1 << (idx as usize % 64);
        } else {
            diag!("giest: ignoring bad palette entry: {v}");
        }
    }),
    ("link-osc8", |c, v, d| c.link_osc8 = parse_bool(v, d.link_osc8)),
    ("link-url", |c, v, d| c.link_url = parse_bool(v, d.link_url)),
    ("clipboard-write-limit-bytes", |c, v, d| {
        c.clipboard.write_limit = match v {
            "" => d.clipboard.write_limit,
            "unlimited" => None,
            _ => v.parse().ok().map(Some).unwrap_or(d.clipboard.write_limit),
        };
    }),
    // Upstream renamed `scrollback-limit` to this (same unit, bytes) and added
    // `unlimited`; both spellings are accepted.
    ("scrollback-limit-bytes", |c, v, d| {
        c.scrollback_limit = match v {
            "" => d.scrollback_limit,
            "unlimited" => usize::MAX,
            _ => v.parse().unwrap_or(d.scrollback_limit),
        };
    }),
    ("scrollback-compression", |c, v, d| {
        c.scrollback_compression = parse_bool(v, d.scrollback_compression);
    }),
    ("scrollback-limit-lines", |c, v, d| {
        c.scrollback_limit_lines = match v {
            "" => d.scrollback_limit_lines,
            "unlimited" => None,
            _ => v.parse().ok().map(Some).unwrap_or(d.scrollback_limit_lines),
        };
    }),
    ("scrollback-limit", |c, v, d| {
        if v.is_empty() {
            c.scrollback_limit = d.scrollback_limit;
        } else if let Ok(n) = v.parse() {
            c.scrollback_limit = n;
        }
    }),
    ("image-storage-limit", |c, v, d| {
        if v.is_empty() {
            c.image_storage_limit = d.image_storage_limit;
        } else if let Ok(n) = v.parse() {
            c.image_storage_limit = n;
        }
    }),
    ("conpty-passthrough", |c, v, d| {
        c.conpty_passthrough = match v.to_ascii_lowercase().as_str() {
            "auto" => ConptyPassthrough::Auto,
            "1" | "t" | "true" => ConptyPassthrough::On,
            "0" | "f" | "false" => ConptyPassthrough::Off,
            _ => d.conpty_passthrough,
        }
    }),
    ("selection-background", |c, v, d| {
        c.selection_bg = color(v, d.selection_bg, c.selection_bg)
    }),
    ("selection-foreground", |c, v, d| {
        c.selection_fg = opt_color(v, d.selection_fg, c.selection_fg)
    }),
    ("copy-on-select", |c, v, d| {
        c.copy_on_select = match v.to_ascii_lowercase().as_str() {
            "" => d.copy_on_select,
            "false" | "0" | "off" | "no" | "none" => CopyOnSelect::None,
            // Upstream's `true` is `clipboard` everywhere but Linux; Windows
            // follows macOS (it has no system PRIMARY to prefer).
            "true" | "1" | "on" | "yes" | "clipboard" => CopyOnSelect::Clipboard,
            "primary" => CopyOnSelect::Primary,
            "both" => CopyOnSelect::Both,
            _ => c.copy_on_select,
        }
    }),
    ("key-remap", |c, v, _d| {
        if v.is_empty() {
            c.key_remap.clear();
        } else if let Some(r) = crate::keyremap::parse(v) {
            c.key_remap.push(r);
        }
    }),
    ("mouse-shift-capture", |c, v, d| {
        c.mouse_shift_capture = match v.to_ascii_lowercase().as_str() {
            "" => d.mouse_shift_capture,
            "false" => MouseShiftCapture::False,
            "true" => MouseShiftCapture::True,
            "always" => MouseShiftCapture::Always,
            "never" => MouseShiftCapture::Never,
            _ => c.mouse_shift_capture,
        }
    }),
    ("click-repeat-interval", |c, v, d| {
        c.click_repeat_interval = if v.is_empty() {
            d.click_repeat_interval
        } else {
            v.parse().unwrap_or(c.click_repeat_interval)
        }
    }),
    ("command-palette-entry", |c, v, _d| {
        if v.is_empty() {
            c.palette_defaults = true;
            c.palette_entries.clear();
        } else if v == "clear" {
            c.palette_defaults = false;
            c.palette_entries.clear();
        } else if let Some(e) = parse_command_palette_entry(v) {
            c.palette_entries.push(e);
        }
    }),
    ("clipboard-codepoint-map", |c, v, _d| {
        if v.is_empty() {
            c.clipboard_codepoint_map.clear();
        } else if let Some(m) = parse_clipboard_map(v) {
            c.clipboard_codepoint_map.push(m);
        }
    }),
    ("font-codepoint-map", |c, v, _d| {
        if v.is_empty() {
            c.font_codepoint_map.clear();
        } else if let Some((lhs, family)) = v.split_once('=')
            && let Some(ranges) = parse_codepoint_ranges(lhs)
            && !family.trim().is_empty()
        {
            c.font_codepoint_map.push(FontCodepointMap {
                ranges,
                family: family.trim().to_string(),
            });
        }
    }),
    ("font-shaping-break", |c, v, d| {
        if v.is_empty() {
            c.font_shaping_break_cursor = d.font_shaping_break_cursor;
            return;
        }
        // Upstream's packed-struct grammar: `opt`, `no-opt`, or a bare boolean
        // for every option. `cursor` is the only option.
        for part in v.split(',') {
            match part.trim().to_ascii_lowercase().as_str() {
                "cursor" | "true" => c.font_shaping_break_cursor = true,
                "no-cursor" | "false" => c.font_shaping_break_cursor = false,
                _ => {}
            }
        }
    }),
    ("font-thicken", |c, v, d| c.font_thicken = parse_bool(v, d.font_thicken)),
    ("font-thicken-strength", |c, v, d| {
        c.font_thicken_strength = if v.is_empty() {
            d.font_thicken_strength
        } else {
            v.parse().unwrap_or(c.font_thicken_strength)
        }
    }),
    ("cursor-click-to-move", |c, v, d| {
        c.cursor_click_to_move = parse_bool(v, d.cursor_click_to_move)
    }),
    ("right-click-action", |c, v, d| {
        c.right_click_action = match v.to_ascii_lowercase().as_str() {
            "" => d.right_click_action,
            "context-menu" => RightClickAction::ContextMenu,
            "copy" => RightClickAction::Copy,
            "paste" => RightClickAction::Paste,
            "copy-or-paste" => RightClickAction::CopyOrPaste,
            "ignore" => RightClickAction::Ignore,
            _ => c.right_click_action,
        }
    }),
    ("middle-click-action", |c, v, d| {
        c.middle_click_action = match v.to_ascii_lowercase().as_str() {
            "" => d.middle_click_action,
            "primary-paste" => MiddleClickAction::PrimaryPaste,
            "clipboard-paste" => MiddleClickAction::ClipboardPaste,
            "ignore" => MiddleClickAction::Ignore,
            _ => c.middle_click_action,
        }
    }),
    ("initial-command", |c, v, d| {
        c.initial_command = if v.is_empty() {
            d.initial_command.clone()
        } else {
            Some(v.to_string())
        }
    }),
    ("wait-after-command", |c, v, d| {
        c.wait_after_command = if v.is_empty() {
            d.wait_after_command
        } else {
            parse_bool(v, c.wait_after_command)
        }
    }),
    ("shell-integration", |c, v, d| {
        c.shell_integration = if v.is_empty() {
            d.shell_integration
        } else {
            crate::profiles::ShellIntegration::parse(v).unwrap_or_else(|| {
                eprintln!("giest: ignoring invalid shell-integration value '{v}'");
                c.shell_integration
            })
        }
    }),
    ("shell-integration-features", |c, v, d| {
        c.shell_integration_features = if v.is_empty() {
            d.shell_integration_features
        } else {
            crate::profiles::ShellFeatures::parse(v).unwrap_or_else(|| {
                eprintln!("giest: ignoring invalid shell-integration-features value '{v}'");
                c.shell_integration_features
            })
        }
    }),
    ("abnormal-command-exit-runtime", |c, v, d| {
        // A plain `u32` of milliseconds upstream, not a `Duration`.
        c.abnormal_command_exit_runtime_ms = if v.is_empty() {
            d.abnormal_command_exit_runtime_ms
        } else {
            v.parse().unwrap_or(c.abnormal_command_exit_runtime_ms)
        }
    }),
    ("env", |c, v, d| {
        // Upstream `RepeatableStringMap`: empty resets the map, `KEY=` removes
        // that key, and re-setting a key overwrites it.
        if v.is_empty() {
            c.env = d.env.clone();
        } else if let Some((key, val)) = v.split_once('=') {
            let key = key.trim();
            if key.is_empty() {
                return;
            }
            if val.is_empty() {
                c.env.retain(|(k, _)| k != key);
            } else if let Some(slot) = c.env.iter_mut().find(|(k, _)| k == key) {
                slot.1 = val.to_string();
            } else {
                c.env.push((key.to_string(), val.to_string()));
            }
        }
    }),
    ("input", |c, v, d| {
        // Repeatable; empty resets. An invalid escape rejects the value, as
        // upstream validates the string at config time.
        if v.is_empty() {
            c.input = d.input.clone();
        } else if let Some(src) = InputSource::parse(v) {
            c.input.push(src);
        } else {
            eprintln!("giest: ignoring invalid input value '{v}'");
        }
    }),
    ("command", |c, v, d| {
        c.shell = if v.is_empty() {
            d.shell.clone()
        } else {
            Some(v.to_string())
        }
    }),
    ("keybind", |c, v, d| {
        // Repeatable: each `keybind = <trigger>=<action>` appends one override.
        // An empty value (or `clear`) resets to the built-in keymap.
        if v.is_empty() || v.eq_ignore_ascii_case("clear") {
            c.keybinds = d.keybinds.clone();
        } else if let Some((trigger, action)) = v.split_once('=') {
            // Split on the **first** `=`, so a payload containing one
            // (`text:a=b`) survives intact.
            //
            // The action is `trim_start`ed, not fully trimmed: an action like
            // `text:hello ` carries its trailing space deliberately, and
            // `Action::from_name` trims the names that *should* be trimmed
            // itself. (The whole config value has already lost surrounding
            // whitespace by this point — upstream trims there too — so keeping a
            // trailing space needs the quoted form,
            // `keybind = "ctrl+k=text:hello "`, exactly as it does in Ghostty.)
            c.keybinds
                .push((trigger.trim().to_string(), action.trim_start().to_string()));
        } else {
            diag!("giest: ignoring malformed keybind (expected 'trigger=action'): {v}");
        }
    }),
    ("confirm-close-surface", |c, v, d| {
        c.confirm_close = match v.to_ascii_lowercase().as_str() {
            "" => d.confirm_close,
            "false" | "no" | "off" | "0" => ConfirmClose::Never,
            "true" | "yes" | "on" | "1" => ConfirmClose::WhenBusy,
            "always" => ConfirmClose::Always,
            _ => c.confirm_close,
        }
    }),
    // The `adjust-*` family. One entry per key rather than a shared prefix
    // handler: the table is the registry, and a typo'd key should be reported as
    // unsupported rather than silently matching a prefix and going nowhere.
    ("adjust-cell-width", |c, v, d| {
        c.adjust.cell_width = adjust_value(v, c.adjust.cell_width, d.adjust.cell_width)
    }),
    ("adjust-cell-height", |c, v, d| {
        c.adjust.cell_height = adjust_value(v, c.adjust.cell_height, d.adjust.cell_height)
    }),
    ("adjust-font-baseline", |c, v, d| {
        c.adjust.font_baseline = adjust_value(v, c.adjust.font_baseline, d.adjust.font_baseline)
    }),
    ("adjust-underline-position", |c, v, d| {
        c.adjust.underline_position =
            adjust_value(v, c.adjust.underline_position, d.adjust.underline_position)
    }),
    ("adjust-underline-thickness", |c, v, d| {
        c.adjust.underline_thickness = adjust_value(
            v,
            c.adjust.underline_thickness,
            d.adjust.underline_thickness,
        )
    }),
    ("adjust-strikethrough-position", |c, v, d| {
        c.adjust.strikethrough_position = adjust_value(
            v,
            c.adjust.strikethrough_position,
            d.adjust.strikethrough_position,
        )
    }),
    ("adjust-strikethrough-thickness", |c, v, d| {
        c.adjust.strikethrough_thickness = adjust_value(
            v,
            c.adjust.strikethrough_thickness,
            d.adjust.strikethrough_thickness,
        )
    }),
    ("adjust-overline-position", |c, v, d| {
        c.adjust.overline_position =
            adjust_value(v, c.adjust.overline_position, d.adjust.overline_position)
    }),
    ("adjust-overline-thickness", |c, v, d| {
        c.adjust.overline_thickness =
            adjust_value(v, c.adjust.overline_thickness, d.adjust.overline_thickness)
    }),
    ("adjust-cursor-thickness", |c, v, d| {
        c.adjust.cursor_thickness =
            adjust_value(v, c.adjust.cursor_thickness, d.adjust.cursor_thickness)
    }),
    ("adjust-cursor-height", |c, v, d| {
        c.adjust.cursor_height = adjust_value(v, c.adjust.cursor_height, d.adjust.cursor_height)
    }),
    ("adjust-box-thickness", |c, v, d| {
        c.adjust.box_thickness = adjust_value(v, c.adjust.box_thickness, d.adjust.box_thickness)
    }),
    ("adjust-icon-height", |c, v, d| {
        c.adjust.icon_height = adjust_value(v, c.adjust.icon_height, d.adjust.icon_height)
    }),
    ("quick-terminal-position", |c, v, d| {
        c.quick_terminal_position = match v {
            "" => d.quick_terminal_position,
            _ => crate::quickterm::Position::parse(v).unwrap_or(c.quick_terminal_position),
        }
    }),
    ("quick-terminal-size", |c, v, d| {
        c.quick_terminal_size = match v {
            "" => d.quick_terminal_size,
            _ => match crate::quickterm::QuickSize::parse(v) {
                Some(s) => s,
                None => {
                    // Ghostty makes a bare number a config *error*; giest logs
                    // and keeps the previous value, since it has no error UI.
                    diag!(
                        "giest: ignoring quick-terminal-size '{v}' \
                         (sizes need a % or px suffix, e.g. '25%' or '400px')"
                    );
                    c.quick_terminal_size
                }
            },
        }
    }),
    // Recognized so a transposed Ghostty config doesn't warn, but only `main` is
    // honored: `mouse` needs per-monitor enumeration giest has no handle for and
    // `macos-menu-bar` has no Windows meaning. Says so rather than silently
    // placing the window on the wrong screen.
    ("quick-terminal-screen", |_c, v, _d| {
        if !v.is_empty() && !v.eq_ignore_ascii_case("main") {
            diag!("giest: quick-terminal-screen '{v}' is not supported; using 'main'");
        }
    }),
    ("quick-terminal-autohide", |c, v, d| {
        c.quick_terminal_autohide = parse_bool(v, d.quick_terminal_autohide)
    }),
    ("window-save-state", |c, v, d| {
        c.window_save_state = match v.to_ascii_lowercase().as_str() {
            "" => d.window_save_state,
            "default" => WindowSaveState::Default,
            "never" => WindowSaveState::Never,
            "always" => WindowSaveState::Always,
            _ => c.window_save_state,
        }
    }),
    ("resize-overlay", |c, v, d| {
        c.resize_overlay = match v.to_ascii_lowercase().as_str() {
            "" => d.resize_overlay,
            "always" => ResizeOverlay::Always,
            "never" => ResizeOverlay::Never,
            "after-first" => ResizeOverlay::AfterFirst,
            _ => c.resize_overlay,
        }
    }),
    ("resize-overlay-position", |c, v, d| {
        c.resize_overlay_position = match v.to_ascii_lowercase().as_str() {
            "" => d.resize_overlay_position,
            "center" => ResizeOverlayPosition::Center,
            "top-left" => ResizeOverlayPosition::TopLeft,
            "top-center" => ResizeOverlayPosition::TopCenter,
            "top-right" => ResizeOverlayPosition::TopRight,
            "bottom-left" => ResizeOverlayPosition::BottomLeft,
            "bottom-center" => ResizeOverlayPosition::BottomCenter,
            "bottom-right" => ResizeOverlayPosition::BottomRight,
            _ => c.resize_overlay_position,
        }
    }),
    ("resize-overlay-duration", |c, v, d| {
        c.resize_overlay_duration_ms = if v.is_empty() {
            d.resize_overlay_duration_ms
        } else {
            // Clamped: below ~250 ms the overlay is gone before it registers, and
            // an unbounded value would pin it on screen indefinitely.
            parse_duration_ms(v)
                .map(|ms| ms.clamp(250, 60_000))
                .unwrap_or(c.resize_overlay_duration_ms)
        }
    }),
    ("clipboard-read", |c, v, d| {
        c.clipboard.read = parse_clipboard_access(v, d.clipboard.read, c.clipboard.read);
    }),
    ("clipboard-write", |c, v, d| {
        c.clipboard.write = parse_clipboard_access(v, d.clipboard.write, c.clipboard.write);
    }),
    ("clipboard-trim-trailing-spaces", |c, v, d| {
        c.clipboard.trim_trailing_spaces = parse_bool(v, d.clipboard.trim_trailing_spaces);
    }),
    ("clipboard-paste-protection", |c, v, d| {
        c.clipboard.paste_protection = parse_bool(v, d.clipboard.paste_protection);
    }),
    ("clipboard-paste-bracketed-safe", |c, v, d| {
        c.clipboard.paste_bracketed_safe = parse_bool(v, d.clipboard.paste_bracketed_safe);
    }),
    ("scrollbar", |c, v, d| {
        c.scrollbar = match v.to_ascii_lowercase().as_str() {
            "" => d.scrollbar,
            "system" => Scrollbar::System,
            "never" => Scrollbar::Never,
            _ => c.scrollbar,
        }
    }),
    ("osc-color-report-format", |c, v, d| {
        c.osc_color_report_format = match v.to_ascii_lowercase().as_str() {
            "" => d.osc_color_report_format,
            "none" => OscColorReportFormat::None,
            "8-bit" => OscColorReportFormat::Bits8,
            "16-bit" => OscColorReportFormat::Bits16,
            _ => c.osc_color_report_format,
        }
    }),
    ("desktop-notifications", |c, v, d| {
        c.desktop_notifications = if v.is_empty() {
            d.desktop_notifications
        } else {
            parse_bool(v, c.desktop_notifications)
        }
    }),
    // Ghostty enforces a 10x4 floor on a *set* size; zero stays zero, meaning
    // "unset". A window narrower than that is unusable rather than merely small.
    ("window-width", |c, v, d| {
        c.window_width = cells(v, d.window_width, c.window_width, 10)
    }),
    ("window-height", |c, v, d| {
        c.window_height = cells(v, d.window_height, c.window_height, 4)
    }),
    ("window-position-x", |c, v, d| {
        c.window_position_x = coord(v, d.window_position_x, c.window_position_x)
    }),
    ("window-position-y", |c, v, d| {
        c.window_position_y = coord(v, d.window_position_y, c.window_position_y)
    }),
    ("mouse-hide-while-typing", |c, v, d| {
        c.mouse_hide_while_typing = if v.is_empty() {
            d.mouse_hide_while_typing
        } else {
            parse_bool(v, c.mouse_hide_while_typing)
        }
    }),
    ("mouse-reporting", |c, v, d| {
        c.mouse_reporting = if v.is_empty() {
            d.mouse_reporting
        } else {
            parse_bool(v, c.mouse_reporting)
        }
    }),
    ("mouse-scroll-multiplier", |c, v, d| {
        c.mouse_scroll_multiplier = if v.is_empty() {
            d.mouse_scroll_multiplier
        } else {
            parse_scroll_multiplier(v, c.mouse_scroll_multiplier)
                .unwrap_or(c.mouse_scroll_multiplier)
        }
    }),
    ("scroll-to-bottom", |c, v, d| {
        c.scroll_to_bottom = if v.is_empty() {
            d.scroll_to_bottom
        } else {
            parse_scroll_to_bottom(v).unwrap_or(c.scroll_to_bottom)
        }
    }),
    ("focus-follows-mouse", |c, v, d| {
        c.focus_follows_mouse = if v.is_empty() {
            d.focus_follows_mouse
        } else {
            parse_bool(v, c.focus_follows_mouse)
        }
    }),
    ("custom-shader", |c, v, d| {
        // Repeatable, like `palette` and `font-feature`: each line appends, and
        // an empty value resets the whole list.
        if v.is_empty() {
            c.custom_shaders = d.custom_shaders.clone();
        } else {
            c.custom_shaders.push(v.to_string());
        }
    }),
    ("custom-shader-animation", |c, v, d| {
        c.custom_shader_animation = match v.to_ascii_lowercase().as_str() {
            "" => d.custom_shader_animation,
            "true" | "1" | "on" | "yes" => CustomShaderAnimation::True,
            "false" | "0" | "off" | "no" => CustomShaderAnimation::False,
            "always" => CustomShaderAnimation::Always,
            _ => c.custom_shader_animation,
        }
    }),
    ("progress-style", |c, v, d| {
        c.progress_style = if v.is_empty() {
            d.progress_style
        } else {
            parse_bool(v, c.progress_style)
        }
    }),
    ("notify-on-command-finish", |c, v, d| {
        c.notify_on_command_finish = match v.to_ascii_lowercase().as_str() {
            "" => d.notify_on_command_finish,
            "never" => NotifyOnCommandFinish::Never,
            "unfocused" => NotifyOnCommandFinish::Unfocused,
            "always" => NotifyOnCommandFinish::Always,
            _ => c.notify_on_command_finish,
        }
    }),
    ("notify-on-command-finish-action", |c, v, d| {
        c.notify_on_command_finish_action = if v.is_empty() {
            d.notify_on_command_finish_action
        } else {
            parse_notify_action(v).unwrap_or(c.notify_on_command_finish_action)
        }
    }),
    ("notify-on-command-finish-after", |c, v, d| {
        c.notify_on_command_finish_after_ms = if v.is_empty() {
            d.notify_on_command_finish_after_ms
        } else {
            // Unclamped at the top (a user may legitimately only want to hear
            // about hour-long jobs), but floored at zero-is-zero: Ghostty
            // accepts `0` and means "every command".
            parse_duration_ms(v).unwrap_or(c.notify_on_command_finish_after_ms)
        }
    }),
    ("undo-timeout", |c, v, d| {
        c.undo_timeout_ms = if v.is_empty() {
            d.undo_timeout_ms
        } else {
            // Unclamped, deliberately: upstream documents both ends — `0` turns
            // undo off, and "a very large timeout" is the sanctioned way to keep
            // operations around indefinitely (with its own warning attached).
            parse_duration_ms(v).unwrap_or(c.undo_timeout_ms)
        }
    }),
    ("bell-features", |c, v, d| {
        c.bell = if v.is_empty() {
            d.bell
        } else {
            parse_bell_features(v).unwrap_or_else(|| {
                diag!("giest: ignoring invalid bell-features '{v}'");
                c.bell
            })
        }
    }),
    ("app-notifications", |c, v, d| {
        c.app_notifications = if v.is_empty() {
            d.app_notifications
        } else {
            parse_app_notifications(v).unwrap_or_else(|| {
                diag!("giest: ignoring invalid app-notifications '{v}'");
                c.app_notifications
            })
        }
    }),
    ("bell-audio-path", |c, v, d| {
        c.bell_audio_path = opt_string(v, &d.bell_audio_path)
    }),
    ("bell-audio-volume", |c, v, d| {
        c.bell_audio_volume = ratio(v, d.bell_audio_volume, c.bell_audio_volume, 0.0, 1.0)
    }),
    // The three inheritance keys are separate in Ghostty and read by one shared
    // decision table (`crate::app::should_inherit_cwd`), so they can't drift.
    ("tab-inherit-working-directory", |c, v, d| {
        c.tab_inherit_working_directory = parse_bool(v, d.tab_inherit_working_directory);
    }),
    ("split-inherit-working-directory", |c, v, d| {
        c.split_inherit_working_directory = parse_bool(v, d.split_inherit_working_directory);
    }),
    // Repeatable, like `font-family`: each line appends another file to load
    // *after* this one. An empty value clears the list collected so far from
    // this body (Ghostty's RepeatablePath), which is the only way to undo an
    // earlier line.
    ("config-file", |c, v, _d| {
        if v.is_empty() {
            c.config_file.clear();
        } else {
            c.config_file.push(v.to_string());
        }
    }),
    ("window-inherit-working-directory", |c, v, d| {
        c.window_inherit_working_directory = parse_bool(v, d.window_inherit_working_directory);
    }),
    ("palette-generate", |c, v, d| c.palette_generate = parse_bool(v, d.palette_generate)),
    ("palette-harmonious", |c, v, d| {
        c.palette_harmonious = parse_bool(v, d.palette_harmonious);
    }),
    ("quick-terminal-animation-duration", |c, v, d| {
        c.quick_terminal_animation_duration = match v {
            "" => d.quick_terminal_animation_duration,
            _ => match v.parse::<f64>() {
                Ok(x) if x.is_finite() && x >= 0.0 => x.min(10.0),
                _ => c.quick_terminal_animation_duration,
            },
        }
    }),
    ("window-decoration", |c, v, d| {
        c.window_decoration = match v.to_ascii_lowercase().as_str() {
            "" => d.window_decoration,
            "auto" | "true" => WindowDecoration::Auto,
            "client" => WindowDecoration::Client,
            "server" => WindowDecoration::Server,
            "none" | "false" => WindowDecoration::None,
            _ => c.window_decoration,
        }
    }),
    ("window-titlebar-background", |c, v, d| {
        c.window_titlebar_background = match v {
            "" => d.window_titlebar_background,
            _ => parse_color(v).or(c.window_titlebar_background),
        }
    }),
    ("window-titlebar-foreground", |c, v, d| {
        c.window_titlebar_foreground = match v {
            "" => d.window_titlebar_foreground,
            _ => parse_color(v).or(c.window_titlebar_foreground),
        }
    }),
    ("window-show-tab-bar", |c, v, d| {
        c.window_show_tab_bar = match v.to_ascii_lowercase().as_str() {
            "" => d.window_show_tab_bar,
            "always" => ShowTabBar::Always,
            "auto" => ShowTabBar::Auto,
            "never" => ShowTabBar::Never,
            _ => c.window_show_tab_bar,
        }
    }),
    ("maximize", |c, v, d| c.maximize = parse_bool(v, d.maximize)),
    ("fullscreen", |c, v, d| {
        c.fullscreen = match v.to_ascii_lowercase().as_str() {
            "" => d.fullscreen,
            "non-native" | "non-native-visible-menu" | "non-native-padded-notch" => true,
            _ => parse_bool(v, c.fullscreen),
        }
    }),
    // An empty value resets (upstream: quote spaces for a blank title), so a
    // value of only spaces is kept verbatim.
    ("title", |c, v, _d| c.title = (!v.is_empty()).then(|| v.to_string())),
    ("window-subtitle", |c, v, d| {
        c.window_subtitle = match v.to_ascii_lowercase().as_str() {
            "" => d.window_subtitle,
            "working-directory" => true,
            "false" => false,
            _ => c.window_subtitle,
        }
    }),
    ("window-title-font-family", |c, v, _d| {
        c.window_title_font_family = (!v.is_empty()).then(|| v.to_string())
    }),
    ("window-step-resize", |c, v, d| {
        c.window_step_resize = parse_bool(v, d.window_step_resize)
    }),
    ("window-vsync", |c, v, d| c.window_vsync = parse_bool(v, d.window_vsync)),
    ("quit-after-last-window-closed", |c, v, d| {
        c.quit_after_last_window_closed = parse_bool(v, d.quit_after_last_window_closed)
    }),
    ("quit-after-last-window-closed-delay", |c, v, d| {
        c.quit_after_last_window_closed_delay_ms = if v.is_empty() {
            d.quit_after_last_window_closed_delay_ms
        } else {
            parse_duration_ms(v).or(c.quit_after_last_window_closed_delay_ms)
        }
    }),
    ("initial-window", |c, v, d| c.initial_window = parse_bool(v, d.initial_window)),
    // A packed-struct flag list: `navigation` / `no-navigation`, comma-separated.
    ("split-preserve-zoom", |c, v, d| {
        if v.is_empty() {
            c.split_preserve_zoom_navigation = d.split_preserve_zoom_navigation;
        }
        for flag in v.split(',').map(str::trim) {
            match flag.to_ascii_lowercase().as_str() {
                "navigation" | "true" => c.split_preserve_zoom_navigation = true,
                "no-navigation" | "false" => c.split_preserve_zoom_navigation = false,
                _ => {}
            }
        }
    }),
    ("title-report", |c, v, d| c.title_report = parse_bool(v, d.title_report)),
    ("vt-kam-allowed", |c, v, d| c.vt_kam_allowed = parse_bool(v, d.vt_kam_allowed)),
    ("grapheme-width-method", |c, v, d| {
        c.grapheme_unicode = match v {
            "unicode" => true,
            "legacy" => false,
            _ => d.grapheme_unicode,
        };
    }),
];

/// Parse a `clipboard-read`/`-write` value: `allow` / `deny` / `ask`. An empty
/// value resets to `default`; anything unrecognized keeps `current`, matching
/// every other enum key.
fn parse_clipboard_access(
    v: &str,
    default: ClipboardAccess,
    current: ClipboardAccess,
) -> ClipboardAccess {
    match v.to_ascii_lowercase().as_str() {
        "" => default,
        "allow" => ClipboardAccess::Allow,
        "deny" => ClipboardAccess::Deny,
        "ask" => ClipboardAccess::Ask,
        _ => current,
    }
}

/// Parse a Ghostty-style boolean (`true`/`false`, `yes`/`no`, `on`/`off`, `1`/`0`),
/// returning `default` for an empty or unrecognized value.
fn parse_bool(v: &str, default: bool) -> bool {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => true,
        "false" | "no" | "off" | "0" => false,
        _ => default,
    }
}

/// Resolve the config file path: `$GIEST_CONFIG` if set, else
/// `%APPDATA%\giest\config` (Ghostty names its file `config`, no extension).
/// Public so the command palette's "Open Config" can reveal it.
pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GIEST_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("giest").join("config"))
}

/// The directory the config file lives in — what a relative `Path`-valued key
/// resolves against. `None` when there is no config path at all.
pub fn config_dir() -> Option<PathBuf> {
    config_path().and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Resolve a `Path`-valued config key (`background-image`, `bell-audio-path`)
/// against `config_dir`.
///
/// Relative paths resolve against the config file's own directory — Ghostty
/// resolves its `Path` values the same way — so a config can ship an image or a
/// sound beside itself. An empty/whitespace value means "unset", not "the
/// config directory".
pub fn resolve_path(raw: &str, config_dir: Option<&Path>) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    Some(match config_dir {
        Some(dir) => dir.join(p),
        None => p.to_path_buf(),
    })
}

/// Split one `config-file` spec into a resolved path and whether it is optional.
///
/// A leading `?` marks the file optional (missing is not an error), matching
/// Ghostty. **Divergence:** upstream lets `"?name"` quote a *literal* leading
/// `?`; giest's parser strips surrounding quotes before any key sees the value,
/// and `?` is not a legal character in a Windows filename anyway, so there is
/// nothing to escape. An empty path is ignored rather than reset — resetting is
/// what a bare empty value does, and `?` alone is a typo, not a reset.
fn parse_include(spec: &str, base: Option<&Path>) -> Option<(PathBuf, bool)> {
    let spec = spec.trim();
    let (rest, optional) = match spec.strip_prefix('?') {
        Some(rest) => (rest, true),
        None => (spec, false),
    };
    resolve_path(rest, base).map(|p| (p, optional))
}

/// The identity a loaded config file is remembered by, for cycle detection.
/// Canonicalized so `a/../b` and a symlink can't reintroduce a cycle by
/// spelling the same file differently; a path that won't canonicalize (it
/// doesn't exist) falls back to itself, which still catches the literal repeat.
fn load_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Return the last `key = value` value in a config body, or `None`. Used to find
/// `theme` before the main parse pass (last-wins, like every other key).
fn config_value(text: &str, key: &str) -> Option<String> {
    let mut found = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                found = Some(unquote(v.trim()).to_string());
            }
        }
    }
    found
}

/// Choose the theme name from a `theme` spec. A bare name (or path) is returned
/// as-is; Ghostty's `light:Foo,dark:Bar` dual form resolves to the dark variant
/// (giest renders dark by default; wiring system light/dark switching is a
/// follow-up).
fn select_theme_variant(spec: &str) -> String {
    if spec.contains("light:") || spec.contains("dark:") {
        let (mut light, mut dark) = (None, None);
        for part in spec.split(',') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix("dark:") {
                dark = Some(rest.trim().to_string());
            } else if let Some(rest) = part.strip_prefix("light:") {
                light = Some(rest.trim().to_string());
            }
        }
        return dark.or(light).unwrap_or_default();
    }
    spec.to_string()
}

/// Expand a leading `~/` or `~\` (or a bare `~`) to `%USERPROFILE%`, as
/// upstream expands `~` in theme paths. Anything else is returned verbatim.
fn expand_home(p: &str) -> PathBuf {
    if p == "~" {
        if let Some(h) = home_dir() {
            return h;
        }
    }
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix(r"~\"))
        && let Some(h) = home_dir()
    {
        return h.join(rest);
    }
    PathBuf::from(p)
}

/// Resolve a theme name to a file (after `~` expansion):an explicit existing path wins, otherwise
/// `<config-dir>/themes/<name>` (Ghostty's `themes/` convention). `None` if no
/// such file exists.
fn resolve_theme_path(name: &str) -> Option<PathBuf> {
    let direct = expand_home(name);
    if direct.is_file() {
        return Some(direct);
    }
    let dir = config_path()?.parent()?.to_path_buf();
    let p = dir.join("themes").join(name);
    p.is_file().then_some(p)
}

/// Strip a single pair of matching surrounding quotes, if present. Ghostty
/// values are unquoted, but tolerating quotes eases pasting TOML-style values.
fn unquote(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Resolve an optional string field: an empty value resets to `default`,
/// anything else is taken verbatim. Used for the `font-family*` keys.
fn opt_string(value: &str, default: &Option<String>) -> Option<String> {
    if value.is_empty() {
        default.clone()
    } else {
        Some(value.to_string())
    }
}

/// Resolve a required color field: empty resets to `default`, a valid hex sets
/// it, anything else keeps `current`.
fn color(value: &str, default: Rgb, current: Rgb) -> Rgb {
    if value.is_empty() {
        default
    } else {
        parse_color(value).unwrap_or(current)
    }
}

/// Resolve an optional color field (cursor / selection foreground): empty resets
/// to `default`, a valid hex sets `Some`, anything else keeps `current`.
fn opt_color(value: &str, default: Option<Rgb>, current: Option<Rgb>) -> Option<Rgb> {
    if value.is_empty() {
        default
    } else {
        parse_color(value).map(Some).unwrap_or(current)
    }
}

/// Resolve a padding field: empty resets to `default`, a valid non-negative
/// number sets it (clamped at 0), anything else keeps `current`.
/// Resolve a cell-count field (`window-width`/`-height`): empty resets, `0`
/// means "unset", and any other value is floored at `min` — Ghostty's rule, and
/// a window below it is unusable rather than merely small.
fn cells(value: &str, default: u32, current: u32, min: u32) -> u32 {
    if value.is_empty() {
        return default;
    }
    match value.parse::<u32>() {
        Ok(0) => 0,
        Ok(n) => n.max(min),
        Err(_) => current,
    }
}

/// Resolve an optional pixel coordinate (`window-position-x`/`-y`): empty
/// resets, a valid integer sets it, anything else keeps the current value.
/// Negative is legal — a second monitor left of the primary has negative x.
fn coord(value: &str, default: Option<i16>, current: Option<i16>) -> Option<i16> {
    if value.is_empty() {
        return default;
    }
    value.parse::<i16>().ok().map(Some).unwrap_or(current)
}

fn padding(value: &str, default: f32, current: f32) -> f32 {
    if value.is_empty() {
        default
    } else {
        value.parse::<f32>().map(|p| p.max(0.0)).unwrap_or(current)
    }
}

/// Resolve a clamped ratio field (the various opacities, contrast, gamma): empty
/// resets to `default`, a valid number is clamped into `min..=max`, anything else
/// keeps `current`. Ghostty clamps out-of-range ratios rather than rejecting
/// them, so `background-opacity = 2` means fully opaque, not a parse error.
fn ratio(value: &str, default: f32, current: f32, min: f32, max: f32) -> f32 {
    if value.is_empty() {
        default
    } else {
        value.parse::<f32>().map(|n| n.clamp(min, max)).unwrap_or(current)
    }
}

/// The body of all four `font-style*` keys, so the slots cannot drift.
fn set_font_style(c: &mut Config, d: &Config, v: &str, slot: usize) {
    c.font_styles[slot] = if v.is_empty() {
        d.font_styles[slot].clone()
    } else {
        FontStyle::parse(v)
    };
}

/// The body of all four `font-variation*` keys, so the slots cannot drift.
///
/// Repeatable like `font-feature` and `palette`: each line **appends** one axis,
/// and an empty value resets that slot to the default (empty). A malformed line
/// is dropped and reported — silence would be indistinguishable from a font
/// that doesn't have the axis, which is the failure this feature is most likely
/// to be blamed for.
fn set_font_variation(c: &mut Config, d: &Config, v: &str, slot: usize) {
    if v.is_empty() {
        c.font_variations[slot] = d.font_variations[slot].clone();
        return;
    }
    match parse_font_variation(v) {
        Some(var) => c.font_variations[slot].push(var),
        None => diag!(
            "giest: font-variation: expected a 4-character axis and a number, e.g. `wght=200`; got {v:?}"
        ),
    }
}

/// Ghostty `font-style` and its three per-style siblings.
///
/// Not a bool and not a string: the three cases behave differently enough that
/// collapsing any two of them loses a real option. `Disabled` in particular is
/// the only one that does something *without* a `font-family` — upstream:
/// "these are only valid if its corresponding font-family is also specified …
/// unless you're disabling the font style".
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum FontStyle {
    /// Pick the style the usual way: by the face's bold/italic flags.
    #[default]
    Default,
    /// Turn this style off. A program asking for it gets the **regular** face,
    /// and nothing is synthesized — that is the difference from
    /// `font-synthetic-style`, which controls how a *missing* style is faked.
    Disabled,
    /// Match the font's own advertised style name, e.g. `Heavy` for
    /// "Iosevka Heavy". This is how you reach a weight that isn't "bold".
    Named(String),
}

impl FontStyle {
    fn parse(v: &str) -> Self {
        match v.trim() {
            "default" => Self::Default,
            "false" => Self::Disabled,
            // Everything else is a style name — including "true", which is not a
            // value upstream gives meaning to, and a name a font could plausibly
            // advertise.
            name => Self::Named(name.to_string()),
        }
    }
}

/// One variable-font axis setting. Ghostty `font-variation` and its three
/// per-style siblings.
///
/// A *variable* font packs several designs into one file along named axes —
/// `wght` (weight), `wdth` (width), `slnt` (slant), `opsz` (optical size) — and
/// a variation picks a point on them. The tag is always exactly four bytes;
/// that is the OpenType format's rule, not a parser convenience.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontVariation {
    pub tag: [u8; 4],
    pub value: f32,
}

impl FontVariation {
    /// The tag as text, for messages and round-tripping.
    pub fn tag_str(&self) -> String {
        self.tag.iter().map(|b| *b as char).collect()
    }
}

/// Parse one `id=value` variation, Ghostty's `RepeatableFontVariation.parseCLI`.
///
/// Whitespace around **both** halves is trimmed — upstream trims explicitly, so
/// `wght = 200` is valid here even though the same spacing is rejected for
/// `font-feature` (there the value is handed to rustybuzz, whose parser is
/// stricter than Ghostty's). The tag must be exactly four characters and the
/// value must parse as a float; anything else is rejected rather than guessed
/// at, because a silently-dropped axis looks exactly like a font that doesn't
/// support it.
///
/// Note the **absence** of comma splitting, which `font-feature` has: upstream
/// takes one axis per occurrence and repeats the key, and splitting here would
/// accept `wght=200, wdth=90` that a real Ghostty config rejects.
fn parse_font_variation(s: &str) -> Option<FontVariation> {
    let (id, value) = s.split_once('=')?;
    let id = id.trim();
    // Four *bytes*: an OpenType tag is four bytes, and a multi-byte character
    // would make a 4-char string that is not a 4-byte tag.
    let tag: [u8; 4] = id.as_bytes().try_into().ok()?;
    if !id.is_ascii() {
        return None;
    }
    let value: f32 = value.trim().parse().ok()?;
    value.is_finite().then_some(FontVariation { tag, value })
}

/// Parse Ghostty's `Duration` grammar into milliseconds.
///
/// A duration is a series of number+unit pairs which **add**, so `1h30m` is 90
/// minutes and even `1h1h` is 2 hours. Units: `y d w h m s ms us`/`µs` `ns`.
/// giest additionally accepts a bare integer as milliseconds — a superset that
/// can't collide, since Ghostty requires a unit on every component.
///
/// Sub-millisecond components are parsed and contribute 0 ms rather than being
/// rejected, so a valid Ghostty config doesn't warn.
/// One `input` source. Ghostty `RepeatableReadableIO` (`config/io.zig`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputSource {
    /// Bytes to send as-is, with Zig string-literal escapes already decoded.
    Raw(Vec<u8>),
    /// A file whose contents are sent; read at spawn, not at config time.
    Path(PathBuf),
}

/// Upstream caps a `path:` input at 10MB so a runaway file can't stall startup.
pub const INPUT_PATH_MAX_BYTES: u64 = 10 * 1024 * 1024;

impl InputSource {
    /// `raw:<text>`, `path:<file>`, or an untagged value (which is `raw`).
    /// Escapes are decoded for **both** forms, as upstream does (its
    /// `cloneParsed` runs the Zig string parser over either variant).
    pub fn parse(v: &str) -> Option<Self> {
        if let Some(p) = v.strip_prefix("path:") {
            let bytes = parse_zig_string(p)?;
            return Some(Self::Path(PathBuf::from(String::from_utf8(bytes).ok()?)));
        }
        let raw = v.strip_prefix("raw:").unwrap_or(v);
        parse_zig_string(raw).map(Self::Raw)
    }

    /// The bytes to write, or `None` if a path can't be read (or is over the
    /// cap).
    fn read(&self) -> Option<Vec<u8>> {
        match self {
            Self::Raw(b) => Some(b.clone()),
            Self::Path(p) => {
                let meta = std::fs::metadata(p).ok()?;
                if !meta.is_file() || meta.len() > INPUT_PATH_MAX_BYTES {
                    return None;
                }
                std::fs::read(p).ok()
            }
        }
    }
}

/// Concatenate every `input` source with no separator, or `None` (send
/// nothing at all) if any one of them fails — upstream's all-or-nothing rule.
pub fn resolve_input(sources: &[InputSource]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for s in sources {
        out.extend(s.read()?);
    }
    Some(out)
}

/// Decode Zig string-literal escapes (Ghostty `config/string.zig`): `\n \r \t
/// \\ \' \"`, `\xNN` (one raw byte), and `\u{N..}` (a codepoint, UTF-8
/// encoded). Any other escape invalidates the whole value.
pub fn parse_zig_string(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next()? {
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            '\\' => out.push(b'\\'),
            '\'' => out.push(b'\''),
            '"' => out.push(b'"'),
            'x' => {
                let hi = chars.next()?.to_digit(16)?;
                let lo = chars.next()?.to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
            }
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let mut n: u32 = 0;
                let mut digits = 0;
                loop {
                    let d = chars.next()?;
                    if d == '}' {
                        break;
                    }
                    n = n.checked_mul(16)?.checked_add(d.to_digit(16)?)?;
                    digits += 1;
                }
                if digits == 0 {
                    return None;
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(char::from_u32(n)?.encode_utf8(&mut buf).as_bytes());
            }
            _ => return None,
        }
    }
    Some(out)
}

fn parse_duration_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Bare integer → milliseconds.
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let mut total_ns: u128 = 0;
    let mut rest = s;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        if digits == 0 {
            return None; // a unit with no number
        }
        let (num, tail) = rest.split_at(digits);
        let n: u128 = num.parse().ok()?;
        let unit_len = tail
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(tail.len());
        let (unit, tail) = tail.split_at(unit_len);
        let ns_per: u128 = match unit {
            "y" => 365 * 24 * 60 * 60 * 1_000_000_000,
            "d" => 24 * 60 * 60 * 1_000_000_000,
            "w" => 7 * 24 * 60 * 60 * 1_000_000_000,
            "h" => 60 * 60 * 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "s" => 1_000_000_000,
            "ms" => 1_000_000,
            "us" | "µs" => 1_000,
            "ns" => 1,
            _ => return None,
        };
        total_ns = total_ns.saturating_add(n.saturating_mul(ns_per));
        rest = tail;
    }
    Some((total_ns / 1_000_000).min(u64::MAX as u128) as u64)
}

/// Parse a palette override entry of the form `<index>=#rrggbb` into its 0–255
/// palette index and color. Returns `None` for malformed entries or an
/// out-of-range index.
fn parse_palette_entry(s: &str) -> Option<(u8, Rgb)> {
    let (idx, color) = s.split_once('=')?;
    let idx: u8 = idx.trim().parse().ok()?;
    let color = parse_color(color.trim())?;
    Some((idx, color))
}

/// Parse a color value the way Ghostty's `Color.parseCLI` does: an X11 color
/// name (case-insensitive) first, then a hex literal. Used by every color key so
/// values stay transposable with a real Ghostty config.
fn parse_color(s: &str) -> Option<Rgb> {
    x11_colors()
        .get(s.trim().to_ascii_lowercase().as_str())
        .copied()
        .or_else(|| parse_hex(s))
}

/// Parse a hex color: `#rrggbb`/`rrggbb`, or the 3-digit short form
/// `#rgb`/`rgb` (each nibble duplicated, e.g. `#f80` → `#ff8800`), matching
/// Ghostty's `Color.fromHex`.
fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#').unwrap_or(s);
    match h.len() {
        6 => {
            let r = u8::from_str_radix(&h[0..2], 16).ok()?;
            let g = u8::from_str_radix(&h[2..4], 16).ok()?;
            let b = u8::from_str_radix(&h[4..6], 16).ok()?;
            Some(Rgb::new(r, g, b))
        }
        3 => {
            // Duplicate each nibble: 0xf → 0xff (× 17).
            let nib = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|v| v * 17);
            Some(Rgb::new(nib(0)?, nib(1)?, nib(2)?))
        }
        _ => None,
    }
}

/// The X11 color-name table (Ghostty's vendored `rgb.txt`), keyed by lowercased
/// name for case-insensitive lookup. Built once on first use. The file lists
/// each color as `R G B<TAB>name`, with both spaced and CamelCase spellings of
/// multi-word names (`ghost white` and `GhostWhite`), so case-insensitive keys
/// cover the space-collapsed form too.
fn x11_colors() -> &'static HashMap<String, Rgb> {
    static MAP: OnceLock<HashMap<String, Rgb>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = HashMap::new();
        for line in include_str!("res/rgb.txt").lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            let (Some(r), Some(g), Some(b)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            let (Ok(r), Ok(g), Ok(b)) = (r.parse::<u8>(), g.parse::<u8>(), b.parse::<u8>()) else {
                continue;
            };
            let name = it.collect::<Vec<_>>().join(" ");
            if !name.is_empty() {
                m.insert(name.to_ascii_lowercase(), Rgb::new(r, g, b));
            }
        }
        m
    })
}

/// The 16 ANSI colors (a calm dark scheme). 0–7 normal, 8–15 bright.
const GIEST_ANSI16: [Rgb; 16] = [
    Rgb::new(0x10, 0x12, 0x18), // black
    Rgb::new(0xcc, 0x66, 0x66), // red
    Rgb::new(0xb5, 0xbd, 0x68), // green
    Rgb::new(0xf0, 0xc6, 0x74), // yellow
    Rgb::new(0x81, 0xa2, 0xbe), // blue
    Rgb::new(0xb2, 0x94, 0xbb), // magenta
    Rgb::new(0x8a, 0xbe, 0xb7), // cyan
    Rgb::new(0xc5, 0xc8, 0xc6), // white
    Rgb::new(0x66, 0x6a, 0x73), // bright black
    Rgb::new(0xd5, 0x4e, 0x53), // bright red
    Rgb::new(0xb9, 0xca, 0x4a), // bright green
    Rgb::new(0xe7, 0xc5, 0x47), // bright yellow
    Rgb::new(0x7a, 0xa6, 0xda), // bright blue
    Rgb::new(0xc3, 0x97, 0xd8), // bright magenta
    Rgb::new(0x70, 0xc0, 0xb1), // bright cyan
    Rgb::new(0xea, 0xea, 0xea), // bright white
];

/// Build the standard 256-color palette from the 16 base ANSI colors.
fn xterm_palette(base16: [Rgb; 16]) -> [Rgb; 256] {
    let mut p = [Rgb::default(); 256];
    p[..16].copy_from_slice(&base16);

    // 16..=231: 6×6×6 RGB cube.
    let steps = [0u8, 95, 135, 175, 215, 255];
    let mut idx = 16;
    for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                p[idx] = Rgb::new(steps[r], steps[g], steps[b]);
                idx += 1;
            }
        }
    }

    // 232..=255: 24-step grayscale ramp.
    for i in 0..24 {
        let v = (8 + i * 10) as u8;
        p[232 + i] = Rgb::new(v, v, v);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a Ghostty-format config body over the defaults.
    fn parsed(body: &str) -> Config {
        Config::from_ghostty_config(body)
    }

    #[test]
    fn window_chrome_keys_parse() {
        let d = Config::default();
        assert_eq!(d.window_decoration, WindowDecoration::Auto);
        assert!(d.quit_after_last_window_closed && d.initial_window && d.window_vsync);
        assert_eq!(parsed("window-decoration = none").window_decoration, WindowDecoration::None);
        assert_eq!(parsed("window-decoration = false").window_decoration, WindowDecoration::None);
        assert_eq!(parsed("window-decoration = true").window_decoration, WindowDecoration::Auto);
        assert_eq!(parsed("window-decoration = server").window_decoration, WindowDecoration::Server);
        assert!(!WindowDecoration::None.decorated() && WindowDecoration::Client.decorated());
        assert_eq!(
            parsed("window-titlebar-background = #102030").window_titlebar_background,
            Some(Rgb::new(0x10, 0x20, 0x30))
        );
        assert_eq!(
            parsed("window-titlebar-foreground = #ffffff\nwindow-titlebar-foreground =")
                .window_titlebar_foreground,
            None
        );
        assert_eq!(parsed("window-show-tab-bar = auto").window_show_tab_bar, ShowTabBar::Auto);
        assert!(!ShowTabBar::Auto.visible(1) && ShowTabBar::Auto.visible(2));
        assert!(!ShowTabBar::Never.visible(5) && ShowTabBar::Always.visible(1));
        assert!(parsed("maximize = true").maximize);
        assert!(parsed("fullscreen = non-native").fullscreen);
        assert!(parsed("fullscreen = true").fullscreen);
        assert!(!parsed("fullscreen = true\nfullscreen = false").fullscreen);
        assert_eq!(parsed("title = \"  \"").title.as_deref(), Some("  "));
        assert_eq!(parsed("title = work\ntitle =").title, None);
        assert!(parsed("window-subtitle = working-directory").window_subtitle);
        assert!(!parsed("window-subtitle = false").window_subtitle);
        assert_eq!(
            parsed("window-title-font-family = Segoe UI").window_title_font_family.as_deref(),
            Some("Segoe UI")
        );
        assert!(parsed("window-step-resize = true").window_step_resize);
        assert!(!parsed("window-vsync = false").window_vsync);
        assert!(!parsed("quit-after-last-window-closed = false").quit_after_last_window_closed);
        assert_eq!(
            parsed("quit-after-last-window-closed-delay = 5s")
                .quit_after_last_window_closed_delay_ms,
            Some(5000)
        );
        assert!(!parsed("initial-window = false").initial_window);
        assert!(parsed("split-preserve-zoom = navigation").split_preserve_zoom_navigation);
        assert!(
            !parsed("split-preserve-zoom = navigation\nsplit-preserve-zoom = no-navigation")
                .split_preserve_zoom_navigation
        );
        assert_eq!(
            parsed("quick-terminal-animation-duration = 0").quick_terminal_animation_duration,
            0.0
        );
        assert_eq!(
            parsed("quick-terminal-animation-duration = -1").quick_terminal_animation_duration,
            0.2
        );
    }

    #[test]
    fn palette_is_well_formed() {
        let c = Config::default();
        // ANSI red at index 1, cube corner white at 231, grayscale endpoints.
        assert_eq!(c.palette[1], GIEST_ANSI16[1]);
        assert_eq!(c.palette[231], Rgb::new(255, 255, 255));
        assert_eq!(c.palette[16], Rgb::new(0, 0, 0));
        assert_eq!(c.palette[232], Rgb::new(8, 8, 8));
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(parse_hex("#0a141e"), Some(Rgb::new(10, 20, 30)));
        assert_eq!(parse_hex("0a141e"), Some(Rgb::new(10, 20, 30)));
        // 3-digit short form: each nibble is duplicated (Ghostty's fromHex).
        assert_eq!(parse_hex("#f80"), Some(Rgb::new(0xff, 0x88, 0x00)));
        assert_eq!(parse_hex("fff"), Some(Rgb::new(255, 255, 255)));
        assert_eq!(parse_hex("#345"), Some(Rgb::new(0x33, 0x44, 0x55)));
        assert_eq!(parse_hex("nothex"), None);
        assert_eq!(parse_hex("#gg0"), None);
    }

    #[test]
    fn parses_x11_named_colors() {
        // Ghostty resolves color values through an X11 name table before hex.
        assert_eq!(parse_color("black"), Some(Rgb::new(0, 0, 0)));
        assert_eq!(parse_color("red"), Some(Rgb::new(255, 0, 0)));
        assert_eq!(parse_color("white"), Some(Rgb::new(255, 255, 255)));
        // Case-insensitive; the CamelCase spelling collapses spaces.
        assert_eq!(parse_color("ForestGreen"), Some(Rgb::new(34, 139, 34)));
        assert_eq!(parse_color("forestgreen"), Some(Rgb::new(34, 139, 34)));
        assert_eq!(parse_color("medium spring green"), Some(Rgb::new(0, 250, 154)));
        // Hex still works through the same entry point (incl. short form).
        assert_eq!(parse_color("#ff8800"), Some(Rgb::new(0xff, 0x88, 0x00)));
        assert_eq!(parse_color("#f80"), Some(Rgb::new(0xff, 0x88, 0x00)));
        // Unknown names fall through to None.
        assert_eq!(parse_color("nosuchcolor"), None);
    }

    #[test]
    fn color_keys_accept_named_colors() {
        // The named-color path backs every color key, like Ghostty.
        assert_eq!(parsed("foreground = red").fg, Rgb::new(255, 0, 0));
        assert_eq!(parsed("background = black").bg, Rgb::new(0, 0, 0));
        assert_eq!(parsed("cursor-color = blue").cursor, Some(Rgb::new(0, 0, 255)));
        assert_eq!(parsed("cursor-text = cell-foreground").cursor_text, Some(TerminalColor::CellForeground));
        assert_eq!(parsed("cursor-text = #00ff00").cursor_text, Some(TerminalColor::Color(Rgb::new(0, 255, 0))));
        assert_eq!(parsed("").cursor_text, None);
        assert_eq!(
            parsed("bold-color = black").bold_color,
            BoldColor::Color(Rgb::new(0, 0, 0))
        );
        assert_eq!(parsed("palette = 1=red").palette[1], Rgb::new(255, 0, 0));
    }

    #[test]
    fn ghostty_keys_override_defaults() {
        // Unquoted, kebab-case, Ghostty-style colors.
        let c = parsed("font-size = 18\nforeground = #ff8800\nbackground = #101010");
        assert_eq!(c.font_points, 18.0);
        assert_eq!(c.fg, Rgb::new(0xff, 0x88, 0x00));
        assert_eq!(c.bg, Rgb::new(0x10, 0x10, 0x10));
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let c = parsed("# a comment\n\n   # indented comment\nfont-size = 20\n");
        assert_eq!(c.font_points, 20.0);
    }

    #[test]
    fn parses_palette_entries() {
        assert_eq!(
            parse_palette_entry("0=#1d1f21"),
            Some((0, Rgb::new(0x1d, 0x1f, 0x21)))
        );
        assert_eq!(
            parse_palette_entry(" 15 = #eaeaea "),
            Some((15, Rgb::new(0xea, 0xea, 0xea)))
        );
        assert_eq!(
            parse_palette_entry("255=#000000"),
            Some((255, Rgb::new(0, 0, 0)))
        );
        assert_eq!(
            parse_palette_entry("256=#000000"),
            None,
            "index out of range"
        );
        assert_eq!(parse_palette_entry("nope"), None);
    }

    #[test]
    fn repeated_palette_keys_accumulate() {
        // Ghostty repeats `palette = N=#hex`, one entry per line.
        let c = parsed("cursor-color = #ff0000\npalette = 1=#abcdef\npalette = 232=#0a0a0a");
        assert_eq!(c.cursor, Some(Rgb::new(0xff, 0, 0)));
        assert_eq!(c.palette[1], Rgb::new(0xab, 0xcd, 0xef));
        assert_eq!(c.palette[232], Rgb::new(0x0a, 0x0a, 0x0a));
        // Untouched indices keep their default.
        assert_eq!(c.palette[2], GIEST_ANSI16[2]);
    }

    #[test]
    fn window_padding_keys_apply() {
        let c = parsed("window-padding-x = 12\nwindow-padding-y = 6");
        assert_eq!(c.padding_x, 12.0);
        assert_eq!(c.padding_y, 6.0);
    }

    #[test]
    fn scrollback_limit_overrides() {
        let c = Config::default();
        assert_eq!(c.scrollback_limit, 10_000);
        assert_eq!(parsed("scrollback-limit = 50000").scrollback_limit, 50_000);
        assert_eq!(parsed("scrollback-limit-bytes = 70000").scrollback_limit, 70_000);
        assert_eq!(parsed("scrollback-limit-bytes = unlimited").scrollback_limit, usize::MAX);
        assert_eq!(parsed("scrollback-limit-lines = 5000").scrollback_limit_lines, Some(5000));
        assert_eq!(parsed("scrollback-limit-lines = unlimited").scrollback_limit_lines, None);
    }

    #[test]
    fn palette_generate_keeps_base_and_explicit_entries() {
        let off = parsed("palette = 100=#123456");
        assert_eq!(off.effective_palette(), off.palette, "off by default: palette as configured");

        let c = parsed("palette-generate = true\npalette = 100=#123456\npalette = 1=#ff0000");
        let p = c.effective_palette();
        assert_eq!(p[1], Rgb { r: 0xff, g: 0, b: 0 }, "base 16 preserved");
        assert_eq!(p[100], Rgb { r: 0x12, g: 0x34, b: 0x56 }, "explicit entry kept");
        assert_ne!(p[101], c.palette[101], "cube regenerated");
        // Ramp runs background -> foreground.
        assert_ne!(p[232], c.palette[232]);
    }

    #[test]
    fn link_and_clipboard_limit_keys() {
        let c = parsed("");
        assert!(c.link_osc8 && c.link_url);
        assert_eq!(c.clipboard.write_limit, Some(64 << 20));
        let c = parsed("link-osc8 = false\nlink-url = false\nclipboard-write-limit-bytes = unlimited");
        assert!(!c.link_osc8 && !c.link_url);
        assert_eq!(c.clipboard.write_limit, None);
        assert_eq!(parsed("clipboard-write-limit-bytes = 0").clipboard.write_limit, Some(0));
    }

    #[test]
    fn conpty_passthrough_parses_auto_true_false() {
        assert_eq!(Config::default().conpty_passthrough, ConptyPassthrough::Auto);
        assert_eq!(parsed("conpty-passthrough = true").conpty_passthrough, ConptyPassthrough::On);
        assert_eq!(parsed("conpty-passthrough = false").conpty_passthrough, ConptyPassthrough::Off);
        assert_eq!(
            parsed("conpty-passthrough = false\nconpty-passthrough = bogus").conpty_passthrough,
            ConptyPassthrough::Auto
        );
    }

    #[test]
    fn image_storage_limit_defaults_to_ghosttys_value() {
        // Ghostty's default is 320 MB *decimal*, not 320 MiB.
        assert_eq!(Config::default().image_storage_limit, 320_000_000);
        assert_eq!(
            parsed("image-storage-limit = 64000000").image_storage_limit,
            64_000_000
        );
        // Zero is meaningful: it disables the image protocols entirely.
        assert_eq!(parsed("image-storage-limit = 0").image_storage_limit, 0);
        // Empty resets; garbage keeps the current value.
        assert_eq!(
            parsed("image-storage-limit = 0\nimage-storage-limit =").image_storage_limit,
            320_000_000
        );
        assert_eq!(
            parsed("image-storage-limit = 0\nimage-storage-limit = lots").image_storage_limit,
            0
        );
    }

    #[test]
    fn selection_and_copy_on_select_overrides() {
        let c = parsed(
            "selection-background = #385a9c\nselection-foreground = #ffffff\ncopy-on-select = true",
        );
        assert_eq!(c.selection_bg, Rgb::new(0x38, 0x5a, 0x9c));
        assert_eq!(c.selection_fg, Some(Rgb::new(0xff, 0xff, 0xff)));
        assert_eq!(c.copy_on_select, CopyOnSelect::Clipboard);
    }

    #[test]
    fn inherit_working_directory_keys_default_true_and_override() {
        let d = Config::default();
        assert!(d.tab_inherit_working_directory);
        assert!(d.split_inherit_working_directory);
        assert!(d.window_inherit_working_directory);

        assert!(!parsed("tab-inherit-working-directory = false").tab_inherit_working_directory);
        assert!(!parsed("split-inherit-working-directory = false").split_inherit_working_directory);
        assert!(
            !parsed("window-inherit-working-directory = false").window_inherit_working_directory
        );

        // An empty value resets to the default (true).
        assert!(parsed("tab-inherit-working-directory =").tab_inherit_working_directory);
        assert!(parsed("split-inherit-working-directory =").split_inherit_working_directory);

        // Each key is independent — turning one off must not disturb the others.
        let c = parsed("split-inherit-working-directory = false");
        assert!(c.tab_inherit_working_directory && c.window_inherit_working_directory);
    }

    /// A scratch directory that removes itself, for the `config-file` tests.
    /// (`std::env::temp_dir` + a counter; giest has no temp-dir dependency.)
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "giest-cfg-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, body: &str) -> PathBuf {
            let p = self.0.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, body).unwrap();
            p
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn config_file_include_is_loaded_after_the_file_that_names_it() {
        // Ghostty's documented and subtle rule: the include wins over the whole
        // including file, not just over the lines above the `config-file` line.
        let s = Scratch::new();
        s.write("inc", "font-size = 20");
        let root = s.write("config", "config-file = inc\nfont-size = 11");
        assert_eq!(Config::load_from_file(&root).font_points, 20.0);
    }

    #[test]
    fn config_file_includes_load_breadth_first_in_order() {
        // `a` (which includes `deep`) then `b`. `deep` joins the *end* of the
        // one shared queue when `a` is parsed, so the order is a, b, deep — and
        // `deep` gets the last word. Depth-first would run a, deep, b and leave
        // `b` winning, which is the divergence this pins.
        let s = Scratch::new();
        s.write("a", "config-file = deep\nfont-size = 1");
        s.write("b", "font-size = 2");
        s.write("deep", "font-size = 3");
        let root = s.write("config", "config-file = a\nconfig-file = b");
        assert_eq!(Config::load_from_file(&root).font_points, 3.0);
    }

    #[test]
    fn config_file_paths_resolve_against_the_including_file() {
        // `sub/child` names `sibling`, which lives in `sub/`, not beside `config`.
        let s = Scratch::new();
        s.write("sub/child", "config-file = sibling");
        s.write("sub/sibling", "font-size = 17");
        let root = s.write("config", "config-file = sub/child");
        assert_eq!(Config::load_from_file(&root).font_points, 17.0);
    }

    #[test]
    fn config_file_optional_prefix_tolerates_a_missing_file() {
        let s = Scratch::new();
        let root = s.write("config", "config-file = ?nope\nfont-size = 13");
        assert_eq!(Config::load_from_file(&root).font_points, 13.0);

        // A *required* missing file is reported but must not lose the rest.
        let root = s.write("config2", "config-file = nope\nfont-size = 14");
        assert_eq!(Config::load_from_file(&root).font_points, 14.0);
    }

    #[test]
    fn config_file_cycles_terminate() {
        // a → b → a, plus a self-include: both must stop rather than hang, and
        // the keys that did load must survive.
        let s = Scratch::new();
        s.write("a", "config-file = b\nfont-size = 21");
        s.write("b", "config-file = a\nconfig-file = b");
        let root = s.write("config", "config-file = a\nconfig-file = config");
        assert_eq!(Config::load_from_file(&root).font_points, 21.0);
    }

    #[test]
    fn config_file_empty_value_clears_the_pending_includes() {
        let s = Scratch::new();
        s.write("inc", "font-size = 30");
        let root = s.write("config", "config-file = inc\nconfig-file =\nfont-size = 12");
        assert_eq!(Config::load_from_file(&root).font_points, 12.0);
    }

    #[test]
    fn config_file_staging_list_is_empty_in_a_loaded_config() {
        // It is a parse-time staging area, not a setting — a caller reading the
        // loaded config must never see leftovers.
        let s = Scratch::new();
        s.write("inc", "font-size = 20");
        let root = s.write("config", "config-file = inc");
        assert!(Config::load_from_file(&root).config_file.is_empty());
    }

    #[test]
    fn copy_on_select_accepts_ghostty_enum() {
        let cos = |v: &str| parsed(&format!("copy-on-select = {v}")).copy_on_select;
        assert_eq!(Config::default().copy_on_select, CopyOnSelect::None);
        assert_eq!(cos("clipboard"), CopyOnSelect::Clipboard);
        assert_eq!(cos("true"), CopyOnSelect::Clipboard);
        assert_eq!(cos("primary"), CopyOnSelect::Primary);
        assert_eq!(cos("both"), CopyOnSelect::Both);
        assert_eq!(cos("none"), CopyOnSelect::None);
        assert_eq!(cos("false"), CopyOnSelect::None);
        assert!(CopyOnSelect::Both.primary() && CopyOnSelect::Both.clipboard());
        assert!(!CopyOnSelect::Primary.clipboard());
    }

    #[test]
    fn key_remap_is_repeatable_and_resettable() {
        let c = parsed("key-remap = ctrl=alt\nkey-remap = bogus\nkey-remap = left_shift=super");
        assert_eq!(c.key_remap.len(), 2);
        assert!(parsed("key-remap = ctrl=alt\nkey-remap =").key_remap.is_empty());
    }

    #[test]
    fn mouse_shift_capture_and_click_interval() {
        use MouseShiftCapture as M;
        assert_eq!(Config::default().mouse_shift_capture, M::False);
        assert_eq!(parsed("mouse-shift-capture = always").mouse_shift_capture, M::Always);
        assert_eq!(parsed("mouse-shift-capture = never").mouse_shift_capture, M::Never);
        assert_eq!(parsed("mouse-shift-capture = true").mouse_shift_capture, M::True);
        // Only true/false defer to the program's XTSHIFTESCAPE.
        assert!(M::False.captured(Some(true)) && !M::False.captured(None));
        assert!(!M::True.captured(Some(false)) && M::True.captured(None));
        assert!(M::Always.captured(Some(false)) && !M::Never.captured(Some(true)));
        assert_eq!(Config::default().click_repeat_interval, 0);
        assert_eq!(parsed("click-repeat-interval = 250").click_repeat_interval, 250);
        assert_eq!(parsed("click-repeat-interval = x").click_repeat_interval, 0);
    }

    #[test]
    fn command_palette_entry_grammar() {
        let e = parse_command_palette_entry("title:Reset Font Style, action:csi:0m").unwrap();
        assert_eq!(e.title, "Reset Font Style");
        assert_eq!(e.action, "csi:0m");
        assert_eq!(e.description, None);
        let e = parse_command_palette_entry(
            r#"title:Focus Split: Right,description:"Focus the split to the right, if it exists.",action:goto_split:right"#,
        )
        .unwrap();
        assert_eq!(e.title, "Focus Split: Right");
        assert_eq!(e.description.as_deref(), Some("Focus the split to the right, if it exists."));
        assert_eq!(e.action, "goto_split:right");
        let e = parse_command_palette_entry(r#"title:"Ghostty",action:"text:\xf0""#).unwrap();
        assert_eq!(e.title, "Ghostty");
        assert_eq!(e.action, "text:\u{f0}");
        // Required fields and unknown fields.
        assert!(parse_command_palette_entry("title:x").is_none());
        assert!(parse_command_palette_entry("action:new_tab").is_none());
        assert!(parse_command_palette_entry("title:x,action:new_tab,bogus:1").is_none());

        let c = parsed("command-palette-entry = title:A,action:new_tab");
        assert!(c.palette_defaults);
        assert_eq!(c.palette_entries.len(), 1);
        let c = parsed("command-palette-entry = title:A,action:new_tab\ncommand-palette-entry = clear\ncommand-palette-entry = title:B,action:new_tab");
        assert!(!c.palette_defaults);
        assert_eq!(c.palette_entries[0].title, "B");
        let c = parsed("command-palette-entry = clear\ncommand-palette-entry =");
        assert!(c.palette_defaults && c.palette_entries.is_empty());
    }

    #[test]
    fn clipboard_codepoint_map_replaces_on_copy() {
        let c = parsed(
            "clipboard-codepoint-map = U+2500=U+002D\nclipboard-codepoint-map = U+03A3=SUM\nclipboard-codepoint-map = U+2500-U+2502=|",
        );
        assert_eq!(c.clipboard_codepoint_map.len(), 3);
        // The later range wins for U+2500.
        assert_eq!(map_clipboard_text(&c.clipboard_codepoint_map, "a\u{2500}\u{3a3}\u{2501}b"), "a|SUM|b");
        assert_eq!(map_clipboard_text(&[], "x\u{2500}"), "x\u{2500}");
        assert!(parse_clipboard_map("2500=x").is_none());
        assert!(parse_clipboard_map("U+2502-U+2500=x").is_none());
    }

    #[test]
    fn font_codepoint_map_and_shaping_and_thicken() {
        let c = parsed("font-codepoint-map = U+E000-U+E0FF,U+F000=Symbols Nerd Font");
        assert_eq!(c.font_codepoint_map[0].ranges, vec![(0xE000, 0xE0FF), (0xF000, 0xF000)]);
        assert_eq!(c.font_codepoint_map[0].family, "Symbols Nerd Font");
        assert!(parsed("font-codepoint-map = U+E000=").font_codepoint_map.is_empty());
        assert!(Config::default().font_shaping_break_cursor);
        assert!(!parsed("font-shaping-break = no-cursor").font_shaping_break_cursor);
        assert!(parsed("font-shaping-break = no-cursor,cursor").font_shaping_break_cursor);
        let c = parsed("font-thicken = true\nfont-thicken-strength = 40");
        assert!(c.font_thicken);
        assert_eq!(c.font_thicken_strength, 40);
        assert_eq!(parsed("font-thicken-strength = 300").font_thicken_strength, 255);
        assert!(!parsed("cursor-click-to-move = false").cursor_click_to_move);
    }

    #[test]
    fn theme_paths_expand_tilde() {
        let Some(home) = home_dir() else { return };
        assert_eq!(expand_home("~/themes/x"), home.join("themes/x"));
        assert_eq!(expand_home(r"~\themes\x"), home.join(r"themes\x"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("dracula"), PathBuf::from("dracula"));
        assert_eq!(expand_home("~user/x"), PathBuf::from("~user/x"));
    }

    #[test]
    fn click_action_defaults() {
        let d = Config::default();
        assert_eq!(d.right_click_action, RightClickAction::ContextMenu);
        assert_eq!(d.middle_click_action, MiddleClickAction::PrimaryPaste);
        // Empty config keeps the defaults.
        assert_eq!(parsed("").right_click_action, RightClickAction::ContextMenu);
        assert_eq!(
            parsed("").middle_click_action,
            MiddleClickAction::PrimaryPaste
        );
    }

    #[test]
    fn right_click_action_parses_each_value() {
        assert_eq!(
            parsed("right-click-action = context-menu").right_click_action,
            RightClickAction::ContextMenu
        );
        assert_eq!(
            parsed("right-click-action = copy").right_click_action,
            RightClickAction::Copy
        );
        assert_eq!(
            parsed("right-click-action = paste").right_click_action,
            RightClickAction::Paste
        );
        assert_eq!(
            parsed("right-click-action = copy-or-paste").right_click_action,
            RightClickAction::CopyOrPaste
        );
        assert_eq!(
            parsed("right-click-action = ignore").right_click_action,
            RightClickAction::Ignore
        );
        // Empty resets to default; garbage keeps the current (here, the default).
        assert_eq!(
            parsed("right-click-action =").right_click_action,
            RightClickAction::ContextMenu
        );
        assert_eq!(
            parsed("right-click-action = nonsense").right_click_action,
            RightClickAction::ContextMenu
        );
    }

    #[test]
    fn middle_click_action_parses_each_value() {
        assert_eq!(
            parsed("middle-click-action = primary-paste").middle_click_action,
            MiddleClickAction::PrimaryPaste
        );
        assert_eq!(
            parsed("middle-click-action = ignore").middle_click_action,
            MiddleClickAction::Ignore
        );
        assert_eq!(
            parsed("middle-click-action =").middle_click_action,
            MiddleClickAction::PrimaryPaste
        );
        assert_eq!(
            parsed("middle-click-action = nonsense").middle_click_action,
            MiddleClickAction::PrimaryPaste
        );
    }

    #[test]
    fn command_sets_shell() {
        assert_eq!(parsed("command = pwsh").shell.as_deref(), Some("pwsh"));
        // Quoted values are tolerated for paths with spaces.
        assert_eq!(
            parsed("command = \"C:\\Program Files\\bash.exe\"")
                .shell
                .as_deref(),
            Some("C:\\Program Files\\bash.exe")
        );
    }

    #[test]
    fn unknown_keys_are_ignored_not_fatal() {
        // A full Ghostty config has many keys we don't support; they must not
        // discard the keys we do support.
        let c = parsed("theme = catppuccin\nfont-family = JetBrains Mono\nfont-size = 14");
        assert_eq!(c.font_points, 14.0);
    }

    #[test]
    fn empty_value_resets_to_default() {
        let mut c = Config::default();
        c.parse("font-size = 30\ncursor-color = #ff0000");
        assert_eq!(c.font_points, 30.0);
        assert_eq!(c.cursor, Some(Rgb::new(0xff, 0, 0)));
        // An empty value restores the default (Ghostty semantics).
        c.parse("font-size =\ncursor-color =");
        assert_eq!(c.font_points, Config::default().font_points);
        assert_eq!(c.cursor, None);
    }

    #[test]
    fn empty_config_keeps_defaults() {
        let c = parsed("");
        let d = Config::default();
        assert_eq!((c.font_points, c.fg, c.bg), (d.font_points, d.fg, d.bg));
    }

    #[test]
    fn text_gamma_overrides_and_clamps() {
        assert_eq!(Config::default().text_gamma, 1.3);
        assert_eq!(parsed("").text_gamma, 1.3);
        assert_eq!(parsed("text-gamma = 1.6").text_gamma, 1.6);
        // Out-of-range values clamp to [0.5, 3.0].
        assert_eq!(parsed("text-gamma = 10.0").text_gamma, 3.0);
        assert_eq!(parsed("text-gamma = 0.1").text_gamma, 0.5);
    }

    #[test]
    fn a_utf8_bom_does_not_hide_the_first_key() {
        // Notepad and PowerShell's `-Encoding utf8` both write one.
        assert_eq!(
            parsed("\u{feff}window-save-state = always").window_save_state,
            WindowSaveState::Always
        );
    }

    #[test]
    fn search_colors_default_to_ghosttys_and_accept_cell_keywords() {
        let c = Config::default();
        assert_eq!(
            c.search_bg,
            TerminalColor::Color(Rgb::new(0xFF, 0xE0, 0x82))
        );
        assert_eq!(c.search_fg, TerminalColor::Color(Rgb::new(0, 0, 0)));
        assert_eq!(
            c.search_selected_bg,
            TerminalColor::Color(Rgb::new(0xF2, 0xA5, 0x7E))
        );
        // The two keywords defer to the cell rather than naming a color.
        assert_eq!(
            parsed("search-background = cell-foreground").search_bg,
            TerminalColor::CellForeground
        );
        assert_eq!(
            parsed("search-foreground = cell-background").search_fg,
            TerminalColor::CellBackground
        );
        // X11 names work here like every other color key.
        assert_eq!(
            parsed("search-selected-background = red").search_selected_bg,
            TerminalColor::Color(Rgb::new(0xFF, 0, 0))
        );
        let (fg, bg) = (Rgb::new(1, 2, 3), Rgb::new(4, 5, 6));
        assert_eq!(TerminalColor::CellForeground.resolve(fg, bg), fg);
        assert_eq!(TerminalColor::CellBackground.resolve(fg, bg), bg);
        assert_eq!(TerminalColor::Color(fg).resolve(bg, bg), fg);
    }

    #[test]
    fn padding_balance_shares_out_the_leftover_space() {
        use PaddingBalance::*;
        // Off: everything falls to the trailing edge, which is giest's old
        // (and Ghostty's default) behaviour.
        assert_eq!(balance_padding(None, 9.0, 20.0), (0.0, 9.0));
        // Equal: split down the middle, odd pixel to the trailing side.
        assert_eq!(balance_padding(Equal, 9.0, 20.0), (4.0, 5.0));
        // Balanced: same, until the leading half would exceed half a cell —
        // upstream's cap, which is what stops a nearly-whole extra row of space
        // from being parked above the first line.
        assert_eq!(balance_padding(Balanced, 9.0, 20.0), (4.0, 5.0));
        assert_eq!(balance_padding(Balanced, 19.0, 20.0), (9.0, 10.0));
        assert_eq!(balance_padding(Balanced, 30.0, 20.0), (10.0, 20.0), "capped");
        assert_eq!(balance_padding(Equal, 30.0, 20.0), (15.0, 15.0), "uncapped");
        // Nothing to share out, and a negative leftover can't invert the rect.
        assert_eq!(balance_padding(Equal, 0.0, 20.0), (0.0, 0.0));
        assert_eq!(balance_padding(Equal, -4.0, 20.0), (0.0, 0.0));
    }

    #[test]
    fn the_new_config_keys_parse() {
        assert_eq!(
            parsed("window-new-tab-position = end").new_tab_position,
            NewTabPosition::End
        );
        // Upstream's default is `current`, not `end` — giest used to always
        // append, so this changes where a new tab lands.
        assert_eq!(Config::default().new_tab_position, NewTabPosition::Current);
        assert_eq!(
            parsed("window-padding-balance = true").window_padding_balance,
            PaddingBalance::Balanced
        );
        assert_eq!(
            parsed("window-padding-balance = equal").window_padding_balance,
            PaddingBalance::Equal
        );
        assert_eq!(
            Config::default().window_padding_balance,
            PaddingBalance::None
        );
        assert_eq!(
            parsed("split-divider-color = #ff8800").split_divider_color,
            Some(Rgb::new(0xFF, 0x88, 0x00))
        );
        assert_eq!(Config::default().split_divider_color, None);
        // Ghostty's defaults, one of which giest previously had backwards:
        // it cleared the selection on every copy.
        assert!(Config::default().selection_clear_on_typing);
        assert!(!Config::default().selection_clear_on_copy);
        assert!(parsed("selection-clear-on-copy = true").selection_clear_on_copy);
        assert!(!parsed("selection-clear-on-typing = false").selection_clear_on_typing);
    }

    #[test]
    fn working_directory_resolves_its_special_values() {
        // `inherit` is the launching process's directory, which is what `None`
        // already means downstream.
        assert_eq!(parsed("working-directory = inherit").working_directory, None);
        assert_eq!(Config::default().working_directory, None);
        assert_eq!(
            parsed(r"working-directory = C:\src").working_directory,
            Some(PathBuf::from(r"C:\src"))
        );
        // `home` and `~/` need a home directory to exist; on a machine that has
        // one they resolve under it.
        if let Some(home) = home_dir() {
            assert_eq!(parsed("working-directory = home").working_directory, Some(home.clone()));
            assert_eq!(
                parsed("working-directory = ~/src").working_directory,
                Some(home.join("src"))
            );
        }
    }

    #[test]
    fn selection_word_chars_parses_characters_not_bytes() {
        // Empty by default = "the engine's own boundaries", so the list isn't
        // duplicated here where it could drift from upstream's.
        assert!(Config::default().selection_word_chars.is_empty());

        let c = parsed("selection-word-chars =  \t'\"|:,()[]{}<>$").selection_word_chars;
        // NUL is always a boundary upstream, and an explicit list replaces the
        // engine's defaults wholesale — so it has to be prepended here.
        assert_eq!(c[0], '\0');
        assert!(c.contains(&'(') && c.contains(&'$') && c.contains(&':'));

        // A multi-byte boundary (upstream's own default list contains U+2502)
        // must survive as one character, not three.
        let c = parsed("selection-word-chars = │").selection_word_chars;
        assert_eq!(c, vec!['\0', '│']);

        // `\t` is an escape, as in Ghostty's documented default.
        let c = parsed(r"selection-word-chars = \t").selection_word_chars;
        assert_eq!(c, vec!['\0', '\t']);
        let c = parsed(r"selection-word-chars = \\").selection_word_chars;
        assert_eq!(c, vec!['\0', '\\']);

        // An empty value resets to the default (the engine's list).
        assert!(
            parsed("selection-word-chars = abc\nselection-word-chars =")
                .selection_word_chars
                .is_empty()
        );
    }

    #[test]
    fn font_synthetic_style_has_the_packed_struct_grammar() {
        // On by default, all three.
        assert_eq!(Config::default().font_synthetic_style, SyntheticStyle::all(true));
        // A bare bool sets every flag.
        assert_eq!(
            parsed("font-synthetic-style = false").font_synthetic_style,
            SyntheticStyle::all(false)
        );
        assert_eq!(
            parsed("font-synthetic-style = true").font_synthetic_style,
            SyntheticStyle::all(true)
        );
        // A list starts from the *defaults*, so `no-bold` disables only bold —
        // upstream calls this out as the easy mistake, and it must not be
        // "helpfully" extended to bold-italic.
        let s = parsed("font-synthetic-style = no-bold").font_synthetic_style;
        assert!(!s.bold && s.italic && s.bold_italic);
        let s = parsed("font-synthetic-style = no-bold,no-italic").font_synthetic_style;
        assert!(!s.bold && !s.italic && s.bold_italic);
        assert!(!parsed("font-synthetic-style = no-bold-italic").font_synthetic_style.bold_italic);
        // One unknown token rejects the whole value rather than half-applying it.
        assert_eq!(
            parsed("font-synthetic-style = no-bold,nope").font_synthetic_style,
            SyntheticStyle::all(true)
        );
        // An empty value resets to the default.
        assert_eq!(
            parsed("font-synthetic-style = false\nfont-synthetic-style =").font_synthetic_style,
            SyntheticStyle::all(true)
        );
    }

    #[test]
    fn adjust_metrics_are_deltas_not_settings() {
        // The trap the upstream docs call out: `1` adds a pixel, it does not set
        // the value to 1.
        assert_eq!(MetricModifier::parse("1"), Some(MetricModifier::Pixels(1)));
        assert_eq!(MetricModifier::parse("-2"), Some(MetricModifier::Pixels(-2)));
        assert_eq!(
            MetricModifier::parse("20%"),
            Some(MetricModifier::Percent(0.2))
        );
        assert_eq!(MetricModifier::parse("nope"), None);

        assert_eq!(MetricModifier::None.apply(4.0), 4.0);
        assert_eq!(MetricModifier::Pixels(1).apply(4.0), 5.0);
        assert_eq!(MetricModifier::Percent(0.5).apply(4.0), 6.0);
        // Clamped to at least 1: a zero-thickness line is invisible, which reads
        // as a missing glyph rather than a too-aggressive setting.
        assert_eq!(MetricModifier::Pixels(-10).apply_thickness(2.0), 1.0);
        assert_eq!(MetricModifier::Percent(-1.0).apply_thickness(8.0), 1.0);
        // …but a *position* is not clamped: zero and negative are meaningful
        // there (an overline sits at 0 by definition).
        assert_eq!(MetricModifier::Pixels(-10).apply(2.0), -8.0);

        assert_eq!(
            parsed("adjust-box-thickness = 2").adjust.box_thickness,
            MetricModifier::Pixels(2)
        );
        assert_eq!(Config::default().adjust, MetricAdjust::default());
    }

    #[test]
    fn window_save_state_parses_and_only_always_restores() {
        assert_eq!(Config::default().window_save_state, WindowSaveState::Default);
        assert_eq!(
            parsed("window-save-state = always").window_save_state,
            WindowSaveState::Always
        );
        assert_eq!(
            parsed("window-save-state = never").window_save_state,
            WindowSaveState::Never
        );
        assert_eq!(
            parsed("window-save-state = default").window_save_state,
            WindowSaveState::Default
        );
        // `default` means "when the OS asks", and Windows never does.
        assert!(WindowSaveState::Always.restores());
        assert!(!WindowSaveState::Default.restores());
        assert!(!WindowSaveState::Never.restores());
    }

    #[test]
    fn shell_integration_keys_parse_with_upstream_defaults() {
        use crate::profiles::{ShellFeatures, ShellIntegration};
        let d = Config::default();
        assert_eq!(d.shell_integration, ShellIntegration::Detect);
        let f = d.shell_integration_features;
        assert!(f.cursor && f.title && f.path);
        assert!(!f.sudo && !f.ssh_env && !f.ssh_terminfo);

        assert_eq!(parsed("shell-integration = none").shell_integration, ShellIntegration::None);
        assert_eq!(parsed("shell-integration = zsh").shell_integration, ShellIntegration::Zsh);
        assert_eq!(
            parsed("shell-integration = nushell").shell_integration,
            ShellIntegration::Nushell
        );
        // Invalid keeps the prior value; empty resets.
        assert_eq!(
            parsed("shell-integration = fish\nshell-integration = wat").shell_integration,
            ShellIntegration::Fish
        );
        assert_eq!(
            parsed("shell-integration = fish\nshell-integration =").shell_integration,
            ShellIntegration::Detect
        );

        let f = parsed("shell-integration-features = no-cursor,sudo").shell_integration_features;
        assert!(!f.cursor && f.sudo && f.title && f.path);
        // Omitted features take their *default*, not the previous line's value.
        let f = parsed("shell-integration-features = no-title\nshell-integration-features = sudo")
            .shell_integration_features;
        assert!(f.title && f.sudo);
        let f = parsed("shell-integration-features = false").shell_integration_features;
        assert_eq!(
            f,
            ShellFeatures {
                cursor: false,
                sudo: false,
                title: false,
                ssh_env: false,
                ssh_terminfo: false,
                path: false
            }
        );
        assert!(parsed("shell-integration-features = true").shell_integration_features.ssh_terminfo);
        // An unknown feature rejects the whole value.
        let f = parsed("shell-integration-features = no-cursor,bogus").shell_integration_features;
        assert!(f.cursor);
    }

    #[test]
    fn child_exit_keys_parse_with_upstream_defaults() {
        let d = Config::default();
        assert!(!d.wait_after_command);
        assert_eq!(d.abnormal_command_exit_runtime_ms, 250);
        assert!(parsed("wait-after-command = true").wait_after_command);
        assert!(!parsed("wait-after-command = true\nwait-after-command =").wait_after_command);
        assert_eq!(
            parsed("abnormal-command-exit-runtime = 1000").abnormal_command_exit_runtime_ms,
            1000
        );
        // Garbage keeps the current value; empty resets.
        assert_eq!(
            parsed("abnormal-command-exit-runtime = 9\nabnormal-command-exit-runtime = x")
                .abnormal_command_exit_runtime_ms,
            9
        );
        assert_eq!(
            parsed("abnormal-command-exit-runtime = 9\nabnormal-command-exit-runtime =")
                .abnormal_command_exit_runtime_ms,
            250
        );
        assert_eq!(parsed("initial-command = cmd.exe").initial_command.as_deref(), Some("cmd.exe"));
        assert_eq!(parsed("initial-command = x\ninitial-command =").initial_command, None);
    }

    #[test]
    fn env_is_an_ordered_map_with_reset_and_removal() {
        let kv = |s: &[(&str, &str)]| {
            s.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(parsed("env = A=1\nenv = B=x=y").env, kv(&[("A", "1"), ("B", "x=y")]));
        // Re-setting overwrites in place; `KEY=` removes; empty resets all.
        assert_eq!(parsed("env = A=1\nenv = B=2\nenv = A=3").env, kv(&[("A", "3"), ("B", "2")]));
        assert_eq!(parsed("env = A=1\nenv = B=2\nenv = A=").env, kv(&[("B", "2")]));
        assert!(parsed("env = A=1\nenv =").env.is_empty());
        // No `=` at all is not a pair.
        assert!(parsed("env = A").env.is_empty());
    }

    #[test]
    fn input_parses_raw_and_path_with_zig_escapes() {
        let c = parsed("input = raw:echo hi\\r\ninput = plain\\x1b[A\ninput = path:C:\\\\x.txt");
        assert_eq!(
            c.input,
            vec![
                InputSource::Raw(b"echo hi\r".to_vec()),
                InputSource::Raw(b"plain\x1b[A".to_vec()),
                InputSource::Path(PathBuf::from("C:\\x.txt")),
            ]
        );
        // Quoted values are unquoted first; `\u{..}` is UTF-8 encoded.
        assert_eq!(
            parsed("input = \"a\\u{e9}\\n\"").input,
            vec![InputSource::Raw("a\u{e9}\n".as_bytes().to_vec())]
        );
        // An invalid escape rejects that value only; empty resets.
        assert_eq!(parsed("input = a\ninput = bad\\q").input.len(), 1);
        assert!(parsed("input = a\ninput =").input.is_empty());
    }

    #[test]
    fn input_resolution_is_all_or_nothing() {
        let dir = std::env::temp_dir().join(format!("giest-input-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("in.txt");
        std::fs::write(&f, b"FILE").unwrap();
        let ok = [InputSource::Raw(b"a".to_vec()), InputSource::Path(f.clone()), InputSource::Raw(b"b".to_vec())];
        assert_eq!(resolve_input(&ok).as_deref(), Some(&b"aFILEb"[..]));
        let bad = [InputSource::Raw(b"a".to_vec()), InputSource::Path(dir.join("missing"))];
        assert_eq!(resolve_input(&bad), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirm_close_surface_parses_ghostty_enum() {
        assert_eq!(Config::default().confirm_close, ConfirmClose::WhenBusy);
        assert_eq!(parsed("confirm-close-surface = false").confirm_close, ConfirmClose::Never);
        assert_eq!(parsed("confirm-close-surface = true").confirm_close, ConfirmClose::WhenBusy);
        assert_eq!(parsed("confirm-close-surface = always").confirm_close, ConfirmClose::Always);
        assert_eq!(
            parsed("confirm-close-surface = false\nconfirm-close-surface = bogus").confirm_close,
            ConfirmClose::Never
        );
        assert_eq!(
            parsed("confirm-close-surface = false\nconfirm-close-surface =").confirm_close,
            ConfirmClose::WhenBusy
        );
    }

    #[test]
    fn confirm_close_decision_table() {
        use super::needs_confirm as nc;
        // `false` never asks, whatever the pane is doing.
        for busy in [None, Some(true), Some(false)] {
            assert!(!nc(ConfirmClose::Never, busy), "{busy:?}");
            assert!(nc(ConfirmClose::Always, busy), "{busy:?}");
        }
        // `true` asks only when something is (or might be) running.
        assert!(nc(ConfirmClose::WhenBusy, Some(true)));
        assert!(!nc(ConfirmClose::WhenBusy, Some(false)));
        // Unknown → confirm: a shell with no OSC 133 marks must not lose work.
        assert!(nc(ConfirmClose::WhenBusy, None));
    }

    #[test]
    fn resize_overlay_keys_parse_ghostty_values() {
        let d = Config::default();
        assert_eq!(d.resize_overlay, ResizeOverlay::AfterFirst);
        assert_eq!(d.resize_overlay_position, ResizeOverlayPosition::Center);
        assert_eq!(d.resize_overlay_duration_ms, 750);

        assert_eq!(parsed("resize-overlay = always").resize_overlay, ResizeOverlay::Always);
        assert_eq!(parsed("resize-overlay = never").resize_overlay, ResizeOverlay::Never);
        assert_eq!(
            parsed("resize-overlay-position = bottom-right").resize_overlay_position,
            ResizeOverlayPosition::BottomRight
        );
        // Garbage keeps the current value; empty resets.
        assert_eq!(
            parsed("resize-overlay = never\nresize-overlay = bogus").resize_overlay,
            ResizeOverlay::Never
        );
        assert_eq!(
            parsed("resize-overlay = never\nresize-overlay =").resize_overlay,
            ResizeOverlay::AfterFirst
        );
    }

    /// The default policy, so each test below varies one knob at a time.
    fn policy() -> ClipboardPolicy {
        Config::default().clipboard
    }

    #[test]
    fn clipboard_defaults_match_ghostty() {
        let p = policy();
        // Reads leak the clipboard *out* to the program, so they're asked for;
        // writes only overwrite it, so they're allowed.
        assert_eq!(p.read, ClipboardAccess::Ask);
        assert_eq!(p.write, ClipboardAccess::Allow);
        assert!(p.trim_trailing_spaces);
        assert!(p.paste_protection);
        assert!(p.paste_bracketed_safe);
    }

    #[test]
    fn clipboard_access_keys_parse_ghostty_values() {
        assert_eq!(parsed("clipboard-read = allow").clipboard.read, ClipboardAccess::Allow);
        assert_eq!(parsed("clipboard-read = deny").clipboard.read, ClipboardAccess::Deny);
        assert_eq!(parsed("clipboard-write = ask").clipboard.write, ClipboardAccess::Ask);
        // Garbage keeps the current value; empty resets to the default.
        assert_eq!(
            parsed("clipboard-write = deny\nclipboard-write = maybe")
                .clipboard
                .write,
            ClipboardAccess::Deny
        );
        assert_eq!(
            parsed("clipboard-write = deny\nclipboard-write =").clipboard.write,
            ClipboardAccess::Allow
        );
    }

    #[test]
    fn plain_text_is_always_safe_to_paste() {
        assert!(!paste_is_unsafe(policy(), false, "hello"));
        assert!(!paste_is_unsafe(policy(), true, "hello"));
        // A trailing-newline-free multi-token command is still one line.
        assert!(!paste_is_unsafe(policy(), false, "git commit -m 'x'"));
    }

    #[test]
    fn newlines_are_unsafe_unless_the_program_brackets_the_paste() {
        // Unbracketed, a newline is a command the shell runs immediately.
        assert!(paste_is_unsafe(policy(), false, "ls\nrm -rf /"));
        assert!(paste_is_unsafe(policy(), false, "trailing\n"));
        // Bracketed, the program has promised to treat it as data.
        assert!(!paste_is_unsafe(policy(), true, "ls\nrm -rf /"));
    }

    #[test]
    fn the_end_marker_is_never_trusted_even_when_bracketed() {
        // This is the attack `clipboard-paste-bracketed-safe` cannot defend
        // against: the payload closes the bracket itself, so everything after
        // it arrives as if typed. Checked *before* the bracketed-safe bail-out.
        let attack = "safe\x1b[201~rm -rf /";
        assert!(paste_is_unsafe(policy(), true, attack));
        assert!(paste_is_unsafe(policy(), false, attack));
        // And still flagged with bracketed-safe explicitly on.
        let trusting = ClipboardPolicy {
            paste_bracketed_safe: true,
            ..policy()
        };
        assert!(paste_is_unsafe(trusting, true, attack));
    }

    #[test]
    fn distrusting_bracketing_falls_back_to_the_plain_rule() {
        let strict = ClipboardPolicy {
            paste_bracketed_safe: false,
            ..policy()
        };
        // Bracketing no longer excuses a newline…
        assert!(paste_is_unsafe(strict, true, "ls\nrm -rf /"));
        // …but plain text is still fine.
        assert!(!paste_is_unsafe(strict, true, "hello"));
    }

    #[test]
    fn disabling_protection_allows_everything() {
        let off = ClipboardPolicy {
            paste_protection: false,
            ..policy()
        };
        assert!(!paste_is_unsafe(off, false, "ls\nrm -rf /"));
        assert!(!paste_is_unsafe(off, true, "safe\x1b[201~rm -rf /"));
    }

    #[test]
    fn scrollbar_key_parses_ghostty_values() {
        assert_eq!(Config::default().scrollbar, Scrollbar::System);
        assert_eq!(parsed("scrollbar = never").scrollbar, Scrollbar::Never);
        assert_eq!(parsed("scrollbar = system").scrollbar, Scrollbar::System);
        // Ghostty has no third value; anything else keeps the current one…
        assert_eq!(
            parsed("scrollbar = never\nscrollbar = always").scrollbar,
            Scrollbar::Never
        );
        // …and an empty value resets to the default.
        assert_eq!(parsed("scrollbar = never\nscrollbar =").scrollbar, Scrollbar::System);
    }

    #[test]
    fn resize_overlay_duration_parses_ghostty_grammar() {
        let ms = |s: &str| parsed(&format!("resize-overlay-duration = {s}")).resize_overlay_duration_ms;
        assert_eq!(ms("750ms"), 750);
        assert_eq!(ms("45s"), 45_000);
        // Components add…
        assert_eq!(ms("1h30m"), 5_400_000_u64.min(60_000));
        // …and repeat rather than overwrite (1h1h == 2h), though both clamp here.
        assert_eq!(ms("2s500ms"), 2_500);
        // A bare integer is milliseconds (a giest superset).
        assert_eq!(ms("300"), 300);
        // Clamped at both ends.
        assert_eq!(ms("10ms"), 250);
        assert_eq!(ms("999y"), 60_000);
        // Sub-millisecond parses but contributes nothing, then clamps up.
        assert_eq!(ms("500us"), 250);
        // Garbage keeps the current value.
        assert_eq!(
            parsed("resize-overlay-duration = 2s\nresize-overlay-duration = soon")
                .resize_overlay_duration_ms,
            2_000
        );
    }

    #[test]
    fn font_style_has_three_meanings_not_two() {
        let one = |body: &str| parsed(body).font_styles[0].clone();
        assert_eq!(one("font-style = default"), FontStyle::Default);
        assert_eq!(one("font-style = false"), FontStyle::Disabled);
        assert_eq!(one("font-style = Heavy"), FontStyle::Named("Heavy".into()));
        // A style name can contain spaces ("Light Italic" is a real subfamily),
        // so only the surrounding whitespace goes.
        assert_eq!(
            one("font-style =  Light Italic "),
            FontStyle::Named("Light Italic".into())
        );
        // `true` is not a value upstream gives meaning to, and *is* a name a
        // font could advertise — so it is a name, not the opposite of `false`.
        assert_eq!(one("font-style = true"), FontStyle::Named("true".into()));
        // An empty value resets, like every other key.
        assert_eq!(one("font-style = Heavy\nfont-style ="), FontStyle::Default);
    }

    #[test]
    fn each_font_style_slot_is_independent() {
        let c = parsed(
            "font-style = Light\n\
             font-style-bold = Semibold\n\
             font-style-italic = false\n\
             font-style-bold-italic = default",
        );
        assert_eq!(c.font_styles[0], FontStyle::Named("Light".into()));
        assert_eq!(c.font_styles[1], FontStyle::Named("Semibold".into()));
        assert_eq!(c.font_styles[2], FontStyle::Disabled);
        assert_eq!(c.font_styles[3], FontStyle::Default);
    }

    #[test]
    fn font_variation_parses_ghosttys_grammar() {
        let one = |body: &str| parsed(body).font_variations[0].clone();
        assert_eq!(
            one("font-variation = wght=200"),
            vec![FontVariation { tag: *b"wght", value: 200.0 }]
        );
        // Upstream trims around **both** halves — unlike `font-feature`, whose
        // value is handed to rustybuzz's stricter parser.
        assert_eq!(one("font-variation = wght = 200.5")[0].value, 200.5);
        // Repeatable: one axis per line, appending.
        assert_eq!(
            one("font-variation = wght=200\nfont-variation = wdth=90")
                .iter()
                .map(|v| (v.tag_str(), v.value))
                .collect::<Vec<_>>(),
            vec![("wght".to_string(), 200.0), ("wdth".to_string(), 90.0)]
        );
        // …and **not** comma-split, which `font-feature` is: upstream takes one
        // axis per occurrence, so accepting a list here would bind a config a
        // real Ghostty rejects.
        assert!(one("font-variation = wght=200, wdth=90").is_empty());
        // An empty value resets the slot, like every other repeatable key.
        assert!(one("font-variation = wght=200\nfont-variation =").is_empty());
        // A tag must be exactly four ASCII characters, and the value a number.
        for bad in ["wgh=200", "weight=200", "wght=heavy", "wght", "=200", "wgh†=200"] {
            assert!(
                one(&format!("font-variation = {bad}")).is_empty(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn each_font_variation_slot_is_its_own_list() {
        // Upstream hands each style descriptor **only** its own key's list, so
        // `font-variation` alone leaves the bold face at the font's default.
        // Surprising, and parity — pinned so it can't drift into inheritance.
        let c = parsed(
            "font-variation = wght=300\n\
             font-variation-bold = wght=700\n\
             font-variation-italic = slnt=-10\n\
             font-variation-bold-italic = wght=700\n\
             font-variation-bold-italic = slnt=-10",
        );
        assert_eq!(c.font_variations[0].len(), 1);
        assert_eq!(c.font_variations[1][0].value, 700.0);
        assert_eq!(c.font_variations[2][0].tag_str(), "slnt");
        assert_eq!(c.font_variations[3].len(), 2);
        // The regular slot is untouched by the others.
        assert_eq!(c.font_variations[0][0].value, 300.0);
    }

    #[test]
    fn undo_timeout_parses_the_duration_grammar_and_defaults_to_five_seconds() {
        let ms = |s: &str| parsed(&format!("undo-timeout = {s}")).undo_timeout_ms;
        assert_eq!(Config::default().undo_timeout_ms, 5_000);
        assert_eq!(ms("45s"), 45_000);
        assert_eq!(ms("1h30m"), 5_400_000);
        // **Unclamped, both ends, deliberately**: upstream documents `0` as
        // "undo is off" and a very large value as the sanctioned way to keep
        // operations around, so a clamp here would break both.
        assert_eq!(ms("0"), 0);
        assert_eq!(ms("1y"), 31_536_000_000);
        // Garbage keeps the current value rather than resetting to the default.
        assert_eq!(parsed("undo-timeout = 2s\nundo-timeout = later").undo_timeout_ms, 2_000);
    }

    #[test]
    fn show_resize_overlay_gates_the_first_layout() {
        use super::show_resize_overlay as show;
        // The whole point of `after-first`: a session's initial sizing is not a
        // "resize" the user did, so it must not flash.
        assert!(!show(ResizeOverlay::AfterFirst, true));
        assert!(show(ResizeOverlay::AfterFirst, false));
        assert!(show(ResizeOverlay::Always, true));
        assert!(show(ResizeOverlay::Always, false));
        assert!(!show(ResizeOverlay::Never, true));
        assert!(!show(ResizeOverlay::Never, false));
    }

    #[test]
    fn osc_color_report_format_parses_each_value() {
        assert_eq!(Config::default().osc_color_report_format, OscColorReportFormat::Bits16);
        assert_eq!(
            parsed("osc-color-report-format = none").osc_color_report_format,
            OscColorReportFormat::None
        );
        assert_eq!(
            parsed("osc-color-report-format = 8-bit").osc_color_report_format,
            OscColorReportFormat::Bits8
        );
        // Garbage keeps the current value; empty resets.
        assert_eq!(
            parsed("osc-color-report-format = none\nosc-color-report-format = bogus")
                .osc_color_report_format,
            OscColorReportFormat::None
        );
        assert_eq!(
            parsed("osc-color-report-format = none\nosc-color-report-format =")
                .osc_color_report_format,
            OscColorReportFormat::Bits16
        );
    }

    #[test]
    fn bell_features_defaults_match_ghostty() {
        let b = Config::default().bell;
        // attention/title on, everything else off — note `border` is OFF, which
        // differs from giest's pre-parity default of always flashing the pane.
        assert!(b.attention && b.title);
        assert!(!b.system && !b.audio && !b.border);
    }

    #[test]
    fn bell_features_parses_feature_list() {
        // A feature list starts from the *defaults*, so attention/title survive.
        let b = parsed("bell-features = audio").bell;
        assert!(b.audio && b.attention && b.title);
        assert!(!b.system && !b.border);

        // `no-` disables one feature and leaves the rest at their defaults.
        let b = parsed("bell-features = no-title").bell;
        assert!(!b.title && b.attention);

        let b = parsed("bell-features = system,border,no-attention").bell;
        assert!(b.system && b.border && b.title);
        assert!(!b.attention && !b.audio);
    }

    #[test]
    fn bell_features_bool_shorthand_sets_all() {
        for v in ["true", "t", "1"] {
            let b = parsed(&format!("bell-features = {v}")).bell;
            assert!(b.system && b.audio && b.attention && b.title && b.border, "{v}");
        }
        for v in ["false", "f", "0"] {
            let b = parsed(&format!("bell-features = {v}")).bell;
            assert!(!b.system && !b.audio && !b.attention && !b.title && !b.border, "{v}");
        }
        // Ghostty's packed-struct parser takes a *stricter* bool set than its
        // ordinary one, so these are unknown feature names, not booleans — and an
        // unknown name keeps the current value (see the wholesale-reject test).
        let b = parsed("bell-features = border\nbell-features = on").bell;
        assert!(b.border, "'on' is not a packed-struct boolean");
    }

    #[test]
    fn bell_features_rejects_unknown_token_wholesale() {
        // The valid `audio` before the bad token must NOT be kept.
        let c = parsed("bell-features = audio,bogus");
        assert_eq!(c.bell, BellFeatures::default());
    }

    #[test]
    fn bell_features_replaces_rather_than_accumulates() {
        // The second line wins outright: `system` is dropped, not merged.
        let b = parsed("bell-features = system\nbell-features = border").bell;
        assert!(b.border && !b.system);
    }

    #[test]
    fn bell_features_empty_resets_to_default() {
        let b = parsed("bell-features = true\nbell-features =").bell;
        assert_eq!(b, BellFeatures::default());
    }

    #[test]
    fn bell_audio_keys_parse() {
        assert_eq!(Config::default().bell_audio_path, None);
        assert_eq!(Config::default().bell_audio_volume, 0.5);
        assert_eq!(
            parsed(r"bell-audio-path = C:\sounds\ding.wav").bell_audio_path.as_deref(),
            Some(r"C:\sounds\ding.wav")
        );
        // Parsed and clamped even though playback can't honor it yet.
        assert_eq!(parsed("bell-audio-volume = 0.25").bell_audio_volume, 0.25);
        assert_eq!(parsed("bell-audio-volume = 9").bell_audio_volume, 1.0);
    }

    #[test]
    fn background_opacity_parses_and_clamps() {
        assert_eq!(Config::default().background_opacity, 1.0);
        assert_eq!(parsed("background-opacity = 0.85").background_opacity, 0.85);
        // Ghostty clamps out-of-range opacities instead of rejecting them.
        assert_eq!(parsed("background-opacity = -1").background_opacity, 0.0);
        assert_eq!(parsed("background-opacity = 2").background_opacity, 1.0);
        // Empty resets; garbage keeps the current value.
        assert_eq!(
            parsed("background-opacity = 0.5\nbackground-opacity =").background_opacity,
            1.0
        );
        assert_eq!(
            parsed("background-opacity = 0.5\nbackground-opacity = nope").background_opacity,
            0.5
        );
    }

    #[test]
    fn background_opacity_cells_parses() {
        assert!(!Config::default().background_opacity_cells);
        assert!(parsed("background-opacity-cells = true").background_opacity_cells);
        assert!(!parsed("background-opacity-cells = false").background_opacity_cells);
        assert!(
            !parsed("background-opacity-cells = true\nbackground-opacity-cells =")
                .background_opacity_cells
        );
    }

    #[test]
    fn unfocused_split_opacity_clamps_to_ghostty_range() {
        assert_eq!(Config::default().unfocused_split_opacity, 0.7);
        assert_eq!(parsed("unfocused-split-opacity = 0.4").unfocused_split_opacity, 0.4);
        // The floor is 0.15, not 0 — Ghostty disallows a fully invisible split.
        assert_eq!(parsed("unfocused-split-opacity = 0.0").unfocused_split_opacity, 0.15);
        assert_eq!(parsed("unfocused-split-opacity = 2.0").unfocused_split_opacity, 1.0);
    }

    #[test]
    fn unfocused_split_fill_defaults_none_and_parses_named_color() {
        assert_eq!(Config::default().unfocused_split_fill, None);
        assert_eq!(
            parsed("unfocused-split-fill = #f80").unfocused_split_fill,
            Some(Rgb::new(0xff, 0x88, 0x00))
        );
        assert_eq!(
            parsed("unfocused-split-fill = rebeccapurple").unfocused_split_fill,
            Some(Rgb::new(0x66, 0x33, 0x99))
        );
    }

    #[test]
    fn cursor_and_faint_opacity_parse_and_clamp() {
        assert_eq!(Config::default().cursor_opacity, 1.0);
        // giest's faint used to be a hardcoded 0.55; Ghostty's default is 0.5.
        assert_eq!(Config::default().faint_opacity, 0.5);
        assert_eq!(parsed("cursor-opacity = 0.4").cursor_opacity, 0.4);
        assert_eq!(parsed("cursor-opacity = 5").cursor_opacity, 1.0);
        assert_eq!(parsed("faint-opacity = 0.1").faint_opacity, 0.1);
        assert_eq!(parsed("faint-opacity = -3").faint_opacity, 0.0);
    }

    #[test]
    fn background_blur_parses_ghostty_grammar() {
        use super::BackgroundBlur;
        assert_eq!(Config::default().background_blur, BackgroundBlur::Off);
        assert_eq!(parsed("background-blur = true").background_blur, BackgroundBlur::On);
        assert_eq!(parsed("background-blur = false").background_blur, BackgroundBlur::Off);
        // Ghostty's parseBool takes `0`/`1`, so those are bools — NOT radii.
        assert_eq!(parsed("background-blur = 0").background_blur, BackgroundBlur::Off);
        assert_eq!(parsed("background-blur = 1").background_blur, BackgroundBlur::On);
        // Only 2 and up reach the numeric branch.
        assert_eq!(
            parsed("background-blur = 20").background_blur,
            BackgroundBlur::Radius(20)
        );
        // Out of u8 range: keep the current value, like Ghostty's parseInt error.
        assert_eq!(
            parsed("background-blur = 20\nbackground-blur = 300").background_blur,
            BackgroundBlur::Radius(20)
        );
        // macOS glass implies plain `true` off macOS.
        assert_eq!(
            parsed("background-blur = macos-glass-clear").background_blur,
            BackgroundBlur::On
        );

        assert_eq!(BackgroundBlur::On.intensity(), 20);
        // `Radius(0)` is disabled, matching Ghostty.
        assert!(!BackgroundBlur::Radius(0).enabled());
        assert!(BackgroundBlur::Radius(1).enabled());
    }

    #[test]
    fn window_geometry_keys() {
        let d = Config::default();
        assert_eq!((d.window_width, d.window_height), (0, 0), "0 means unset");
        assert_eq!(d.window_position_x, None);

        assert_eq!(parsed("window-width = 120").window_width, 120);
        assert_eq!(parsed("window-height = 40").window_height, 40);
        // Ghostty's 10x4 floor: a window below it is unusable, not merely small.
        assert_eq!(parsed("window-width = 2").window_width, 10);
        assert_eq!(parsed("window-height = 1").window_height, 4);
        // …but zero still means "let the OS decide", not "the minimum".
        assert_eq!(parsed("window-width = 0").window_width, 0);
        // Garbage keeps the current value; empty resets.
        assert_eq!(parsed("window-width = 80\nwindow-width = wide").window_width, 80);
        assert_eq!(parsed("window-width = 80\nwindow-width =").window_width, 0);

        assert_eq!(parsed("window-position-x = 100").window_position_x, Some(100));
        // Negative is legal: a monitor left of the primary has negative x.
        assert_eq!(parsed("window-position-y = -40").window_position_y, Some(-40));
        assert_eq!(
            parsed("window-position-x = 10\nwindow-position-x =").window_position_x,
            None
        );
    }

    #[test]
    fn mouse_scroll_multiplier_grammar() {
        let d = Config::default().mouse_scroll_multiplier;
        // Ghostty's split defaults: notched wheels move further per event.
        assert_eq!(d.precision, 1.0);
        assert_eq!(d.discrete, 3.0);

        // A bare number sets both.
        let m = parsed("mouse-scroll-multiplier = 2").mouse_scroll_multiplier;
        assert_eq!((m.precision, m.discrete), (2.0, 2.0));

        // Prefixes set them independently — Ghostty's own example.
        let m = parsed("mouse-scroll-multiplier = precision:0.1,discrete:3").mouse_scroll_multiplier;
        assert_eq!((m.precision, m.discrete), (0.1, 3.0));

        // One prefix leaves the other alone.
        let m = parsed("mouse-scroll-multiplier = discrete:5").mouse_scroll_multiplier;
        assert_eq!((m.precision, m.discrete), (1.0, 5.0));

        // Clamped to Ghostty's range at both ends.
        let m = parsed("mouse-scroll-multiplier = 0").mouse_scroll_multiplier;
        assert_eq!(m.discrete, 0.01, "zero would make the wheel a no-op");
        let m = parsed("mouse-scroll-multiplier = 999999").mouse_scroll_multiplier;
        assert_eq!(m.discrete, 10_000.0);

        // Garbage keeps the current value rather than half-applying.
        let m = parsed("mouse-scroll-multiplier = 4\nmouse-scroll-multiplier = sideways:2")
            .mouse_scroll_multiplier;
        assert_eq!((m.precision, m.discrete), (4.0, 4.0));
        // Empty resets.
        let m = parsed("mouse-scroll-multiplier = 4\nmouse-scroll-multiplier =")
            .mouse_scroll_multiplier;
        assert_eq!((m.precision, m.discrete), (1.0, 3.0));
    }

    #[test]
    fn scroll_to_bottom_flags() {
        let d = Config::default().scroll_to_bottom;
        // Ghostty's default is `keystroke, no-output`.
        assert!(d.keystroke && !d.output);

        let s = parsed("scroll-to-bottom = output").scroll_to_bottom;
        assert!(s.keystroke && s.output, "a list starts from the defaults");
        let s = parsed("scroll-to-bottom = no-keystroke").scroll_to_bottom;
        assert!(!s.keystroke && !s.output);
        let s = parsed("scroll-to-bottom = no-keystroke,output").scroll_to_bottom;
        assert!(!s.keystroke && s.output);
        // An unknown name rejects the whole value.
        let s = parsed("scroll-to-bottom = no-keystroke\nscroll-to-bottom = output,bogus")
            .scroll_to_bottom;
        assert!(!s.keystroke, "a bad value must not half-apply");
    }

    #[test]
    fn notify_on_command_finish_keys_match_ghostty() {
        let d = Config::default();
        // Opt-in: nothing happens until you ask for it.
        assert_eq!(d.notify_on_command_finish, NotifyOnCommandFinish::Never);
        assert!(d.notify_on_command_finish_action.bell);
        assert!(!d.notify_on_command_finish_action.notify);
        assert_eq!(d.notify_on_command_finish_after_ms, 5_000);

        for (v, want) in [
            ("never", NotifyOnCommandFinish::Never),
            ("unfocused", NotifyOnCommandFinish::Unfocused),
            ("always", NotifyOnCommandFinish::Always),
        ] {
            assert_eq!(
                parsed(&format!("notify-on-command-finish = {v}")).notify_on_command_finish,
                want
            );
        }
        // Unknown keeps the current value; empty resets to the default.
        assert_eq!(
            parsed("notify-on-command-finish = always\nnotify-on-command-finish = bogus")
                .notify_on_command_finish,
            NotifyOnCommandFinish::Always
        );
        assert_eq!(
            parsed("notify-on-command-finish = always\nnotify-on-command-finish =")
                .notify_on_command_finish,
            NotifyOnCommandFinish::Never
        );

        // Ghostty's own documented example.
        let a = parsed("notify-on-command-finish-action = no-bell,notify")
            .notify_on_command_finish_action;
        assert!(!a.bell && a.notify);
        // A list starts from the defaults, so naming only `notify` leaves the
        // bell on — same rule as `bell-features`.
        let a = parsed("notify-on-command-finish-action = notify").notify_on_command_finish_action;
        assert!(a.bell && a.notify);
        // One unknown name rejects the whole value.
        let a = parsed("notify-on-command-finish-action = no-bell\nnotify-on-command-finish-action = bell,nope")
            .notify_on_command_finish_action;
        assert!(!a.bell, "a bad value must not half-apply");

        // Ghostty's additive duration grammar, shared with resize-overlay.
        assert_eq!(
            parsed("notify-on-command-finish-after = 45s").notify_on_command_finish_after_ms,
            45_000
        );
        assert_eq!(
            parsed("notify-on-command-finish-after = 1h30m").notify_on_command_finish_after_ms,
            5_400_000
        );
        // Zero means "every command" — this key is deliberately not clamped the
        // way `resize-overlay-duration` is.
        assert_eq!(
            parsed("notify-on-command-finish-after = 0").notify_on_command_finish_after_ms,
            0
        );
    }

    #[test]
    fn notify_focus_gate() {
        use super::should_notify_on_finish as go;
        // never: nothing, either way.
        assert!(!go(NotifyOnCommandFinish::Never, true));
        assert!(!go(NotifyOnCommandFinish::Never, false));
        // unfocused: only when you've looked away.
        assert!(!go(NotifyOnCommandFinish::Unfocused, true));
        assert!(go(NotifyOnCommandFinish::Unfocused, false));
        // always: both.
        assert!(go(NotifyOnCommandFinish::Always, true));
        assert!(go(NotifyOnCommandFinish::Always, false));
    }

    #[test]
    fn background_image_keys_match_ghostty() {
        let d = Config::default();
        assert_eq!(d.background_image, None);
        assert_eq!(d.background_image_opacity, 1.0);
        assert_eq!(d.background_image_position, BackgroundImagePosition::Center);
        assert_eq!(d.background_image_fit, BackgroundImageFit::Contain);
        assert!(!d.background_image_repeat);

        // The path is stored raw; resolution happens at the use site.
        assert_eq!(
            parsed(r"background-image = C:\pics\wall.png")
                .background_image
                .as_deref(),
            Some(r"C:\pics\wall.png")
        );
        assert_eq!(
            parsed("background-image = wall.png\nbackground-image =").background_image,
            None
        );

        for (v, want) in [
            ("contain", BackgroundImageFit::Contain),
            ("cover", BackgroundImageFit::Cover),
            ("stretch", BackgroundImageFit::Stretch),
            ("none", BackgroundImageFit::None),
        ] {
            assert_eq!(
                parsed(&format!("background-image-fit = {v}")).background_image_fit,
                want
            );
        }
        // An unknown value keeps the current setting rather than resetting.
        assert_eq!(
            parsed("background-image-fit = cover\nbackground-image-fit = bogus").background_image_fit,
            BackgroundImageFit::Cover
        );

        for (v, want) in [
            ("top-left", BackgroundImagePosition::TopLeft),
            ("top-center", BackgroundImagePosition::TopCenter),
            ("top-right", BackgroundImagePosition::TopRight),
            ("center-left", BackgroundImagePosition::CenterLeft),
            ("center", BackgroundImagePosition::Center),
            // Ghostty's enum spells the middle both ways.
            ("center-center", BackgroundImagePosition::Center),
            ("center-right", BackgroundImagePosition::CenterRight),
            ("bottom-left", BackgroundImagePosition::BottomLeft),
            ("bottom-center", BackgroundImagePosition::BottomCenter),
            ("bottom-right", BackgroundImagePosition::BottomRight),
        ] {
            assert_eq!(
                parsed(&format!("background-image-position = {v}")).background_image_position,
                want,
                "{v}"
            );
        }

        assert!(parsed("background-image-repeat = true").background_image_repeat);
        assert!(!parsed("background-image-repeat = true\nbackground-image-repeat =")
            .background_image_repeat);
    }

    #[test]
    fn background_image_opacity_allows_values_above_one() {
        // Ghostty documents >1 as meaningful (the image ends up more opaque than
        // the background color), so this must NOT clamp to 1.0 the way
        // `background-opacity` does.
        assert_eq!(
            parsed("background-image-opacity = 1.5").background_image_opacity,
            1.5
        );
        assert_eq!(
            parsed("background-image-opacity = 0.25").background_image_opacity,
            0.25
        );
        // Negative is nonsense and clamps to zero (fully hidden image).
        assert_eq!(
            parsed("background-image-opacity = -1").background_image_opacity,
            0.0
        );
        assert_eq!(
            parsed("background-image-opacity = 2\nbackground-image-opacity =")
                .background_image_opacity,
            1.0
        );
    }

    #[test]
    fn path_values_resolve_relative_to_the_config_dir() {
        let dir = Path::new(r"C:\Users\me\AppData\Roaming\giest");
        assert_eq!(
            resolve_path("wall.png", Some(dir)),
            Some(dir.join("wall.png"))
        );
        assert_eq!(
            resolve_path(r"C:\pics\wall.png", Some(dir)),
            Some(PathBuf::from(r"C:\pics\wall.png"))
        );
        // Blank means unset, not "the config directory".
        assert_eq!(resolve_path("   ", Some(dir)), None);
        // With no config file at all, a relative path stays relative.
        assert_eq!(resolve_path("wall.png", None), Some(PathBuf::from("wall.png")));
    }

    #[test]
    fn font_family_and_feature_keys_parse() {
        let c = parsed(
            "font-family = Cascadia Code\n\
             font-family-bold = Cascadia Code SemiBold\n\
             font-family-italic = Cascadia Code Italic\n\
             font-feature = -calt\n\
             font-feature = ss01, cv01",
        );
        assert_eq!(c.font_family, vec!["Cascadia Code"]);
        assert_eq!(c.font_family_bold.as_deref(), Some("Cascadia Code SemiBold"));
        assert_eq!(c.font_family_italic.as_deref(), Some("Cascadia Code Italic"));
        assert_eq!(c.font_family_bold_italic, None);
        // Repeatable; each value is a comma-separated list.
        assert_eq!(c.font_features, vec!["-calt", "ss01", "cv01"]);
        // Comma-only split (not whitespace): a feature's own value syntax keeps
        // its spaces, so `liga off` is one feature the shaper resolves to liga=0.
        assert_eq!(parsed("font-feature = liga off").font_features, vec!["liga off"]);

        // Defaults and resets.
        assert!(Config::default().font_family.is_empty());
        assert!(Config::default().font_features.is_empty());
        assert!(
            parsed("font-family = Foo\nfont-family =")
                .font_family
                .is_empty()
        );
        // Repeatable: each line appends to the fallback chain, in order.
        assert_eq!(
            parsed("font-family = Foo\nfont-family = Bar").font_family,
            vec!["Foo", "Bar"]
        );
        // Only an empty value resets; `clear` is not special (stored as a literal
        // token the shaper later drops — matching Ghostty).
        assert!(parsed("font-feature = -calt\nfont-feature =").font_features.is_empty());
        assert_eq!(
            parsed("font-feature = -calt\nfont-feature = clear").font_features,
            vec!["-calt", "clear"]
        );
    }

    #[test]
    fn cursor_style_and_blink_parse() {
        let d = Config::default();
        assert_eq!(d.cursor_style, CursorShape::Block);
        assert_eq!(d.cursor_style_blink, None);

        assert_eq!(parsed("cursor-style = bar").cursor_style, CursorShape::Bar);
        assert_eq!(
            parsed("cursor-style = underline").cursor_style,
            CursorShape::Underline
        );
        // Garbage keeps the current; empty resets to default.
        assert_eq!(parsed("cursor-style = wat").cursor_style, CursorShape::Block);
        assert_eq!(parsed("cursor-style =").cursor_style, CursorShape::Block);

        assert_eq!(
            parsed("cursor-style-blink = true").cursor_style_blink,
            Some(true)
        );
        assert_eq!(
            parsed("cursor-style-blink = false").cursor_style_blink,
            Some(false)
        );
        assert_eq!(parsed("cursor-style-blink =").cursor_style_blink, None);
    }

    #[test]
    fn bold_color_and_bold_is_bright_parse() {
        assert_eq!(Config::default().bold_color, BoldColor::None);
        assert_eq!(parsed("bold-color = bright").bold_color, BoldColor::Bright);
        assert_eq!(
            parsed("bold-color = #ff8800").bold_color,
            BoldColor::Color(Rgb::new(0xff, 0x88, 0x00))
        );
        // Deprecated alias.
        assert_eq!(parsed("bold-is-bright = true").bold_color, BoldColor::Bright);
        assert_eq!(parsed("bold-is-bright = false").bold_color, BoldColor::None);
        // Empty resets; garbage keeps the current (default here).
        assert_eq!(parsed("bold-color =").bold_color, BoldColor::None);
        assert_eq!(parsed("bold-color = nonsense").bold_color, BoldColor::None);
    }

    #[test]
    fn minimum_contrast_parses_and_clamps() {
        assert_eq!(Config::default().min_contrast, 1.0);
        assert_eq!(parsed("minimum-contrast = 4.5").min_contrast, 4.5);
        // Clamped to [1.0, 21.0].
        assert_eq!(parsed("minimum-contrast = 0.2").min_contrast, 1.0);
        assert_eq!(parsed("minimum-contrast = 99").min_contrast, 21.0);
        assert_eq!(parsed("minimum-contrast =").min_contrast, 1.0);
    }

    #[test]
    fn theme_spec_selects_dark_variant() {
        // A bare name passes through; the dual `light:/dark:` form picks dark.
        assert_eq!(select_theme_variant("dracula"), "dracula");
        assert_eq!(select_theme_variant("light:rosewater,dark:mocha"), "mocha");
        assert_eq!(select_theme_variant("dark:nord"), "nord");
        assert_eq!(select_theme_variant("light:onlylight"), "onlylight");
        assert_eq!(select_theme_variant(""), "");
    }

    #[test]
    fn unresolved_theme_is_ignored_but_other_keys_apply() {
        // `theme` resolves out-of-band; an unknown theme is ignored and must not
        // discard the supported keys around it.
        let c = parsed("theme = does-not-exist\nfont-size = 21\nbackground = #010203");
        assert_eq!(c.font_points, 21.0);
        assert_eq!(c.bg, Rgb::new(1, 2, 3));
    }

    #[test]
    fn diagnostics_collect_unknown_keys_bad_values_and_malformed_lines() {
        let c = parsed("frobnicate = 1\nnot a key value line\nfont-size = 15\nbell-features = bogus");
        // Parsing otherwise proceeds exactly as before: the good line applies.
        assert_eq!(c.font_points, 15.0);
        assert_eq!(c.diagnostics.len(), 3, "{:?}", c.diagnostics);
        assert!(c.diagnostics[0].contains("frobnicate"));
        assert!(c.diagnostics[1].contains("malformed config line 2"));
        assert!(c.diagnostics[2].contains("bell-features"));
        // The stderr prefix is stripped for the dialog.
        assert!(c.diagnostics.iter().all(|d| !d.starts_with("giest: ")));
    }

    #[test]
    fn diagnostics_are_empty_for_a_clean_config_and_do_not_leak_between_loads() {
        let _ = parsed("frobnicate = 1");
        let c = parsed("font-size = 12");
        assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    }

    #[test]
    fn diagnostics_report_a_missing_theme() {
        let c = parsed("theme = does-not-exist");
        assert!(c.diagnostics.iter().any(|d| d.contains("does-not-exist")));
    }

    #[test]
    fn diagnostics_cover_unreadable_includes_across_files() {
        let s = Scratch::new();
        s.write("inc", "nope = 1");
        let root =
            s.write("config", "config-file = inc\nconfig-file = missing\nconfig-file = ?quiet");
        let c = Config::load_from_file(&root);
        assert_eq!(c.diagnostics.len(), 2, "{:?}", c.diagnostics);
        assert!(c.diagnostics.iter().any(|d| d.contains("'nope'")));
        assert!(c.diagnostics.iter().any(|d| d.contains("missing")));
    }

    #[test]
    fn app_notifications_defaults_are_all_on() {
        let d = Config::default().app_notifications;
        assert!(d.clipboard_copy && d.config_reload);
    }

    #[test]
    fn app_notifications_parses_flags_from_defaults() {
        let c = parsed("app-notifications = no-clipboard-copy");
        assert!(!c.app_notifications.clipboard_copy);
        assert!(c.app_notifications.config_reload);
        let c = parsed("app-notifications = no-config-reload, clipboard-copy");
        assert!(c.app_notifications.clipboard_copy && !c.app_notifications.config_reload);
        // A second line replaces rather than accumulates.
        let c = parsed(
            "app-notifications = no-clipboard-copy\napp-notifications = no-config-reload",
        );
        assert!(c.app_notifications.clipboard_copy && !c.app_notifications.config_reload);
    }

    #[test]
    fn app_notifications_bool_shorthand_errors_and_reset() {
        assert_eq!(
            parsed("app-notifications = false").app_notifications,
            AppNotifications { clipboard_copy: false, config_reload: false }
        );
        // Only the strict packed-struct booleans; `off` is an unknown flag and
        // rejects the value wholesale (the previous value stays).
        let c = parsed("app-notifications = false\napp-notifications = off");
        assert!(!c.app_notifications.clipboard_copy);
        assert!(c.diagnostics.iter().any(|d| d.contains("app-notifications")));
        let c = parsed("app-notifications = false\napp-notifications =");
        assert_eq!(c.app_notifications, AppNotifications::default());
    }
}
