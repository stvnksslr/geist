//! The application icon carried by a *running* window — taskbar button, Alt-Tab
//! and the title-bar corner.
//!
//! This is only half of the story. The `.ico` (all eight sizes) is embedded as a
//! Win32 `RT_GROUP_ICON` resource by `build.rs`, and that is what Explorer, the
//! Start menu and a pinned taskbar shortcut read straight off the file — a
//! process that never runs still needs an icon. The two are deliberately
//! separate mechanisms over the same artwork; neither substitutes for the other.
//!
//! We hand egui the 256×256 master and let Windows downscale for the live
//! window, rather than parsing the `.ico`: `IconData` holds exactly one bitmap,
//! so the hand-tuned small sizes in the `.ico` have nowhere to go here anyway.

use std::sync::{Arc, OnceLock};

use eframe::egui;

/// The 256×256 straight-alpha master. Compiled in so the icon cannot go missing
/// at runtime — `assets/` is not installed alongside the exe.
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

/// Decoded once per process and shared by every window.
///
/// **The `Arc` identity is load-bearing, not an optimization.**
/// `ViewportBuilder::patch` decides whether the icon changed with
/// `Arc::ptr_eq` (egui `viewport.rs`), and `App::child_builder` rebuilds its
/// builder on *every* pass — so handing out a freshly-allocated `Arc` would look
/// like a new icon every frame and push a `ViewportCommand::Icon` for every
/// child window, forever. Exactly the trap `BG_IMAGE_CACHE` exists to avoid,
/// for exactly the same reason.
///
/// `None` means the decode failed, which for a compiled-in asset means the file
/// was replaced with something invalid. Callers then leave the icon unset and
/// the window keeps egui's default, rather than taking the process down over
/// window dressing.
static ICON: OnceLock<Option<Arc<egui::IconData>>> = OnceLock::new();

/// Apply the app icon to a `ViewportBuilder`, if it decoded.
///
/// Both the root viewport (`main.rs`) and every child window
/// (`App::child_builder`) go through here, so they cannot drift apart.
pub fn apply(builder: egui::ViewportBuilder) -> egui::ViewportBuilder {
    match icon_data() {
        Some(icon) => builder.with_icon(icon),
        None => builder,
    }
}

/// The shared icon, decoding it on first call.
///
/// Once [`configure`] has run this is the *configured* icon (`macos-icon`),
/// still one `Arc` until the config changes it.
pub fn icon_data() -> Option<Arc<egui::IconData>> {
    if let Ok(cur) = CURRENT.lock()
        && let Some((_, icon)) = cur.as_ref()
    {
        return icon.clone();
    }
    ICON.get_or_init(|| decode(ICON_PNG).map(Arc::new)).clone()
}

/// The shipped master, decoded fresh (tests compare the art against it).
pub fn decode_master() -> Option<egui::IconData> {
    decode(ICON_PNG)
}

/// Decode the master to the straight-alpha RGBA8 `IconData` wants.
///
/// `IconData::rgba` is documented as "separate/unmultiplied alpha", which is
/// what the PNG already stores — so there is no premultiply step here, and
/// adding one would darken the antialiased edge against light chrome.
fn decode(bytes: &[u8]) -> Option<egui::IconData> {
    // png 0.18 requires `Read + Seek`; a slice is only `Read`.
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Normalize to 8-bit RGBA regardless of how the file was written, so a
    // re-exported icon (palette, grayscale, 16-bit) still loads.
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;

    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }

    let len = (info.width as usize).checked_mul(info.height as usize)?.checked_mul(4)?;
    if buf.len() < len {
        return None;
    }
    buf.truncate(len);

    Some(egui::IconData {
        rgba: buf,
        width: info.width,
        height: info.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_decodes_to_256_square_rgba() {
        let icon = decode(ICON_PNG).expect("bundled icon.png must decode");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }

    /// The `Arc` must be shared, or `ViewportBuilder::patch`'s `Arc::ptr_eq`
    /// sees a new icon every pass and re-sets it on every child window forever.
    #[test]
    fn icon_arc_is_shared_across_calls() {
        let (a, b) = (icon_data().expect("decodes"), icon_data().expect("decodes"));
        assert!(Arc::ptr_eq(&a, &b));
    }

    /// The artwork is straight-alpha: at least one partially transparent pixel
    /// has a colour channel above its alpha, which premultiplication makes
    /// impossible. Guards against a future re-export flattening it wrongly.
    #[test]
    fn master_is_straight_alpha() {
        let icon = decode(ICON_PNG).expect("decodes");
        let found = icon.rgba.chunks_exact(4).any(|p| {
            let a = p[3];
            a > 0 && a < 255 && p[..3].iter().any(|&c| c > a)
        });
        assert!(found, "icon.png looks premultiplied (or has no antialiased edge)");
    }
}

// --- Runtime custom icon (`macos-icon` and friends) -------------------------

use crate::config::{AppIcon, Config, IconFrame};
use crate::iconart::Palette;

/// What the configured icon is drawn from. Also the cache key: the drawn
/// `Arc` is replaced only when this changes, which is what keeps the
/// one-shared-`Arc` rule above true for a custom icon too.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    Official,
    Palette(Palette),
    File(String),
}

