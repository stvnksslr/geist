//! The chrome design system: one [`egui::Style`] derived from the terminal theme.
//!
//! Everything egui draws that is *not* the terminal grid — the tab strip, the
//! command palette, the search overlay, the modals, the split borders — used to
//! run on stock egui defaults. That had two consequences worth naming, because
//! both look like bugs rather than missing configuration:
//!
//! 1. egui defaults to [`egui::ThemePreference::System`], so on a light-themed
//!    Windows the tab strip and every dialog rendered **light over a dark
//!    terminal**. This module pins the theme instead of following the OS.
//! 2. Nothing in the chrome knew what `background` / `foreground` / `palette`
//!    the user configured, so the chrome was a fixed grey regardless of theme
//!    and five separate hardcoded accents (four different blues and greens)
//!    disagreed with each other and with the terminal.
//!
//! [`Chrome`] fixes both: every surface is a mix of the configured background
//! and foreground, and every accent comes from the configured ANSI palette.
//!
//! # The opacity rule
//!
//! **Every color this module produces is opaque.** `background-opacity` is
//! applied at exactly one site — the tab-strip panel fill in `app.rs`, which
//! reads [`egui::Visuals::panel_fill`] and re-emits it with an alpha. Baking
//! alpha in here would double-apply it (`Frame::side_top_panel` reads
//! `panel_fill` directly), and a translucent `window_fill` would leave the
//! palette and modals unreadable over live terminal text. `every_color_is_opaque`
//! pins this.

use eframe::egui;
use egui::Color32;

use crate::config::{Config, WindowTheme};
use crate::engine::Rgb;

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Corner radii, small → extra-large: buttons and tabs, pills, overlays, the
/// command palette.
pub const RADIUS_SM: u8 = 4;
pub const RADIUS_MD: u8 = 6;
pub const RADIUS_LG: u8 = 8;
pub const RADIUS_XL: u8 = 12;

/// Tab strip geometry. `CLOSE_COL` is reserved on every tab even when the `×` is
/// hidden, so a label never reflows when its close button fades in on hover.
pub const TAB_H: f32 = 24.0;
pub const TAB_MIN_W: f32 = 96.0;
pub const TAB_MAX_W: f32 = 220.0;
pub const TAB_GAP: f32 = 2.0;
pub const TAB_CLOSE_COL: f32 = 20.0;
/// Height of the tint band drawn along a colored tab's bottom edge. The tint is
/// a band rather than the tab's fill so a *tinted inactive* tab still reads as
/// inactive.
pub const TAB_TINT_H: f32 = 3.0;

// ---------------------------------------------------------------------------
// Color math
// ---------------------------------------------------------------------------

/// Linear blend between two sRGB colors. `t = 0` is `a`, `t = 1` is `b`.
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Rgb::new(ch(a.r, b.r), ch(a.g, b.g), ch(a.b, b.b))
}

/// Whether `bg` should be treated as a dark theme.
///
/// Rec. 601 luma against half intensity — cheap, and more than accurate enough
/// to answer "is this a dark theme?". This is the *same* predicate `blur.rs`
/// uses to pick DWM's immersive-dark-mode flag; it lives here and is called from
/// there so the chrome and the title bar can never disagree.
pub fn prefers_dark(bg: Rgb) -> bool {
    0.299 * bg.r as f32 + 0.587 * bg.g as f32 + 0.114 * (bg.b as f32) < 128.0
}

/// Nudge `c` toward the nearest extreme until it clears `min` contrast against
/// `bg`, giving up after a bounded number of steps.
///
/// Unlike the engine's `enforce_contrast` — which snaps a failing color straight
/// to pure white or black, correct for *text* legibility — this walks in 5%
/// steps so a low-contrast accent stays recognisably the user's color instead of
/// becoming white.
fn lift(c: Rgb, bg: Rgb, min: f32) -> Rgb {
    let target = if prefers_dark(bg) {
        Rgb::new(255, 255, 255)
    } else {
        Rgb::new(0, 0, 0)
    };
    let mut out = c;
    for _ in 0..20 {
        if crate::engine::contrast_ratio(out, bg) >= min {
            break;
        }
        out = mix(out, target, 0.05);
    }
    out
}

