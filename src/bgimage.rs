//! `background-image`: decoding the file, and placing it behind the grid.
//!
//! Two halves, both deliberately free of GPU state so they can be tested
//! without a device. [`load`] turns a PNG or JPEG file into straight-alpha
//! RGBA8; [`dest_rect`] is Ghostty's fit/position math (`shaders.metal`'s
//! `bg_image_vertex`) lifted onto the CPU. Upstream recomputes that math per
//! vertex from a uniform, which is exactly the place it can't be unit-tested —
//! here it is a pure function with a table test, and the shader gets handed a
//! finished rect.
//!
//! The renderer half lives in [`crate::render`] (instance `mode` 4, plus the
//! texture at group-0 binding 4).

use std::io::Cursor;
use std::path::Path;

use anyhow::{Result, bail};

use crate::config::{BackgroundImageFit, BackgroundImagePosition};

/// A decoded background image: straight-alpha (**not** premultiplied) 8-bit
/// RGBA in sRGB, row-major, exactly `width * height * 4` bytes.
#[derive(Clone)]
pub struct BgImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Dimensions only — a derived `Debug` would print every pixel, which for a
/// wallpaper is tens of millions of numbers into whatever log or assertion
/// message asked for it.
impl std::fmt::Debug for BgImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BgImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish()
    }
}

/// Cap on the decoded pixel count (~256 MB as RGBA8, and the same again in
/// VRAM). Ghostty warns in its own docs that a background image is duplicated
/// per terminal and can balloon VRAM; geist uploads one texture per window, but
/// a mistyped path pointing at a gigapixel TIFF-sized PNG should still fail
/// cleanly rather than take the process down with it.
const MAX_PIXELS: u64 = 64 * 1024 * 1024;

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Decode `path` into straight-alpha RGBA8.
///
/// The format is sniffed from the file's magic bytes rather than its extension,
/// so a `.png` that is really a JPEG still loads (and, more usefully, an
/// extensionless path works at all). Ghostty supports exactly PNG and JPEG —
/// anything else is rejected with the same message it gives.
pub fn load(path: &Path) -> Result<BgImage> {
    let bytes = std::fs::read(path)?;
    if bytes.starts_with(PNG_MAGIC) {
        decode_png(&bytes)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        decode_jpeg(&bytes)
    } else {
        bail!("not a PNG or JPEG (other image formats are not supported)")
    }
}

/// Reject a decoded size that would be absurd *before* allocating for it.
fn check_size(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("image has a zero dimension");
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        bail!("image is too large ({width}x{height})");
    }
    Ok(())
}

fn decode_png(bytes: &[u8]) -> Result<BgImage> {
    // png 0.18 wants `Read + Seek`; a slice is only `Read`.
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    // Normalize to 8-bit RGBA: expand palettes/grayscale/low bit depths, add an
    // alpha channel, and drop 16-bit samples to 8. Same transformations the
    // kitty-graphics decoder uses (`engine::png_decode`).
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let info = reader.info();
    check_size(info.width, info.height)?;

    let size = reader
        .output_buffer_size()
        .ok_or_else(|| anyhow::anyhow!("PNG output size overflowed"))?;
    let mut buf = vec![0u8; size];
    let frame = reader.next_frame(&mut buf)?;
    if frame.color_type != png::ColorType::Rgba || frame.bit_depth != png::BitDepth::Eight {
        bail!("unsupported PNG pixel format");
    }
    let len = frame.width as usize * frame.height as usize * 4;
    if buf.len() < len {
        bail!("PNG decoded short");
    }
    buf.truncate(len);
    Ok(BgImage {
        width: frame.width,
        height: frame.height,
        rgba: buf,
    })
}