fn rgb(c: crate::engine::Rgb) -> [u8; 3] {
    [c.r, c.g, c.b]
}

/// `macos-icon-frame` as a rim colour.
pub fn frame_rgb(f: IconFrame) -> [u8; 3] {
    match f {
        IconFrame::Aluminum => [0xB9, 0xBE, 0xC4],
        IconFrame::Beige => [0xD8, 0xCF, 0xB8],
        IconFrame::Plastic => [0x2A, 0x2A, 0x2E],
        IconFrame::Chrome => [0xE4, 0xE8, 0xEE],
    }
}

/// Map the config onto an icon source.
///
/// Upstream's named variants are artist-drawn Ghostty icons; giest has one
/// artwork, so each becomes a palette in the variant's spirit. `custom-style`
/// maps its layers onto giest's: the *screen* gradient is the tile, the
/// *ghost* is the monogram, the *frame* is a rim. Upstream requires both
/// colours for `custom-style`; a missing one keeps the official colour here.
/// `custom` loads `macos-custom-icon` (PNG or JPEG; not ICNS) and falls back
/// to the official icon when that is unset or unreadable.
pub fn source_for(cfg: &Config) -> Source {
    let pal = |tile: &[[u8; 3]], glyph: [u8; 3], cursor: [u8; 3], rim: Option<[u8; 3]>| {
        Source::Palette(Palette { tile: tile.to_vec(), tile_alpha: 255, glyph, cursor, rim })
    };
    match cfg.app_icon {
        AppIcon::Official => Source::Official,
        AppIcon::Blueprint => pal(
            &[[0x12, 0x3A, 0x7A], [0x1D, 0x5B, 0xB5]],
            [0xFF, 0xFF, 0xFF],
            [0x9C, 0xC9, 0xFF],
            Some([0xD8, 0xE6, 0xFF]),
        ),
        AppIcon::Chalkboard => pal(
            &[[0x1F, 0x2A, 0x24], [0x2E, 0x3D, 0x34]],
            [0xF2, 0xF2, 0xEA],
            [0xF6, 0xE2, 0x7A],
            Some([0x7A, 0x5A, 0x3A]),
        ),
        AppIcon::Microchip => pal(
            &[[0x0B, 0x3B, 0x24], [0x13, 0x60, 0x3A]],
            [0xE8, 0xC7, 0x66],
            [0xF2, 0xD9, 0x8A],
            Some([0xB8, 0x96, 0x2E]),
        ),
        AppIcon::Glass => Source::Palette(Palette {
            tile: vec![[0x1A, 0x2A, 0x3A], [0x3A, 0x5A, 0x7A]],
            tile_alpha: 150,
            glyph: [0xFF, 0xFF, 0xFF],
            cursor: [0x8F, 0xD3, 0xFF],
            rim: Some([0xC8, 0xD8, 0xE8]),
        }),
        AppIcon::Holographic => pal(
            &[[0x6A, 0x3F, 0xD8], [0xE2, 0x4B, 0xB8], [0x38, 0xD6, 0xE8]],
            [0xFF, 0xFF, 0xFF],
            [0xFF, 0xFF, 0xFF],
            Some([0xE6, 0xE6, 0xF0]),
        ),
        AppIcon::Paper => pal(
            &[[0xED, 0xE6, 0xD6], [0xF7, 0xF3, 0xEA]],
            [0x2B, 0x2B, 0x2B],
            [0xC0, 0x39, 0x2B],
            Some([0xCF, 0xC6, 0xB0]),
        ),
        AppIcon::Retro => pal(
            &[[0x14, 0x14, 0x14]],
            [0xFF, 0xB0, 0x00],
            [0xFF, 0x7A, 0x00],
            Some(frame_rgb(IconFrame::Beige)),
        ),
        AppIcon::Xray => pal(
            &[[0x04, 0x12, 0x1F], [0x0B, 0x2A, 0x44]],
            [0x9F, 0xE8, 0xFF],
            [0xFF, 0xFF, 0xFF],
            None,
        ),
        AppIcon::CustomStyle => {
            let off = Palette::official();
            let tile = if cfg.icon_screen_color.is_empty() {
                off.tile.clone()
            } else {
                cfg.icon_screen_color.iter().map(|&c| rgb(c)).collect()
            };
            Source::Palette(Palette {
                tile,
                tile_alpha: 255,
                glyph: cfg.icon_ghost_color.map_or(off.glyph, rgb),
                cursor: off.cursor,
                rim: Some(frame_rgb(cfg.icon_frame)),
            })
        }
        AppIcon::Custom => match &cfg.custom_icon {
            Some(p) => Source::File(p.clone()),
            None => Source::Official,
        },
    }
}

