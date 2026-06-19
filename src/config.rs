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
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::engine::{BoldColor, CursorShape, Rgb};

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
/// Windows has no PRIMARY selection, so `primary-paste` reads the system clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MiddleClickAction {
    /// Paste the (system) clipboard. The default.
    PrimaryPaste,
    /// Do nothing.
    Ignore,
}

/// User-facing configuration applied at startup.
#[derive(Clone, Debug)]
pub struct Config {
    /// Logical font size in points (scaled by the display DPI for the atlas).
    /// Ghostty `font-size`.
    pub font_points: f32,
    /// Primary font family (name or file path); `None` keeps the built-in
    /// JetBrains Mono. Ghostty `font-family`. *(Applied at startup; changing it
    /// needs a restart — config reload re-applies colors/size, not the font.)*
    pub font_family: Option<String>,
    /// Per-style family overrides; each `None` falls back to `font_family`.
    /// Ghostty `font-family-bold` / `-italic` / `-bold-italic`.
    pub font_family_bold: Option<String>,
    pub font_family_italic: Option<String>,
    pub font_family_bold_italic: Option<String>,
    /// OpenType feature specs applied while shaping, e.g. `-calt` (disable
    /// ligatures/contextual alternates), `ss01`, `cv01=2`. Repeatable. Ghostty
    /// `font-feature`.
    pub font_features: Vec<String>,
    /// Default foreground (text) color. Ghostty `foreground`.
    pub fg: Rgb,
    /// Default background color. Ghostty `background`.
    pub bg: Rgb,
    /// The 256-color palette (indices 0–15 = ANSI, 16–231 = color cube,
    /// 232–255 = grayscale ramp). Ghostty `palette`.
    pub palette: [Rgb; 256],
    /// Logical-point padding on the left/right of the grid. Ghostty
    /// `window-padding-x`.
    pub padding_x: f32,
    /// Logical-point padding above/below the grid. Ghostty `window-padding-y`.
    pub padding_y: f32,
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
    /// Maximum scrollback *lines* retained per pane. Ghostty's `scrollback-limit`
    /// is expressed in bytes; giest's underlying VT engine takes a line count, so
    /// the key name matches but the unit is lines.
    pub scrollback_limit: usize,
    /// Background color of selected cells. Ghostty `selection-background`.
    pub selection_bg: Rgb,
    /// Text color over a selection; `None` keeps each cell's own foreground.
    /// Ghostty `selection-foreground`.
    pub selection_fg: Option<Rgb>,
    /// Copy a selection to the clipboard as soon as it is made. Ghostty
    /// `copy-on-select` (an enum there; Windows has no primary selection, so
    /// `clipboard`/`primary`/`true` all map to true).
    pub copy_on_select: bool,
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
    /// Whether the visual bell (a brief flash on the pane that rang) is enabled.
    /// Ghostty `bell-features` — giest currently implements only the visual
    /// feature; the audible bell is a follow-up.
    pub bell_visual: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_points: 16.0,
            font_family: None,
            font_family_bold: None,
            font_family_italic: None,
            font_family_bold_italic: None,
            font_features: Vec::new(),
            fg: Rgb::new(0xc5, 0xc8, 0xc6),
            bg: Rgb::new(0x10, 0x12, 0x18),
            palette: xterm_palette(GIEST_ANSI16),
            padding_x: 20.0,
            padding_y: 2.0,
            text_gamma: 1.3,
            cursor: None,
            cursor_style: CursorShape::Block,
            cursor_style_blink: None,
            bold_color: BoldColor::None,
            min_contrast: 1.0,
            scrollback_limit: 10_000,
            selection_bg: Rgb::new(0x38, 0x5a, 0x9c),
            selection_fg: None,
            copy_on_select: false,
            right_click_action: RightClickAction::ContextMenu,
            middle_click_action: MiddleClickAction::PrimaryPaste,
            shell: None,
            keybinds: Vec::new(),
            bell_visual: true,
        }
    }
}