fn decode_jpeg(bytes: &[u8]) -> Result<BgImage> {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    decoder.read_info()?;
    let info = decoder
        .info()
        .ok_or_else(|| anyhow::anyhow!("JPEG has no frame header"))?;
    check_size(u32::from(info.width), u32::from(info.height))?;
    let pixels = decoder.decode()?;

    let (w, h) = (u32::from(info.width), u32::from(info.height));
    let count = w as usize * h as usize;
    let mut rgba = vec![0u8; count * 4];
    match info.pixel_format {
        // Baseline color JPEG.
        jpeg_decoder::PixelFormat::RGB24 => {
            if pixels.len() < count * 3 {
                bail!("JPEG decoded short");
            }
            for (px, src) in rgba
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(pixels.as_chunks::<3>().0)
            {
                px[..3].copy_from_slice(src);
                px[3] = 0xff;
            }
        }
        // Grayscale, 8- and 16-bit. 16-bit only appears for lossless JPEGs;
        // take the high byte, which is what an 8-bit display wants anyway.
        jpeg_decoder::PixelFormat::L8 => {
            if pixels.len() < count {
                bail!("JPEG decoded short");
            }
            for (px, &l) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(pixels.iter()) {
                px.copy_from_slice(&[l, l, l, 0xff]);
            }
        }
        jpeg_decoder::PixelFormat::L16 => {
            if pixels.len() < count * 2 {
                bail!("JPEG decoded short");
            }
            for (px, src) in rgba
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(pixels.as_chunks::<2>().0)
            {
                let l = src[1];
                px.copy_from_slice(&[l, l, l, 0xff]);
            }
        }
        // CMYK/YCCK JPEGs carry an Adobe inversion flag that jpeg-decoder does
        // not expose, so any conversion here would be a guess that silently
        // renders half of them inverted. Reject instead.
        jpeg_decoder::PixelFormat::CMYK32 => bail!("CMYK JPEGs are not supported"),
    }
    Ok(BgImage {
        width: w,
        height: h,
        rgba,
    })
}

/// Where the image lands inside an `area` of `w × h` device pixels, as
/// `[offset_x, offset_y, dest_w, dest_h]` **relative to the area's top-left**.
///
/// Straight from Ghostty's `bg_image_vertex`: `contain` and `cover` are the
/// min/max uniform scale that makes the image fit inside / cover the area,
/// `stretch` is the area itself, and `none` is the texture's own size. The
/// position anchors then pick, per axis, between `0`, `(area - dest) / 2` and
/// `area - dest`.
///
/// Under `cover` (and under `none` with an oversized image) the latter two go
/// **negative** — the image overhangs the area and gets cropped. That is the
/// point of `cover`, so nothing is clamped here; the quad's own bounds do the
/// cropping.
pub fn dest_rect(
    area: (f32, f32),
    tex: (f32, f32),
    fit: BackgroundImageFit,
    pos: BackgroundImagePosition,
) -> [f32; 4] {
    let (aw, ah) = (area.0.max(0.0), area.1.max(0.0));
    let (tw, th) = (tex.0.max(1.0), tex.1.max(1.0));
    let (dw, dh) = match fit {
        BackgroundImageFit::Contain => {
            let s = (aw / tw).min(ah / th);
            (tw * s, th * s)
        }
        BackgroundImageFit::Cover => {
            let s = (aw / tw).max(ah / th);
            (tw * s, th * s)
        }
        BackgroundImageFit::Stretch => (aw, ah),
        BackgroundImageFit::None => (tw, th),
    };

    use BackgroundImagePosition as P;
    let (mid_x, mid_y) = ((aw - dw) / 2.0, (ah - dh) / 2.0);
    let (end_x, end_y) = (aw - dw, ah - dh);
    let x = match pos {
        P::TopLeft | P::CenterLeft | P::BottomLeft => 0.0,
        P::TopCenter | P::Center | P::BottomCenter => mid_x,
        P::TopRight | P::CenterRight | P::BottomRight => end_x,
    };
    let y = match pos {
        P::TopLeft | P::TopCenter | P::TopRight => 0.0,
        P::CenterLeft | P::Center | P::CenterRight => mid_y,
        P::BottomLeft | P::BottomCenter | P::BottomRight => end_y,
    };
    [x, y, dw, dh]
}

