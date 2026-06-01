//! Terminal configuration: fonts and the color theme. Built-in defaults that an
//! optional `config.toml` (in `%APPDATA%\giest\` or `$GIEST_CONFIG`) overrides.

use std::path::PathBuf;

use serde::Deserialize;

use crate::engine::Rgb;

/// Subset of [`Config`] that may be set in `config.toml`. All fields optional;
/// unset ones keep the built-in default. Colors are `#rrggbb` hex strings.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    font_points: Option<f32>,
    foreground: Option<String>,
    background: Option<String>,
    /// Cursor color (`#rrggbb`); unset lets the running program / default decide.
    cursor_color: Option<String>,
    /// Blank space (in logical points) on the left/right of the grid.
    padding_x: Option<f32>,
    /// Blank space (in logical points) above/below the grid.
    padding_y: Option<f32>,
    /// Palette overrides, Ghostty-style: each entry is `"<index>=#rrggbb"`,
    /// e.g. `palette = ["0=#1d1f21", "1=#cc6666"]`.
    palette: Option<Vec<String>>,
    /// Maximum scrollback lines retained per pane.
    scrollback_limit: Option<usize>,
    /// Background color of selected cells (`#rrggbb`).
    selection_background: Option<String>,
    /// Foreground (text) color over a selection; unset keeps the cell's own fg.
    selection_foreground: Option<String>,
    /// Copy the selection to the clipboard as soon as it's made.
    copy_on_select: Option<bool>,
    /// Default shell to launch (name like `pwsh`/`cmd`/`wsl`, or a full path);
    /// unset auto-detects (PowerShell 7 preferred).
    shell: Option<String>,
}

/// User-facing configuration applied at startup.
#[derive(Clone, Debug)]
pub struct Config {
    /// Logical font size in points (scaled by the display DPI for the atlas).
    pub font_points: f32,
    /// Default foreground (text) color.
    pub fg: Rgb,
    /// Default background color.
    pub bg: Rgb,
    /// The 256-color palette (indices 0–15 = ANSI, 16–231 = color cube,
    /// 232–255 = grayscale ramp).
    pub palette: [Rgb; 256],
    /// Logical-point padding on the left/right of the grid (Ghostty
    /// `window-padding-x`).
    pub padding_x: f32,
    /// Logical-point padding above/below the grid (Ghostty `window-padding-y`).
    pub padding_y: f32,
    /// Cursor color; `None` defers to the running program / engine default.
    pub cursor: Option<Rgb>,
    /// Maximum scrollback lines retained per pane (Ghostty `scrollback-limit`).
    pub scrollback_limit: usize,
    /// Background color of selected cells.
    pub selection_bg: Rgb,
    /// Text color over a selection; `None` keeps each cell's own foreground.
    pub selection_fg: Option<Rgb>,
    /// Copy a selection to the clipboard as soon as it is made.
    pub copy_on_select: bool,
    /// Configured default shell (name or path); `None` auto-detects.
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
            cursor: None,
            scrollback_limit: 10_000,
            selection_bg: Rgb::new(0x38, 0x5a, 0x9c),
            selection_fg: None,
            copy_on_select: false,
            shell: None,
        }
    }
}

impl Config {
    /// Load configuration, applying `config.toml` overrides over the defaults.
    /// A missing file is fine; a malformed one logs and falls back to defaults.
    pub fn load() -> Self {
        let mut cfg = Self::default();
        let Some(path) = config_path() else {
            return cfg;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<FileConfig>(&text) {
                Ok(file) => cfg.apply(file),
                Err(e) => eprintln!("giest: ignoring bad config {}: {e}", path.display()),
            },
            Err(_) => {} // no file → defaults
        }
        cfg
    }