impl Config {
    /// Load configuration, applying the config file's overrides over the
    /// defaults. A missing file is fine; unreadable lines are logged and skipped.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_ghostty_config(&text),
            Err(_) => Self::default(),
        }
    }

    /// Build a [`Config`] by applying a Ghostty-format config body over the
    /// built-in defaults. This is the entry point [`load`](Self::load) uses after
    /// reading the file; it is public so integration tests and tooling can drive
    /// the parser directly without touching the filesystem.
    pub fn from_ghostty_config(text: &str) -> Self {
        let mut cfg = Self::default();
        // Resolve `theme = ...` first so the theme's colors/palette form a base
        // that the user's own keys then override, regardless of line order
        // (matching Ghostty, where an explicit `background` wins over the theme).
        if let Some(spec) = config_value(text, "theme") {
            cfg.apply_theme_spec(&spec);
        }
        cfg.parse(text);
        cfg
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
            eprintln!("giest: theme '{name}' not found (looked in <config-dir>/themes/)");
            return;
        };
        match std::fs::read_to_string(&path) {
            // Theme files are giest/Ghostty config bodies (palette/fg/bg/…). A
            // nested `theme` key inside a theme file is skipped by `apply`, so
            // this cannot recurse.
            Ok(body) => self.parse(&body),
            Err(e) => eprintln!("giest: could not read theme file {}: {e}", path.display()),
        }
    }

    /// Parse a Ghostty-format config body, applying each `key = value` line.
    /// Blank lines and `#` comment lines are skipped; malformed lines and
    /// unknown keys are logged and ignored (the rest of the file still applies).
    fn parse(&mut self, text: &str) {
        let defaults = Config::default();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                eprintln!("giest: ignoring malformed config line {}: {raw}", i + 1);
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
            None => eprintln!("giest: ignoring unsupported config key '{key}'"),
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
    ("font-family", |c, v, d| {
        c.font_family = opt_string(v, &d.font_family)
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
        if v.is_empty() {
            c.min_contrast = d.min_contrast;
        } else if let Ok(n) = v.parse::<f32>() {
            c.min_contrast = n.clamp(1.0, 21.0);
        }
    }),
    ("window-padding-x", |c, v, d| {
        c.padding_x = padding(v, d.padding_x, c.padding_x)
    }),
    ("window-padding-y", |c, v, d| {
        c.padding_y = padding(v, d.padding_y, c.padding_y)
    }),
    ("text-gamma", |c, v, d| {
        if v.is_empty() {
            c.text_gamma = d.text_gamma;
        } else if let Ok(g) = v.parse::<f32>() {
            c.text_gamma = g.clamp(0.5, 3.0);
        }
    }),
    ("palette", |c, v, d| {
        if v.is_empty() {
            c.palette = d.palette;
        } else if let Some((idx, col)) = parse_palette_entry(v) {
            c.palette[idx as usize] = col;
        } else {
            eprintln!("giest: ignoring bad palette entry: {v}");
        }
    }),
    ("scrollback-limit", |c, v, d| {
        if v.is_empty() {
            c.scrollback_limit = d.scrollback_limit;
        } else if let Ok(n) = v.parse() {
            c.scrollback_limit = n;
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
            "false" | "0" | "off" | "no" => false,
            "true" | "1" | "on" | "yes" | "clipboard" | "primary" => true,
            _ => c.copy_on_select,
        }
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
            "ignore" => MiddleClickAction::Ignore,
            _ => c.middle_click_action,
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
            c.keybinds
                .push((trigger.trim().to_string(), action.trim().to_string()));
        } else {
            eprintln!("giest: ignoring malformed keybind (expected 'trigger=action'): {v}");
        }
    }),
    ("bell-features", |c, v, d| {
        // giest implements the visual bell; an explicit off-value disables it,
        // any feature list (or unset) leaves it on.
        c.bell_visual = match v.to_ascii_lowercase().as_str() {
            "" => d.bell_visual,
            "no" | "false" | "off" | "none" | "0" => false,
            _ => true,
        };
    }),
];

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

/// Resolve a theme name to a file: an explicit existing path wins, otherwise
/// `<config-dir>/themes/<name>` (Ghostty's `themes/` convention). `None` if no
/// such file exists.
fn resolve_theme_path(name: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(name);
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
fn padding(value: &str, default: f32, current: f32) -> f32 {
    if value.is_empty() {
        default
    } else {
        value.parse::<f32>().map(|p| p.max(0.0)).unwrap_or(current)
    }
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
    }

    #[test]
    fn selection_and_copy_on_select_overrides() {
        let c = parsed(
            "selection-background = #385a9c\nselection-foreground = #ffffff\ncopy-on-select = true",
        );
        assert_eq!(c.selection_bg, Rgb::new(0x38, 0x5a, 0x9c));
        assert_eq!(c.selection_fg, Some(Rgb::new(0xff, 0xff, 0xff)));
        assert!(c.copy_on_select);
    }

    #[test]
    fn copy_on_select_accepts_ghostty_enum() {
        assert!(parsed("copy-on-select = clipboard").copy_on_select);
        assert!(parsed("copy-on-select = primary").copy_on_select);
        assert!(!parsed("copy-on-select = false").copy_on_select);
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
    fn font_family_and_feature_keys_parse() {
        let c = parsed(
            "font-family = Cascadia Code\n\
             font-family-bold = Cascadia Code SemiBold\n\
             font-family-italic = Cascadia Code Italic\n\
             font-feature = -calt\n\
             font-feature = ss01, cv01",
        );
        assert_eq!(c.font_family.as_deref(), Some("Cascadia Code"));
        assert_eq!(c.font_family_bold.as_deref(), Some("Cascadia Code SemiBold"));
        assert_eq!(c.font_family_italic.as_deref(), Some("Cascadia Code Italic"));
        assert_eq!(c.font_family_bold_italic, None);
        // Repeatable; each value is a comma-separated list.
        assert_eq!(c.font_features, vec!["-calt", "ss01", "cv01"]);
        // Comma-only split (not whitespace): a feature's own value syntax keeps
        // its spaces, so `liga off` is one feature the shaper resolves to liga=0.
        assert_eq!(parsed("font-feature = liga off").font_features, vec!["liga off"]);

        // Defaults and resets.
        assert_eq!(Config::default().font_family, None);
        assert!(Config::default().font_features.is_empty());
        assert_eq!(parsed("font-family = Foo\nfont-family =").font_family, None);
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
}
