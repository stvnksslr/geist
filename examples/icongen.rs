//! Regenerates `assets/icon.png` and `assets/icon.ico` from parametric geometry.
//!
//! Run from the repo root:
//!
//! ```powershell
//! mise exec -- cargo run --example icongen
//! ```
//!
//! (Examples link the giest lib, so this needs the same Zig 0.15.2 toolchain
//! every build here already needs; `mise exec` provides it.)
//!
//! The artwork — dark rounded tile, lowercase-"g" monogram, blue block-cursor
//! accent — is defined as rounded-rect primitives, and **every icon size is
//! redrawn on its own integer pixel grid** from the per-size table in
//! [`layout`], never downscaled from the 256px master. Downscaling is what made
//! earlier revisions mushy at small sizes: strokes landed on half-pixels and
//! rendered as 50%-grey smears. With the tables, every straight edge sits on an
//! integer coordinate, so the only antialiased pixels are on corner arcs (and at
//! 16px there are no arcs at all — every in-tile pixel is one of the three pure
//! colors).
//!
//! Two constraints the geometry must keep:
//!
//! - The letterform is the *union* of bowl-ring, stem and tail. The bowl's
//!   right-side corners are square and sit exactly on the stem's left edge, so
//!   the seam is buried inside the union and the silhouette cannot crease —
//!   earlier revisions drew rounded shapes butted edge-to-edge, which left dark
//!   V-notches at the joins.
//! - Output is straight (unpremultiplied) alpha, and partially-transparent
//!   pixels occur only on the tile silhouette — `src/icon.rs` compiles the PNG
//!   in and its tests assert both. `main` re-asserts them after rendering so a
//!   geometry edit that breaks the contract fails here, not in `cargo test`.

use std::path::Path;

/// Tile fill `#101824` — matches the app's default background family.
const TILE: [u8; 3] = [0x10, 0x18, 0x24];
/// Glyph fill `#E6E8EB`.
const GLYPH: [u8; 3] = [0xE6, 0xE8, 0xEB];
/// Cursor fill `#6C8BFF`.
const CURSOR: [u8; 3] = [0x6C, 0x8B, 0xFF];

/// Supersampling grid per axis: 16×16 = 256 coverage levels per pixel.
///
/// Per-sample boolean classification makes union and subtraction exact at
/// flush seams and coincident arcs — exactly where analytic per-shape coverage
/// goes wrong (it cannot know two shapes' partial coverages overlap).
const SS: u32 = 16;

/// Axis-aligned rounded rectangle over half-open `[x0,x1) × [y0,y1)`, y down.
/// Radii are per-corner `[TL, TR, BR, BL]`; `0.0` keeps that corner square.
struct RRect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    r: [f32; 4],
}

impl RRect {
    fn contains(&self, x: f32, y: f32) -> bool {
        if x < self.x0 || x >= self.x1 || y < self.y0 || y >= self.y1 {
            return false;
        }
        for (i, &r) in self.r.iter().enumerate() {
            if r <= 0.0 {
                continue;
            }
            // Sample lies in this corner's r×r box iff it must pass the arc test.
            let (cx, cy) = match i {
                0 => (self.x0 + r, self.y0 + r), // TL
                1 => (self.x1 - r, self.y0 + r), // TR
                2 => (self.x1 - r, self.y1 - r), // BR
                _ => (self.x0 + r, self.y1 - r), // BL
            };
            let in_box = match i {
                0 => x < cx && y < cy,
                1 => x > cx && y < cy,
                2 => x > cx && y > cy,
                _ => x < cx && y > cy,
            };
            if in_box {
                let (dx, dy) = (x - cx, y - cy);
                if dx * dx + dy * dy > r * r {
                    return false;
                }
            }
        }
        true
    }
}

fn rr(x0: f32, y0: f32, x1: f32, y1: f32, r: [f32; 4]) -> RRect {
    RRect { x0, y0, x1, y1, r }
}

