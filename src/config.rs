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

use std::path::PathBuf;

use crate::engine::Rgb;

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_points: 16.0,
            fg: Rgb::new(0xc5, 0xc8, 0xc6),
            bg: Rgb::new(0x10, 0x12, 0x18),
            palette: xterm_palette(GIEST_ANSI16),
            padding_x: 20.0,
            padding_y: 2.0,
            text_gamma: 1.3,
            cursor: None,
            scrollback_limit: 10_000,
            selection_bg: Rgb::new(0x38, 0x5a, 0x9c),
            selection_fg: None,
            copy_on_select: false,
            right_click_action: RightClickAction::ContextMenu,
            middle_click_action: MiddleClickAction::PrimaryPaste,
            shell: None,
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
        cfg.parse(text);
        cfg
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

    /// Apply a single `key`/`value` pair. An empty `value` resets the key to its
    /// `defaults` value (Ghostty semantics); an unparseable value keeps the
    /// current setting.
    fn apply(&mut self, key: &str, value: &str, defaults: &Config) {
        match key {
            "font-size" => {
                if value.is_empty() {
                    self.font_points = defaults.font_points;
                } else if let Ok(v) = value.parse::<f32>() {
                    if v > 0.0 {
                        self.font_points = v;
                    }
                }
            }
            "foreground" => self.fg = color(value, defaults.fg, self.fg),
            "background" => self.bg = color(value, defaults.bg, self.bg),
            "cursor-color" => self.cursor = opt_color(value, defaults.cursor, self.cursor),
            "window-padding-x" => {
                self.padding_x = padding(value, defaults.padding_x, self.padding_x)
            }
            "window-padding-y" => {
                self.padding_y = padding(value, defaults.padding_y, self.padding_y)
            }
            "text-gamma" => {
                if value.is_empty() {
                    self.text_gamma = defaults.text_gamma;
                } else if let Ok(g) = value.parse::<f32>() {
                    self.text_gamma = g.clamp(0.5, 3.0);
                }
            }
            "palette" => {
                if value.is_empty() {
                    self.palette = defaults.palette;
                } else if let Some((idx, c)) = parse_palette_entry(value) {
                    self.palette[idx as usize] = c;
                } else {
                    eprintln!("giest: ignoring bad palette entry: {value}");
                }
            }
            "scrollback-limit" => {
                if value.is_empty() {
                    self.scrollback_limit = defaults.scrollback_limit;
                } else if let Ok(n) = value.parse() {
                    self.scrollback_limit = n;
                }
            }
            "selection-background" => {
                self.selection_bg = color(value, defaults.selection_bg, self.selection_bg)
            }
            "selection-foreground" => {
                self.selection_fg = opt_color(value, defaults.selection_fg, self.selection_fg)
            }
            "copy-on-select" => {
                self.copy_on_select = match value.to_ascii_lowercase().as_str() {
                    "" => defaults.copy_on_select,
                    "false" | "0" | "off" | "no" => false,
                    "true" | "1" | "on" | "yes" | "clipboard" | "primary" => true,
                    _ => self.copy_on_select,
                }
            }
            "right-click-action" => {
                self.right_click_action = match value.to_ascii_lowercase().as_str() {
                    "" => defaults.right_click_action,
                    "context-menu" => RightClickAction::ContextMenu,
                    "copy" => RightClickAction::Copy,
                    "paste" => RightClickAction::Paste,
                    "copy-or-paste" => RightClickAction::CopyOrPaste,
                    "ignore" => RightClickAction::Ignore,
                    _ => self.right_click_action,
                }
            }
            "middle-click-action" => {
                self.middle_click_action = match value.to_ascii_lowercase().as_str() {
                    "" => defaults.middle_click_action,
                    "primary-paste" => MiddleClickAction::PrimaryPaste,
                    "ignore" => MiddleClickAction::Ignore,
                    _ => self.middle_click_action,
                }
            }
            "command" => {
                self.shell = if value.is_empty() {
                    defaults.shell.clone()
                } else {
                    Some(value.to_string())
                }
            }
            _ => eprintln!("giest: ignoring unsupported config key '{key}'"),
        }
    }
}

/// Resolve the config file path: `$GIEST_CONFIG` if set, else
/// `%APPDATA%\giest\config` (Ghostty names its file `config`, no extension).
fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GIEST_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("giest").join("config"))
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

/// Resolve a required color field: empty resets to `default`, a valid hex sets
/// it, anything else keeps `current`.
fn color(value: &str, default: Rgb, current: Rgb) -> Rgb {
    if value.is_empty() {
        default
    } else {
        parse_hex(value).unwrap_or(current)
    }
}

/// Resolve an optional color field (cursor / selection foreground): empty resets
/// to `default`, a valid hex sets `Some`, anything else keeps `current`.
fn opt_color(value: &str, default: Option<Rgb>, current: Option<Rgb>) -> Option<Rgb> {
    if value.is_empty() {
        default
    } else {
        parse_hex(value).map(Some).unwrap_or(current)
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
    let color = parse_hex(color.trim())?;
    Some((idx, color))
}

/// Parse a `#rrggbb` (or `rrggbb`) hex color.
fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(Rgb::new(r, g, b))
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
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("nothex"), None);
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
}
