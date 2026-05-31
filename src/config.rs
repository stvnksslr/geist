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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_points: 16.0,
            fg: Rgb::new(0xc5, 0xc8, 0xc6),
            bg: Rgb::new(0x10, 0x12, 0x18),
            palette: xterm_palette(GIEST_ANSI16),
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
    fn empty_config_keeps_defaults() {
        let mut c = Config::default();
        let before = (c.font_points, c.fg, c.bg);
        c.apply(toml::from_str("").unwrap());
        assert_eq!((c.font_points, c.fg, c.bg), before);
    }
}