/// Uniform-radius shorthand.
fn rq(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> RRect {
    rr(x0, y0, x1, y1, [r; 4])
}

struct Layout {
    tile: RRect,
    bowl: RRect,    // outer rounded ring box; `counter` is subtracted from it
    counter: RRect, // the bowl's hole
    stem: RRect,    // right vertical, flush against the bowl's right stroke
    tail: RRect,    // descender bar, hooking left; bottom edge shared with stem
    cursor: RRect,  // blue block-cursor accent, top row == bowl cap row
}

/// The per-size geometry tables — the artwork itself.
///
/// Hinting rules used to derive each row (kept so future edits stay coherent):
/// scale the 256 master by `size/256`, round the *parts* (strokes, counter,
/// gaps), then rebuild totals from parts so rounding can't drift; minimums are
/// stroke ≥ 1, counter ≥ 2×2, gaps ≥ 1, cursor ≥ 2×2; the ensemble is centered
/// with any odd leftover margin going right/bottom; the cursor's top row always
/// equals the bowl's top row; radii shrink with size and drop to 0 before a
/// curve could occupy less than ~2px.
fn layout(size: u32) -> Layout {
    match size {
        // Strokes 1, zero radii anywhere inside the tile: every in-tile pixel
        // is a pure color. Counter widened to 4×2 so the bowl reads as a ring.
        16 => Layout {
            tile: rq(1.0, 1.0, 15.0, 15.0, 3.0),
            bowl: rq(3.0, 4.0, 9.0, 8.0, 0.0),
            counter: rq(4.0, 5.0, 8.0, 7.0, 0.0),
            stem: rq(8.0, 4.0, 10.0, 11.0, 0.0),
            tail: rq(4.0, 9.0, 10.0, 11.0, 0.0),
            cursor: rq(11.0, 4.0, 13.0, 6.0, 0.0),
        },
        20 => Layout {
            tile: rq(1.0, 1.0, 19.0, 19.0, 3.0),
            bowl: rr(4.0, 5.0, 10.0, 10.0, [1.0, 0.0, 0.0, 1.0]),
            counter: rq(5.0, 6.0, 9.0, 9.0, 0.0),
            stem: rr(10.0, 5.0, 12.0, 14.0, [0.0, 1.0, 0.0, 0.0]),
            tail: rq(6.0, 12.0, 12.0, 14.0, 0.0),
            cursor: rq(13.0, 5.0, 15.0, 7.0, 0.0),
        },
        24 => Layout {
            tile: rq(2.0, 2.0, 22.0, 22.0, 4.0),
            bowl: rr(5.0, 6.0, 13.0, 13.0, [1.0, 0.0, 0.0, 1.0]),
            counter: rq(7.0, 8.0, 11.0, 11.0, 0.0),
            stem: rr(13.0, 6.0, 15.0, 17.0, [0.0, 1.0, 1.0, 0.0]),
            tail: rr(7.0, 15.0, 15.0, 17.0, [1.0, 0.0, 1.0, 1.0]),
            cursor: rq(16.0, 6.0, 18.0, 8.0, 0.0),
        },
        // The ideal 2.5px stroke splits 2 (left/top/bottom) / 3 (right) — the
        // extra pixel hides inside the fused bowl+stem bar where no seam shows.
        32 => Layout {
            tile: rq(2.0, 2.0, 30.0, 30.0, 6.0),
            bowl: rr(7.0, 9.0, 18.0, 17.0, [2.0, 0.0, 0.0, 2.0]),
            counter: rq(9.0, 11.0, 15.0, 15.0, 1.0),
            stem: rr(18.0, 9.0, 20.0, 23.0, [0.0, 1.0, 1.0, 0.0]),
            tail: rr(10.0, 20.0, 20.0, 23.0, [1.0, 0.0, 1.0, 1.0]),
            cursor: rq(21.0, 9.0, 24.0, 12.0, 0.0),
        },
        40 => Layout {
            tile: rq(3.0, 3.0, 37.0, 37.0, 7.0),
            bowl: rr(8.0, 11.0, 22.0, 22.0, [2.0, 0.0, 0.0, 2.0]),
            counter: rq(11.0, 14.0, 19.0, 19.0, 1.0),
            stem: rr(22.0, 11.0, 25.0, 29.0, [0.0, 2.0, 2.0, 0.0]),
            tail: rr(12.0, 25.0, 25.0, 29.0, [2.0, 0.0, 2.0, 2.0]),
            cursor: rq(27.0, 11.0, 31.0, 15.0, 1.0),
        },
        48 => Layout {
            tile: rq(3.0, 3.0, 45.0, 45.0, 8.0),
            bowl: rr(10.0, 13.0, 27.0, 26.0, [3.0, 0.0, 0.0, 3.0]),
            counter: rq(14.0, 17.0, 23.0, 22.0, 1.0),
            stem: rr(27.0, 13.0, 31.0, 34.0, [0.0, 2.0, 2.0, 0.0]),
            tail: rr(14.0, 30.0, 31.0, 34.0, [2.0, 0.0, 2.0, 2.0]),
            cursor: rq(33.0, 13.0, 38.0, 18.0, 1.0),
        },
        64 => Layout {
            tile: rq(4.0, 4.0, 60.0, 60.0, 11.0),
            bowl: rr(14.0, 17.0, 36.0, 35.0, [4.0, 0.0, 0.0, 4.0]),
            counter: rq(19.0, 22.0, 31.0, 30.0, 2.0),
            stem: rr(36.0, 17.0, 41.0, 46.0, [0.0, 2.0, 2.0, 0.0]),
            tail: rr(20.0, 40.0, 41.0, 46.0, [2.0, 0.0, 2.0, 2.0]),
            cursor: rq(43.0, 17.0, 50.0, 24.0, 1.0),
        },
        // The master. All strokes 20; ensemble bbox [56,200)×[71,184) centers
        // at (128, 127.5) with 39|39 side margins inside the tile.
        256 => Layout {
            tile: rq(17.0, 17.0, 239.0, 239.0, 44.0),
            bowl: rr(56.0, 71.0, 144.0, 141.0, [16.0, 0.0, 0.0, 16.0]),
            counter: rq(76.0, 91.0, 124.0, 121.0, 8.0),
            stem: rr(144.0, 71.0, 164.0, 184.0, [0.0, 10.0, 10.0, 0.0]),
            tail: rr(79.0, 161.0, 164.0, 184.0, [10.0, 0.0, 10.0, 10.0]),
            cursor: rq(174.0, 71.0, 200.0, 97.0, 4.0),
        },
        other => panic!("no geometry table for size {other}"),
    }
}

/// Render one size to straight-alpha RGBA8.
///
/// Per pixel, each of the 256 samples is classified cursor → letter → tile →
/// empty; alpha is total coverage and RGB the coverage-weighted mean of the
/// participating fills. The art sits well inside the tile, so a pixel never
/// mixes "empty" with a non-tile fill — silhouette pixels keep pure tile RGB
/// with alpha = coverage (correct straight alpha), and interior edges blend
/// colors at full opacity. A constant-color fringe is unrepresentable here.
fn render(size: u32, l: &Layout) -> Vec<u8> {
    let n = size as usize;
    let total = SS * SS;
    let mut out = vec![0u8; n * n * 4];
    for py in 0..n {
        for px in 0..n {
            let (mut nc, mut ng, mut nt) = (0u32, 0u32, 0u32);
            for sy in 0..SS {
                let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                    if l.cursor.contains(x, y) {
                        nc += 1;
                    } else if (l.bowl.contains(x, y) && !l.counter.contains(x, y))
                        || l.stem.contains(x, y)
                        || l.tail.contains(x, y)
                    {
                        ng += 1;
                    } else if l.tile.contains(x, y) {
                        nt += 1;
                    }
                }
            }
            let cov = nc + ng + nt;
            if cov == 0 {
                continue; // stays (0,0,0,0)
            }
            let i = (py * n + px) * 4;
            for ch in 0..3 {
                let sum =
                    CURSOR[ch] as u32 * nc + GLYPH[ch] as u32 * ng + TILE[ch] as u32 * nt;
                out[i + ch] = ((sum + cov / 2) / cov) as u8;
            }
            out[i + 3] = ((cov * 255 + total / 2) / total) as u8;
        }
    }
    out
}

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
    let mask_row = (w + 31) / 32 * 4;
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
    let sizes: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

    let mut entries = Vec::new();
    let mut master_png = Vec::new();
    for &size in &sizes {
        let rgba = render(size, &layout(size));

        // The contract src/icon.rs and the renderer's own comment promise:
        // every partially-transparent pixel is tile-colored (fringe classes are
        // impossible), and — for the master the app compiles in — at least one
        // such pixel has a channel above its alpha (the straight-alpha test's
        // witness: B = 0x24 > A needs an arc pixel with coverage < 36/255, which
        // the master's r44 arcs guarantee; a small size's short arc might not).
        let mut witness = false;
        for p in rgba.chunks_exact(4) {
            if p[3] > 0 && p[3] < 255 {
                assert_eq!([p[0], p[1], p[2]], TILE, "non-tile color on the silhouette at size {size}");
                witness |= p[..3].iter().any(|&c| c > p[3]);
            }
        }
        assert!(size != 256 || witness, "master lost its straight-alpha witness pixel");

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
    println!("wrote {} ({} bytes, {} entries)", ico_path.display(), ico.len(), entries.len());
    Ok(())
}