fn c32(c: Rgb) -> Color32 {
    Color32::from_rgb(c.r, c.g, c.b)
}

// ---------------------------------------------------------------------------
// Chrome
// ---------------------------------------------------------------------------

/// The resolved chrome palette. Built once from a [`Config`] and held by each
/// window, so painting code (split borders, the tab strip, overlay pills) can
/// reach for a named color instead of a literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chrome {
    /// Whether the chrome is being drawn dark-on-light or light-on-dark.
    pub dark: bool,
    /// The base surface the mixes are derived from — the terminal background
    /// under `window-theme = auto`.
    pub base: Rgb,
    /// The ink the mixes are derived from — the terminal foreground under
    /// `window-theme = auto`.
    pub ink: Rgb,

    /// Tab-strip fill. The one color `background-opacity` is applied to, at its
    /// single site in `app.rs`.
    pub panel_fill: Color32,
    /// Fill for floating surfaces: palette, search overlay, modals, the resize
    /// pill. Opaque so their contents stay readable over live terminal text.
    pub window_fill: Color32,
    /// Fill for sunken surfaces — text-edit interiors.
    pub extreme_bg: Color32,
    /// Hairlines: the split gutter, the tab-strip's bottom edge, palette row
    /// separators, overlay outlines.
    pub divider: Color32,

    pub text: Color32,
    pub weak_text: Color32,

    /// The primary accent: focused-split border, active-tab underline, drag
    /// caret, selection fill, primary buttons. ANSI blue, contrast-floored.
    pub accent: Color32,
    /// Text drawn *on* [`Self::accent`].
    pub on_accent: Color32,
    /// Zoomed-split border. ANSI green.
    pub accent_zoom: Color32,
    /// Visual-bell border. ANSI yellow.
    pub accent_warn: Color32,
    /// Destructive actions ("Close", "Allow"). ANSI red.
    pub danger: Color32,
    /// Text drawn *on* [`Self::danger`].
    pub on_danger: Color32,

    /// Widget fills, in ascending prominence. Also used directly by the tab
    /// strip for its inactive / hovered / active / pressed states.
    pub fill_weak: Color32,
    pub fill_hover: Color32,
    pub fill_active: Color32,
    /// Scrollbar knob — deliberately stronger than the widget fills, because it
    /// is drawn over terminal text rather than over a chrome surface.
    pub knob: Color32,
}

/// Resolve the chrome palette for a config.
pub fn chrome(cfg: &Config) -> Chrome {
    let auto_dark = prefers_dark(cfg.bg);
    let dark = match cfg.window_theme {
        WindowTheme::Auto => auto_dark,
        WindowTheme::Dark => true,
        WindowTheme::Light => false,
    };

    // Under `auto` — the default, and the case that matters — the chrome is
    // literally the terminal's own colors. When the user *forces* a mode that
    // disagrees with their background, deriving from that background would
    // produce e.g. a "light" theme made of dark greys; fall back to a neutral
    // pair in the requested mode instead.
    let (base, ink) = if dark == auto_dark {
        (cfg.bg, cfg.fg)
    } else if dark {
        (Rgb::new(0x1c, 0x1c, 0x1c), Rgb::new(0xe6, 0xe6, 0xe6))
    } else {
        (Rgb::new(0xf6, 0xf5, 0xf4), Rgb::new(0x1c, 0x1c, 0x1c))
    };

    let surface = |t: f32| c32(mix(base, ink, t));
    let panel = mix(base, ink, 0.05);

    // Accents are the user's own ANSI colors, floored to WCAG's 3.0 minimum for
    // non-text UI against the surface they sit on. `accent` doubles as a
    // selection fill with text on top, so it gets the 4.5 text minimum.
    let accent = lift(cfg.palette[4], panel, 4.5);
    let danger = lift(cfg.palette[1], panel, 3.0);
    let on = |c: Rgb| {
        if prefers_dark(c) {
            Color32::WHITE
        } else {
            Color32::BLACK
        }
    };

    Chrome {
        dark,
        base,
        ink,
        panel_fill: c32(panel),
        window_fill: surface(0.09),
        extreme_bg: surface(0.03),
        divider: surface(0.16),
        text: c32(ink),
        weak_text: c32(mix(ink, base, 0.45)),
        accent: c32(accent),
        on_accent: on(accent),
        accent_zoom: c32(lift(cfg.palette[2], panel, 3.0)),
        accent_warn: c32(lift(cfg.palette[3], panel, 3.0)),
        danger: c32(danger),
        on_danger: on(danger),
        fill_weak: surface(0.08),
        fill_hover: surface(0.14),
        fill_active: surface(0.20),
        knob: surface(0.55),
    }
}

