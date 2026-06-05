//! Ghostty config-format conformance.
//!
//! giest's config file *is* Ghostty's config format, so a key written for
//! Ghostty should parse and behave the same way in giest. Ghostty's own config
//! parser isn't vendored (only its VT engine is), so this suite can't do
//! differential testing against the real implementation; instead it pins
//! giest's parser to Ghostty's documented format and per-key semantics, feeding
//! it genuine Ghostty-syntax snippets through the public
//! [`Config::from_ghostty_config`] entry point.
//!
//! Reference: Ghostty's configuration format — `key = value` lines, kebab-case
//! keys, unquoted values, `#` comment lines (no inline comments), repeatable
//! keys (e.g. `palette`), and an empty value resets a key to its default.

use giest::config::Config;
use giest::engine::Rgb;

/// Parse a Ghostty-format body over the built-in defaults.
fn cfg(body: &str) -> Config {
    Config::from_ghostty_config(body)
}

fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb::new(r, g, b)
}

// ---------------------------------------------------------------------------
// Format-level conformance — the rules that distinguish Ghostty's format from
// TOML and that any Ghostty config relies on.
// ---------------------------------------------------------------------------

#[test]
fn colors_are_unquoted_hex() {
    // Ghostty: `background = #1d1f21` — the leading `#` is part of the color,
    // not a comment, and there are no surrounding quotes.
    let c = cfg("foreground = #c5c8c6\nbackground = #1d1f21");
    assert_eq!(c.fg, rgb(0xc5, 0xc8, 0xc6));
    assert_eq!(c.bg, rgb(0x1d, 0x1f, 0x21));
}

#[test]
fn hex_accepted_with_or_without_leading_hash() {
    assert_eq!(cfg("background = #1d1f21").bg, rgb(0x1d, 0x1f, 0x21));
    assert_eq!(cfg("background = 1d1f21").bg, rgb(0x1d, 0x1f, 0x21));
}

#[test]
fn comment_lines_start_with_hash() {
    // A `#` at the start of a (trimmed) line is a comment; everything else on a
    // comment line is ignored.
    let c = cfg("# this is a comment\n   # indented comment too\nfont-size = 21");
    assert_eq!(c.font_points, 21.0);
}

#[test]
fn no_inline_comments() {
    // Ghostty has no inline comments: a trailing `# ...` is part of the value,
    // so a color followed by a comment is not a valid color and is rejected
    // (the field keeps its default rather than silently truncating).
    let c = cfg("background = #1d1f21 # not a comment");
    assert_eq!(c.bg, Config::default().bg, "trailing text invalidates the value");
}

#[test]
fn blank_lines_are_ignored() {
    let c = cfg("\n\nfont-size = 18\n\n\nbackground = #000000\n\n");
    assert_eq!(c.font_points, 18.0);
    assert_eq!(c.bg, rgb(0, 0, 0));
}

#[test]
fn whitespace_around_key_and_value_is_trimmed() {
    let c = cfg("   font-size    =    19   \n\tbackground\t=\t#102030\t");
    assert_eq!(c.font_points, 19.0);
    assert_eq!(c.bg, rgb(0x10, 0x20, 0x30));
}

#[test]
fn malformed_line_is_skipped_rest_still_applies() {
    // A line with no `=` is malformed; it must not discard the rest of the file.
    let c = cfg("this line has no equals\nfont-size = 17");
    assert_eq!(c.font_points, 17.0);
}

#[test]
fn last_value_wins_for_scalar_keys() {
    // Ghostty: a repeated scalar key takes the last value.
    let c = cfg("font-size = 10\nfont-size = 14\nfont-size = 22");
    assert_eq!(c.font_points, 22.0);
}

#[test]
fn empty_value_resets_to_default() {
    // Ghostty: `key =` (empty value) resets that key to its default.
    let body = "font-size = 30\ncursor-color = #ff0000\nfont-size =\ncursor-color =";
    let c = cfg(body);
    assert_eq!(c.font_points, Config::default().font_points);
    assert_eq!(c.cursor, None);
}

#[test]
fn unsupported_keys_are_ignored_not_fatal() {
    // Pasting a real Ghostty config brings keys giest doesn't implement; they
    // must be ignored without discarding the keys giest does support.
    let body = "\
theme = catppuccin-mocha
font-family = JetBrains Mono
window-decoration = false
keybind = ctrl+a=select_all
font-size = 13
background = #11151a";
    let c = cfg(body);
    assert_eq!(c.font_points, 13.0);
    assert_eq!(c.bg, rgb(0x11, 0x15, 0x1a));
}

// ---------------------------------------------------------------------------
// Per-key conformance — every supported key, with Ghostty's spelling/semantics.
// ---------------------------------------------------------------------------

#[test]
fn font_size_accepts_integer_and_float() {
    assert_eq!(cfg("font-size = 13").font_points, 13.0);
    assert_eq!(cfg("font-size = 13.5").font_points, 13.5);
}

#[test]
fn foreground_and_background() {
    let c = cfg("foreground = #abcdef\nbackground = #123456");
    assert_eq!(c.fg, rgb(0xab, 0xcd, 0xef));
    assert_eq!(c.bg, rgb(0x12, 0x34, 0x56));
}

#[test]
fn cursor_color() {
    assert_eq!(cfg("cursor-color = #00ff00").cursor, Some(rgb(0, 0xff, 0)));
    // Unset → defer to the program/default.
    assert_eq!(cfg("").cursor, None);
}