/// Runtime supersampling: 8x8 = 64 coverage levels. The asset uses 16x16, but
/// that is ~17M samples for the 256px master — seconds in a debug build.
const RUNTIME_SS: u32 = 8;

fn draw(src: &Source) -> Option<Arc<egui::IconData>> {
    match src {
        Source::Official => ICON.get_or_init(|| decode(ICON_PNG).map(Arc::new)).clone(),
        Source::Palette(p) => {
            let rgba = crate::iconart::render(256, p, RUNTIME_SS)?;
            Some(Arc::new(egui::IconData { rgba, width: 256, height: 256 }))
        }
        Source::File(path) => {
            let img = match crate::bgimage::load(std::path::Path::new(path)) {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("giest: macos-custom-icon '{path}': {e}");
                    return draw(&Source::Official);
                }
            };
            let (w, h, rgba) = fit_256(img.width, img.height, img.rgba);
            Some(Arc::new(egui::IconData { rgba, width: w, height: h }))
        }
    }
}

/// Nearest-neighbour shrink so the longest side is at most 256 — the size the
/// shell asks for, and what keeps a photo from becoming a 50 MB icon.
fn fit_256(w: u32, h: u32, rgba: Vec<u8>) -> (u32, u32, Vec<u8>) {
    let longest = w.max(h);
    if longest <= 256 {
        return (w, h, rgba);
    }
    let (nw, nh) = ((w * 256 / longest).max(1), (h * 256 / longest).max(1));
    let mut out = Vec::with_capacity((nw * nh * 4) as usize);
    for y in 0..nh {
        let sy = (y * h / nh) as usize;
        for x in 0..nw {
            let sx = (x * w / nw) as usize;
            let i = (sy * w as usize + sx) * 4;
            out.extend_from_slice(&rgba[i..i + 4]);
        }
    }
    (nw, nh, out)
}

type Current = Option<(Source, Option<Arc<egui::IconData>>)>;
static CURRENT: std::sync::Mutex<Current> = std::sync::Mutex::new(None);

/// Select the icon for `cfg`. Returns the new icon when it **changed** — the
/// caller must then push it to the root viewport with `ViewportCommand::Icon`
/// (children pick it up through `apply`, whose `patch` sees the new `Arc`).
/// Unchanged config returns `None` and keeps the same `Arc`.
pub fn configure(cfg: &Config) -> Option<Arc<egui::IconData>> {
    let src = source_for(cfg);
    let mut cur = CURRENT.lock().ok()?;
    if cur.as_ref().is_some_and(|(s, _)| *s == src) {
        return None;
    }
    let was_set = cur.is_some();
    let icon = draw(&src);
    *cur = Some((src, icon.clone()));
    // The very first call is startup: the builders already carry this icon.
    if was_set { icon } else { None }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    fn cfg(text: &str) -> Config {
        Config::from_ghostty_config(text)
    }

    #[test]
    fn official_is_the_default_and_custom_without_a_path_falls_back() {
        assert_eq!(source_for(&cfg("")), Source::Official);
        assert_eq!(source_for(&cfg("macos-icon = custom")), Source::Official);
        assert_eq!(
            source_for(&cfg("macos-icon = custom\nmacos-custom-icon = C:\\x.png")),
            Source::File("C:\\x.png".into())
        );
    }

    #[test]
    fn custom_style_maps_screen_ghost_and_frame_onto_tile_glyph_and_rim() {
        let c = cfg("macos-icon = custom-style\n\
                     macos-icon-ghost-color = #ff0000\n\
                     macos-icon-screen-color = #000010, #0000f0\n\
                     macos-icon-frame = plastic");
        let Source::Palette(p) = source_for(&c) else { panic!("palette") };
        assert_eq!(p.glyph, [0xFF, 0, 0]);
        assert_eq!(p.tile, vec![[0, 0, 0x10], [0, 0, 0xF0]]);
        assert_eq!(p.rim, Some(frame_rgb(IconFrame::Plastic)));
    }

    #[test]
    fn every_preset_draws_a_256_icon_distinct_from_official() {
        let official = decode_master().unwrap().rgba;
        for name in [
            "blueprint", "chalkboard", "microchip", "glass", "holographic", "paper", "retro", "xray",
        ] {
            let src = source_for(&cfg(&format!("macos-icon = {name}")));
            let icon = draw(&src).expect(name);
            assert_eq!((icon.width, icon.height), (256, 256), "{name}");
            assert_ne!(icon.rgba, official, "{name} looks official");
        }
    }

    #[test]
    fn a_large_custom_image_is_shrunk_to_256_keeping_aspect() {
        let (w, h, px) = fit_256(1024, 512, vec![7; 1024 * 512 * 4]);
        assert_eq!((w, h), (256, 128));
        assert_eq!(px.len(), 256 * 128 * 4);
        let (w, h, _) = fit_256(64, 32, vec![0; 64 * 32 * 4]);
        assert_eq!((w, h), (64, 32));
    }
}