// ---------------------------------------------------------------------------
// Style
// ---------------------------------------------------------------------------

/// Build the full [`egui::Style`] for a resolved palette.
pub fn style(ch: &Chrome) -> egui::Style {
    let mut s = egui::Style::default();

    // Type scale. The chrome deliberately stays on `Proportional`;
    // `install_ui_fallback_font` already appends the bundled Nerd Font to both
    // families, so `× ↑ ↓ ⏷` resolve without any font work here.
    use egui::{FontFamily::Monospace, FontFamily::Proportional, FontId, TextStyle};
    s.text_styles = [
        (TextStyle::Heading, FontId::new(17.0, Proportional)),
        (TextStyle::Body, FontId::new(13.0, Proportional)),
        (TextStyle::Button, FontId::new(13.0, Proportional)),
        (TextStyle::Small, FontId::new(11.0, Proportional)),
        (TextStyle::Monospace, FontId::new(12.0, Monospace)),
    ]
    .into();

    // Spacing. The two that matter most: egui's default vertical item spacing of
    // 3 is what makes stacked dialog content look cramped, and its button
    // padding of (4, 1) is what makes every button in the app read as a label
    // with a box around it.
    s.spacing.item_spacing = egui::vec2(8.0, 6.0);
    s.spacing.button_padding = egui::vec2(8.0, 4.0);
    s.spacing.interact_size = egui::vec2(40.0, 22.0);
    s.spacing.window_margin = egui::Margin::same(8);
    s.spacing.menu_margin = egui::Margin::same(8);

    let v = &mut s.visuals;
    v.dark_mode = ch.dark;
    v.panel_fill = ch.panel_fill;
    v.window_fill = ch.window_fill;
    v.extreme_bg_color = ch.extreme_bg;
    v.faint_bg_color = ch.fill_weak;
    v.code_bg_color = ch.extreme_bg;
    v.window_stroke = egui::Stroke::new(1.0_f32, ch.divider);
    v.window_corner_radius = egui::CornerRadius::same(RADIUS_XL);
    v.menu_corner_radius = egui::CornerRadius::same(RADIUS_LG);
    v.override_text_color = None;
    v.weak_text_color = Some(ch.weak_text);
    v.hyperlink_color = ch.accent;
    v.warn_fg_color = ch.accent_warn;
    v.error_fg_color = ch.danger;
    v.selection = egui::style::Selection {
        bg_fill: ch.accent,
        stroke: egui::Stroke::new(1.0_f32, ch.on_accent),
    };
    v.text_cursor.stroke.color = ch.accent;

    let radius = egui::CornerRadius::same(RADIUS_SM);
    let w = &mut v.widgets;
    for (slot, fill, ink) in [
        (&mut w.noninteractive, Color32::TRANSPARENT, ch.weak_text),
        (&mut w.inactive, ch.fill_weak, ch.text),
        (&mut w.hovered, ch.fill_hover, ch.text),
        (&mut w.active, ch.fill_active, ch.text),
        (&mut w.open, ch.fill_hover, ch.text),
    ] {
        slot.bg_fill = fill;
        slot.weak_bg_fill = fill;
        slot.bg_stroke = egui::Stroke::NONE;
        slot.fg_stroke = egui::Stroke::new(1.0_f32, ink);
        slot.corner_radius = radius;
        // egui grows a hovered widget by 1pt by default. On the small buttons
        // this app is full of (the tab `×`, the search cluster) that reads as a
        // visible jiggle rather than as feedback.
        slot.expansion = 0.0;
    }
    // The one bordered slot: `Frame::group` and separators take their stroke
    // from `noninteractive`.
    w.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, ch.divider);
    w.hovered.bg_stroke = egui::Stroke::new(1.0_f32, ch.divider);

    s
}