    fn apply(&mut self, file: FileConfig) {
        if let Some(fp) = file.font_points {
            if fp > 0.0 {
                self.font_points = fp;
            }
        }
        if let Some(c) = file.foreground.as_deref().and_then(parse_hex) {
            self.fg = c;
        }
        if let Some(c) = file.background.as_deref().and_then(parse_hex) {
            self.bg = c;
        }
        if let Some(p) = file.padding_x {
            self.padding_x = p.max(0.0);
        }
        if let Some(p) = file.padding_y {
            self.padding_y = p.max(0.0);
        }
        if let Some(c) = file.cursor_color.as_deref().and_then(parse_hex) {
            self.cursor = Some(c);
        }
        for entry in file.palette.iter().flatten() {
            if let Some((idx, color)) = parse_palette_entry(entry) {
                self.palette[idx as usize] = color;
            }
        }
        if let Some(n) = file.scrollback_limit {
            self.scrollback_limit = n;
        }
        if let Some(c) = file.selection_background.as_deref().and_then(parse_hex) {
            self.selection_bg = c;
        }
        if let Some(c) = file.selection_foreground.as_deref().and_then(parse_hex) {
            self.selection_fg = Some(c);
        }
        if let Some(b) = file.copy_on_select {
            self.copy_on_select = b;
        }
        if let Some(s) = file.shell {
            self.shell = Some(s);
        }
    }
}

/// Resolve the config file path: `$GIEST_CONFIG` if set, else
/// `%APPDATA%\giest\config.toml`.
fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GIEST_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("giest").join("config.toml"))
}

/// Parse a palette override entry of the form `"<index>=#rrggbb"` into its
/// 0–255 palette index and color. Returns `None` for malformed entries or an
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
    fn file_config_overrides_defaults() {
        let mut c = Config::default();
        let file: FileConfig = toml::from_str(
            "font_points = 18.0\nforeground = \"#ff8800\"\nbackground = \"#101010\"",
        )
        .unwrap();
        c.apply(file);
        assert_eq!(c.font_points, 18.0);
        assert_eq!(c.fg, Rgb::new(0xff, 0x88, 0x00));
        assert_eq!(c.bg, Rgb::new(0x10, 0x10, 0x10));
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
    fn palette_and_cursor_overrides_apply() {
        let mut c = Config::default();
        let file: FileConfig = toml::from_str(
            "cursor_color = \"#ff0000\"\npalette = [\"1=#abcdef\", \"232=#0a0a0a\"]",
        )
        .unwrap();
        c.apply(file);
        assert_eq!(c.cursor, Some(Rgb::new(0xff, 0, 0)));
        assert_eq!(c.palette[1], Rgb::new(0xab, 0xcd, 0xef));
        assert_eq!(c.palette[232], Rgb::new(0x0a, 0x0a, 0x0a));
        // Untouched indices keep their default.
        assert_eq!(c.palette[2], GIEST_ANSI16[2]);
    }

    #[test]
    fn scrollback_limit_overrides() {
        let mut c = Config::default();
        assert_eq!(c.scrollback_limit, 10_000);
        c.apply(toml::from_str("scrollback_limit = 50000").unwrap());
        assert_eq!(c.scrollback_limit, 50_000);
    }

    #[test]
    fn selection_and_copy_on_select_overrides() {
        let mut c = Config::default();
        assert!(c.selection_fg.is_none());
        assert!(!c.copy_on_select);
        c.apply(
            toml::from_str(
                "selection_background = \"#385a9c\"\nselection_foreground = \"#ffffff\"\ncopy_on_select = true",
            )
            .unwrap(),
        );
        assert_eq!(c.selection_bg, Rgb::new(0x38, 0x5a, 0x9c));
        assert_eq!(c.selection_fg, Some(Rgb::new(0xff, 0xff, 0xff)));
        assert!(c.copy_on_select);
    }

    #[test]
    fn empty_config_keeps_defaults() {
        let mut c = Config::default();
        let before = (c.font_points, c.fg, c.bg);
        c.apply(toml::from_str("").unwrap());
        assert_eq!((c.font_points, c.fg, c.bg), before);
    }
}
