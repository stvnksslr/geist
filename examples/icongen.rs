//! Regenerates `assets/icon.png` and `assets/icon.ico` from parametric geometry.
//!
//! Run from the repo root:
//!
//! ```powershell
//! mise exec -- cargo run --example icongen
//! ```
//!
//! The artwork itself (geometry tables, palette, supersampled renderer) lives
//! in `src/iconart.rs`, shared with the runtime custom icon (`macos-icon`), so
//! a tinted icon can never drift from the official one; see that module for
//! the hinting rules. This binary renders every size in the official palette
//! at 16x16 supersampling and packs the PNG and ICO, re-asserting the contract
//! `src/icon.rs` relies on: straight alpha, and partially-transparent pixels
//! only on the tile silhouette, in pure tile colour.

use std::path::Path;

use geist::iconart::{Palette, SIZES, render};

/// Supersampling grid per axis: 16x16 = 256 coverage levels per pixel.
const SS: u32 = 16;

fn encode_png(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, size, size);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().expect("png header");
    w.write_image_data(rgba).expect("png data");
    w.finish().expect("png finish");
    out
}

/// A 32bpp BMP icon payload: BITMAPINFOHEADER with `biHeight = 2h`, BGRA rows
/// bottom-up, then the (all-zero, DWORD-padded) AND mask the format requires
/// even for alpha icons. Mirrors the byte layout `winresource` has been
/// embedding successfully.
fn bmp_entry(size: u32, rgba: &[u8]) -> Vec<u8> {
    let (w, h) = (size as usize, size as usize);
    let mask_row = w.div_ceil(32) * 4;
    let mut out = Vec::with_capacity(40 + w * h * 4 + h * mask_row);
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&((2 * h) as i32).to_le_bytes()); // biHeight (XOR+AND)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&[0u8; 24]); // compression through clr-important
    for y in (0..h).rev() {
        for x in 0..w {
            let i = (y * w + x) * 4;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    out.resize(out.len() + h * mask_row, 0);
    out
}

fn build_ico(entries: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut offset = 6 + entries.len() as u32 * 16;
    for (size, data) in entries {
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]); // w, h (0 = 256), colors, reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bitcount
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in entries {
        out.extend_from_slice(data);
    }
    out
}

fn main() -> std::io::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let pal = Palette::official();
    let tile = pal.tile[0];

    let mut entries = Vec::new();
    let mut master_png = Vec::new();
    for size in SIZES {
        let rgba = render(size, &pal, SS).expect("every listed size has a table");

        // The contract src/icon.rs and the renderer's own comment promise:
        // every partially-transparent pixel is tile-colored (fringe classes are
        // impossible), and — for the master the app compiles in — at least one
        // such pixel has a channel above its alpha (the straight-alpha test's
        // witness: B = 0x24 > A needs an arc pixel with coverage < 36/255, which
        // the master's r44 arcs guarantee; a small size's short arc might not).
        let mut witness = false;
        for p in rgba.as_chunks::<4>().0 {
            if p[3] > 0 && p[3] < 255 {
                assert_eq!(
                    [p[0], p[1], p[2]],
                    tile,
                    "non-tile color on the silhouette at size {size}"
                );
                witness |= p[..3].iter().any(|&c| c > p[3]);
            }
        }
        assert!(
            size != 256 || witness,
            "master lost its straight-alpha witness pixel"
        );

        if size == 256 {
            master_png = encode_png(size, &rgba);
            // The ICO's 256 entry is the same bytes as icon.png, as before.
            entries.push((size, master_png.clone()));
        } else {
            entries.push((size, bmp_entry(size, &rgba)));
        }
    }

    let png_path = root.join("assets/icon.png");
    let ico_path = root.join("assets/icon.ico");
    std::fs::write(&png_path, &master_png)?;
    let ico = build_ico(&entries);
    std::fs::write(&ico_path, &ico)?;
    println!("wrote {} ({} bytes)", png_path.display(), master_png.len());
    println!(
        "wrote {} ({} bytes, {} entries)",
        ico_path.display(),
        ico.len(),
        entries.len()
    );
    Ok(())
}
