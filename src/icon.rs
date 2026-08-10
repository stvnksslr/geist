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
pub fn icon_data() -> Option<Arc<egui::IconData>> {
    ICON.get_or_init(|| decode(ICON_PNG).map(Arc::new)).clone()
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