/// Install the chrome style on a context, pinning the theme.
///
/// Both theme slots get the *same* style, so the chrome keeps its appearance
/// even if something flips the preference out from under us.
///
/// Note that `Style` lives in egui's `Options`, which is **process-global**:
/// eframe keeps one `Context` for every viewport, so a config reload in one
/// window restyles them all. That is the same class of app-global as the font
/// size, and for the same reason.
pub fn install(ctx: &egui::Context, cfg: &Config) {
    let ch = chrome(cfg);
    let st = std::sync::Arc::new(style(&ch));
    ctx.set_theme(if ch.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
    ctx.set_style_of(egui::Theme::Dark, st.clone());
    ctx.set_style_of(egui::Theme::Light, st);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::contrast_ratio;

    fn dark_cfg() -> Config {
        Config::default()
    }

    /// "Redmean" color distance — a cheap approximation of perceptual difference
    /// that weights green heaviest and adapts the red/blue weights to where the
    /// pair sits in the red axis. Roughly `0..765`; ~40 is "clearly different".
    fn redmean(a: Rgb, b: Rgb) -> f32 {
        let rbar = (a.r as f32 + b.r as f32) * 0.5;
        let (dr, dg, db) = (
            a.r as f32 - b.r as f32,
            a.g as f32 - b.g as f32,
            a.b as f32 - b.b as f32,
        );
        ((2.0 + rbar / 256.0) * dr * dr + 4.0 * dg * dg + (2.0 + (255.0 - rbar) / 256.0) * db * db)
            .sqrt()
    }

    fn light_cfg() -> Config {
        let mut c = Config::default();
        c.bg = Rgb::new(0xfa, 0xfa, 0xfa);
        c.fg = Rgb::new(0x20, 0x22, 0x26);
        c
    }

    #[test]
    fn mix_interpolates_and_clamps() {
        let a = Rgb::new(0, 0, 0);
        let b = Rgb::new(255, 255, 255);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        assert_eq!(mix(a, b, 0.5), Rgb::new(128, 128, 128));
        // Out-of-range `t` saturates rather than wrapping past the endpoints.
        assert_eq!(mix(a, b, -1.0), a);
        assert_eq!(mix(a, b, 2.0), b);
    }

    #[test]
    fn prefers_dark_splits_at_half_intensity() {
        assert!(prefers_dark(Rgb::new(0x10, 0x12, 0x18)));
        assert!(!prefers_dark(Rgb::new(0xfa, 0xfa, 0xfa)));
    }

    /// The rule this whole module is built around: `background-opacity` is
    /// applied at exactly one site in `app.rs`, so nothing here may carry alpha.
    /// A translucent `panel_fill` would be composited twice, and a translucent
    /// `window_fill` would leave the palette and modals unreadable over live
    /// terminal text.
    #[test]
    fn every_color_is_opaque() {
        for cfg in [dark_cfg(), light_cfg()] {
            let st = style(&chrome(&cfg));
            let v = &st.visuals;
            let mut colors = vec![
                v.panel_fill,
                v.window_fill,
                v.extreme_bg_color,
                v.faint_bg_color,
                v.code_bg_color,
                v.window_stroke.color,
                v.hyperlink_color,
                v.warn_fg_color,
                v.error_fg_color,
                v.selection.bg_fill,
                v.selection.stroke.color,
                v.weak_text_color.unwrap(),
            ];
            for w in [
                &v.widgets.noninteractive,
                &v.widgets.inactive,
                &v.widgets.hovered,
                &v.widgets.active,
                &v.widgets.open,
            ] {
                // `noninteractive.bg_fill` is deliberately fully transparent —
                // "draw no background" — which is not the same failure as a
                // partially translucent color.
                colors.push(w.weak_bg_fill);
                colors.push(w.bg_stroke.color);
                colors.push(w.fg_stroke.color);
            }
            for c in colors {
                assert!(
                    c.a() == 255 || c == Color32::TRANSPARENT,
                    "chrome color {c:?} is partially translucent"
                );
            }
        }
    }

    #[test]
    fn text_meets_wcag_aa_on_every_surface() {
        for cfg in [dark_cfg(), light_cfg()] {
            let ch = chrome(&cfg);
            let ink = Rgb::new(ch.text.r(), ch.text.g(), ch.text.b());
            for (name, s) in [
                ("panel", ch.panel_fill),
                ("window", ch.window_fill),
                ("extreme", ch.extreme_bg),
                ("fill_active", ch.fill_active),
            ] {
                let bg = Rgb::new(s.r(), s.g(), s.b());
                let ratio = contrast_ratio(ink, bg);
                assert!(ratio >= 4.5, "text on {name} is only {ratio:.2}:1");
            }
        }
    }

    #[test]
    fn accents_clear_the_surface_and_each_other() {
        for cfg in [dark_cfg(), light_cfg()] {
            let ch = chrome(&cfg);
            let panel = Rgb::new(ch.panel_fill.r(), ch.panel_fill.g(), ch.panel_fill.b());
            let rgb = |c: Color32| Rgb::new(c.r(), c.g(), c.b());
            for (name, c, min) in [
                ("accent", ch.accent, 4.5),
                ("accent_zoom", ch.accent_zoom, 3.0),
                ("accent_warn", ch.accent_warn, 3.0),
                ("danger", ch.danger, 3.0),
            ] {
                let ratio = contrast_ratio(rgb(c), panel);
                assert!(ratio >= min, "{name} is only {ratio:.2}:1 on the panel");
            }
            // Each accent *means* something, and a pair that collapses to the
            // same color destroys that meaning. Checked with a redmean distance
            // rather than a channel sum: a plain sum scales with brightness, so
            // it calls any two dark colors "identical" — which is exactly the
            // false alarm it raised for the default palette's olive green and
            // sand yellow after both were darkened for a light background.
            let named = [
                ("accent", ch.accent),
                ("accent_zoom", ch.accent_zoom),
                ("accent_warn", ch.accent_warn),
                ("danger", ch.danger),
            ];
            for (i, (an, a)) in named.iter().enumerate() {
                for (bn, b) in &named[i + 1..] {
                    let d = redmean(rgb(*a), rgb(*b));
                    assert!(d >= 40.0, "{an} and {bn} are nearly identical ({d:.1})");
                }
            }
        }
    }

    #[test]
    fn text_on_accent_is_legible() {
        for cfg in [dark_cfg(), light_cfg()] {
            let ch = chrome(&cfg);
            let rgb = |c: Color32| Rgb::new(c.r(), c.g(), c.b());
            for (name, fill, ink) in [
                ("accent", ch.accent, ch.on_accent),
                ("danger", ch.danger, ch.on_danger),
            ] {
                let ratio = contrast_ratio(rgb(ink), rgb(fill));
                assert!(ratio >= 4.5, "text on {name} is only {ratio:.2}:1");
            }
        }
    }

    /// `auto` follows the terminal; the explicit modes must actually override it,
    /// and must not derive a "light" chrome out of a dark background.
    #[test]
    fn window_theme_overrides_the_background() {
        let mut cfg = dark_cfg();
        assert!(chrome(&cfg).dark, "a dark background should give dark chrome");

        cfg.window_theme = WindowTheme::Light;
        let ch = chrome(&cfg);
        assert!(!ch.dark);
        assert!(
            !prefers_dark(ch.base),
            "forced-light chrome must not be built from a dark base"
        );

        cfg.window_theme = WindowTheme::Dark;
        assert!(chrome(&cfg).dark);

        // A light background under `auto` gives light chrome derived from the
        // user's own colors, not from the neutral fallback.
        let light = light_cfg();
        let ch = chrome(&light);
        assert!(!ch.dark);
        assert_eq!(ch.base, light.bg);
        assert_eq!(ch.ink, light.fg);
    }

    #[test]
    fn lift_walks_toward_the_extreme_without_snapping_to_it() {
        let bg = Rgb::new(0x10, 0x12, 0x18);
        // A blue too dark to read on a dark panel gets brighter…
        let raw = Rgb::new(0x10, 0x20, 0x50);
        let out = lift(raw, bg, 4.5);
        assert!(crate::engine::contrast_ratio(out, bg) >= 4.5);
        // …but stays blue rather than becoming white.
        assert!(out.b > out.r, "lift should preserve the hue's dominant channel");
        assert!(out != Rgb::new(255, 255, 255));
        // An already-passing color is returned untouched.
        let fine = Rgb::new(0x9a, 0xb8, 0xff);
        assert_eq!(lift(fine, bg, 4.5), fine);
    }
}