#[test]
fn window_padding_x_and_y() {
    let c = cfg("window-padding-x = 12\nwindow-padding-y = 6");
    assert_eq!(c.padding_x, 12.0);
    assert_eq!(c.padding_y, 6.0);
}

#[test]
fn scrollback_limit() {
    // Note: giest's unit is lines (its VT engine takes a line count), whereas
    // Ghostty's scrollback-limit is bytes — the key transposes, the unit differs.
    assert_eq!(cfg("scrollback-limit = 50000").scrollback_limit, 50_000);
}

#[test]
fn selection_background_and_foreground() {
    let c = cfg("selection-background = #385a9c\nselection-foreground = #ffffff");
    assert_eq!(c.selection_bg, rgb(0x38, 0x5a, 0x9c));
    assert_eq!(c.selection_fg, Some(rgb(0xff, 0xff, 0xff)));
}

#[test]
fn copy_on_select_enum_values() {
    // Ghostty's copy-on-select is an enum: false / true / clipboard / primary.
    assert!(!cfg("copy-on-select = false").copy_on_select);
    assert!(cfg("copy-on-select = true").copy_on_select);
    assert!(cfg("copy-on-select = clipboard").copy_on_select);
    // Windows has no primary selection, so `primary` behaves like the others.
    assert!(cfg("copy-on-select = primary").copy_on_select);
}

#[test]
fn command_sets_the_shell() {
    assert_eq!(cfg("command = pwsh").shell.as_deref(), Some("pwsh"));
    assert_eq!(cfg("command = cmd").shell.as_deref(), Some("cmd"));
}

#[test]
fn palette_key_is_repeatable() {
    // Ghostty repeats `palette = N=#hex`, one entry per line; entries accumulate.
    let body = "palette = 0=#101218\npalette = 1=#cc6666\npalette = 8=#666a73";
    let c = cfg(body);
    assert_eq!(c.palette[0], rgb(0x10, 0x12, 0x18));
    assert_eq!(c.palette[1], rgb(0xcc, 0x66, 0x66));
    assert_eq!(c.palette[8], rgb(0x66, 0x6a, 0x73));
    // An index nobody set keeps the bundled theme.
    assert_eq!(c.palette[2], Config::default().palette[2]);
}

#[test]
fn palette_full_256_range_addressable() {
    let body = "palette = 16=#000000\npalette = 231=#ffffff\npalette = 255=#0a0a0a";
    let c = cfg(body);
    assert_eq!(c.palette[16], rgb(0, 0, 0));
    assert_eq!(c.palette[231], rgb(0xff, 0xff, 0xff));
    assert_eq!(c.palette[255], rgb(0x0a, 0x0a, 0x0a));
}

#[test]
fn text_gamma_is_giest_specific_and_clamped() {
    // giest extension (Ghostty has no equivalent); clamped to [0.5, 3.0].
    assert_eq!(cfg("text-gamma = 1.6").text_gamma, 1.6);
    assert_eq!(cfg("text-gamma = 10.0").text_gamma, 3.0);
    assert_eq!(cfg("text-gamma = 0.1").text_gamma, 0.5);
}

// ---------------------------------------------------------------------------
// End-to-end: a realistic Ghostty config dropped into giest verbatim.
// ---------------------------------------------------------------------------

#[test]
fn realistic_ghostty_config_applies_supported_subset() {
    // The kind of file a Ghostty user already has — supported and unsupported
    // keys interleaved, comments, blank lines, repeated palette entries.
    let body = "\
# My terminal config

font-family = JetBrains Mono     # ignored by giest (no font-family yet)
font-size = 14
theme = tokyonight              # ignored

foreground = #c0caf5
background = #1a1b26
cursor-color = #c0caf5

window-padding-x = 8
window-padding-y = 8
window-decoration = true        # ignored

selection-background = #283457
selection-foreground = #c0caf5
copy-on-select = clipboard

scrollback-limit = 100000

palette = 0=#15161e
palette = 1=#f7768e
palette = 2=#9ece6a

keybind = ctrl+shift+t=new_tab  # ignored
";

    let c = cfg(body);

    // Supported keys took effect.
    assert_eq!(c.font_points, 14.0);
    assert_eq!(c.fg, rgb(0xc0, 0xca, 0xf5));
    assert_eq!(c.bg, rgb(0x1a, 0x1b, 0x26));
    assert_eq!(c.cursor, Some(rgb(0xc0, 0xca, 0xf5)));
    assert_eq!(c.padding_x, 8.0);
    assert_eq!(c.padding_y, 8.0);
    assert_eq!(c.selection_bg, rgb(0x28, 0x34, 0x57));
    assert_eq!(c.selection_fg, Some(rgb(0xc0, 0xca, 0xf5)));
    assert!(c.copy_on_select);
    assert_eq!(c.scrollback_limit, 100_000);
    assert_eq!(c.palette[0], rgb(0x15, 0x16, 0x1e));
    assert_eq!(c.palette[1], rgb(0xf7, 0x76, 0x8e));
    assert_eq!(c.palette[2], rgb(0x9e, 0xce, 0x6a));

    // Unsupported keys left the corresponding giest behavior at its default.
    // (font-family/theme/keybind/window-decoration have no giest field.)
}