/// Texture coordinates for the full-area quad, in the `[u0, v0, du, dv]` form
/// the renderer's vertex stage interpolates (`uv.xy + corner * uv.zw`).
///
/// Expressed in image-normalized units, so the fragment shader's repeat is a
/// plain `fract` and "outside the image" is just a `0..1` bounds test — no
/// pixel-space coordinates and no second sampler. Values outside `0..1` are
/// expected and meaningful (`contain` leaves margins; `cover` overhangs).
pub fn uv(area: (f32, f32), dest: [f32; 4]) -> [f32; 4] {
    let dw = dest[2].abs().max(1e-6);
    let dh = dest[3].abs().max(1e-6);
    [
        -dest[0] / dw,
        -dest[1] / dh,
        area.0.max(0.0) / dw,
        area.1.max(0.0) / dh,
    ]
}

#[cfg(test)]
mod tests {
    use super::{BgImage, dest_rect, load, uv};
    use crate::config::BackgroundImageFit as F;
    use crate::config::BackgroundImagePosition as P;

    fn close(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3)
    }

    #[test]
    fn fit_matches_ghostty_scale_rules() {
        // A 100x50 image into a 400x400 area.
        let area = (400.0, 400.0);
        let tex = (100.0, 50.0);
        // contain: min(4, 8) = 4  ->  400x200, centered vertically.
        assert!(close(
            dest_rect(area, tex, F::Contain, P::Center),
            [0.0, 100.0, 400.0, 200.0]
        ));
        // cover: max(4, 8) = 8  ->  800x400, overhanging horizontally. The
        // centered offset is negative on purpose.
        assert!(close(
            dest_rect(area, tex, F::Cover, P::Center),
            [-200.0, 0.0, 800.0, 400.0]
        ));
        // stretch ignores the aspect ratio entirely.
        assert!(close(
            dest_rect(area, tex, F::Stretch, P::Center),
            [0.0, 0.0, 400.0, 400.0]
        ));
        // none keeps the texture's own size.
        assert!(close(
            dest_rect(area, tex, F::None, P::Center),
            [150.0, 175.0, 100.0, 50.0]
        ));
    }

    #[test]
    fn position_anchors_pick_start_mid_end_per_axis() {
        let area = (400.0, 300.0);
        let tex = (100.0, 100.0);
        // `none` keeps a 100x100 dest, so the anchors are exact.
        let at = |p| {
            let r = dest_rect(area, tex, F::None, p);
            (r[0], r[1])
        };
        assert_eq!(at(P::TopLeft), (0.0, 0.0));
        assert_eq!(at(P::TopCenter), (150.0, 0.0));
        assert_eq!(at(P::TopRight), (300.0, 0.0));
        assert_eq!(at(P::CenterLeft), (0.0, 100.0));
        assert_eq!(at(P::Center), (150.0, 100.0));
        assert_eq!(at(P::CenterRight), (300.0, 100.0));
        assert_eq!(at(P::BottomLeft), (0.0, 200.0));
        assert_eq!(at(P::BottomCenter), (150.0, 200.0));
        assert_eq!(at(P::BottomRight), (300.0, 200.0));
    }

    #[test]
    fn cover_never_leaves_a_gap() {
        // Whatever the aspect mismatch, `cover` must reach every corner: the
        // dest rect has to contain the area on both axes.
        for tex in [(16.0, 9.0), (9.0, 16.0), (1.0, 1.0), (2000.0, 3.0)] {
            for area in [(800.0, 600.0), (300.0, 900.0), (500.0, 500.0)] {
                let [x, y, w, h] = dest_rect(area, tex, F::Cover, P::Center);
                assert!(x <= 1e-3 && y <= 1e-3, "{tex:?} {area:?}");
                assert!(
                    x + w >= area.0 - 1e-3 && y + h >= area.1 - 1e-3,
                    "{tex:?} {area:?}"
                );
            }
        }
    }

    #[test]
    fn contain_never_overflows() {
        for tex in [(16.0, 9.0), (9.0, 16.0), (2000.0, 3.0)] {
            for area in [(800.0, 600.0), (300.0, 900.0)] {
                let [x, y, w, h] = dest_rect(area, tex, F::Contain, P::Center);
                assert!(x >= -1e-3 && y >= -1e-3, "{tex:?} {area:?}");
                assert!(
                    x + w <= area.0 + 1e-3 && y + h <= area.1 + 1e-3,
                    "{tex:?} {area:?}"
                );
            }
        }
    }

    #[test]
    fn uv_maps_the_quad_corners_onto_the_dest_rect() {
        let area = (400.0, 400.0);
        // `stretch`: the quad and the image coincide, so uv spans exactly 0..1.
        let d = dest_rect(area, (100.0, 50.0), F::Stretch, P::Center);
        assert!(close(uv(area, d), [0.0, 0.0, 1.0, 1.0]));

        // Top-left `none` of a 100x50 image in a 400x400 area: the image
        // occupies the first quarter horizontally and eighth vertically, so the
        // quad spans 4x/8x the image.
        let d = dest_rect(area, (100.0, 50.0), F::None, P::TopLeft);
        assert!(close(uv(area, d), [0.0, 0.0, 4.0, 8.0]));

        // Centered: the image starts a quarter of the way in, so u0 is -1.5
        // (1.5 image-widths of margin to the left).
        let d = dest_rect(area, (100.0, 50.0), F::None, P::Center);
        assert!(close(uv(area, d), [-1.5, -3.5, 4.0, 8.0]));
    }

    #[test]
    fn uv_round_trips_the_image_corners() {
        // Wherever the image lands, sampling at its own corners must give 0 and
        // 1: u(dest.x) == 0 and u(dest.x + dest.w) == 1.
        let area = (640.0, 480.0);
        for fit in [F::Contain, F::Cover, F::None, F::Stretch] {
            for pos in [P::TopLeft, P::Center, P::BottomRight] {
                let d = dest_rect(area, (300.0, 200.0), fit, pos);
                let [u0, v0, du, dv] = uv(area, d);
                // Fraction of the way across the quad where the image starts.
                let fx = d[0] / area.0;
                let fy = d[1] / area.1;
                assert!((u0 + fx * du).abs() < 1e-3, "{fit:?} {pos:?}");
                assert!((v0 + fy * dv).abs() < 1e-3, "{fit:?} {pos:?}");
                let fx1 = (d[0] + d[2]) / area.0;
                let fy1 = (d[1] + d[3]) / area.1;
                assert!((u0 + fx1 * du - 1.0).abs() < 1e-3, "{fit:?} {pos:?}");
                assert!((v0 + fy1 * dv - 1.0).abs() < 1e-3, "{fit:?} {pos:?}");
            }
        }
    }

    #[test]
    fn degenerate_sizes_do_not_produce_nan() {
        for area in [(0.0, 0.0), (100.0, 0.0), (0.0, 100.0)] {
            for fit in [F::Contain, F::Cover, F::None, F::Stretch] {
                let d = dest_rect(area, (10.0, 10.0), fit, P::Center);
                assert!(d.iter().all(|v| v.is_finite()), "{area:?} {fit:?} -> {d:?}");
                assert!(uv(area, d).iter().all(|v| v.is_finite()));
            }
        }
    }

    /// A 2x1 PNG built by the `png` encoder, so the decode path is exercised
    /// end to end without shipping a binary fixture.
    fn png_fixture() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(std::io::Cursor::new(&mut out), 2, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[0xff, 0x00, 0x00, 0x00, 0x00, 0xff])
                .unwrap();
        }
        out
    }

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn png_loads_as_opaque_rgba() {
        let p = write_temp("geist-bgimage-test.png", &png_fixture());
        let img: BgImage = load(&p).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        // RGB source gains a fully opaque alpha channel.
        assert_eq!(img.rgba, vec![0xff, 0, 0, 0xff, 0, 0, 0xff, 0xff]);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn format_is_sniffed_from_magic_not_extension() {
        // A PNG named `.jpg` still loads…
        let p = write_temp("geist-bgimage-test-mislabelled.jpg", &png_fixture());
        assert!(load(&p).is_ok());
        let _ = std::fs::remove_file(p);

        // …and something that is neither is rejected, not misparsed.
        let p = write_temp("geist-bgimage-test-bogus.png", b"not an image at all");
        let err = load(&p).unwrap_err().to_string();
        assert!(err.contains("PNG or JPEG"), "{err}");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        assert!(load(std::path::Path::new("does-not-exist-geist.png")).is_err());
    }
}
