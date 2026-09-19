//! The giest icon artwork as code: parametric geometry plus a palette.
//!
//! Shared by `examples/icongen.rs` (which regenerates `assets/icon.{png,ico}`
//! with [`Palette::official`]) and by `icon.rs` at runtime, which redraws the
//! 256px master in another palette for `macos-icon` / `macos-icon-*`. One copy
//! of the geometry means a tinted icon can never drift from the official one.
//!
//! The artwork — dark rounded tile, lowercase-"g" monogram, blue block-cursor
//! accent — is defined as rounded-rect primitives, and **every icon size is
//! redrawn on its own integer pixel grid** from the per-size table in
//! [`layout`], never downscaled from the 256px master. Downscaling is what made
//! earlier revisions mushy at small sizes: strokes landed on half-pixels and
//! rendered as 50%-grey smears. With the tables, every straight edge sits on an
//! integer coordinate, so the only antialiased pixels are on corner arcs (and
//! at 16px there are no arcs at all).
//!
//! Two constraints the geometry must keep:
//!
//! - The letterform is the *union* of bowl-ring, stem and tail. The bowl's
//!   right-side corners are square and sit exactly on the stem's left edge, so
//!   the seam is buried inside the union and the silhouette cannot crease.
//! - Output is straight (unpremultiplied) alpha. With the official palette
//!   (one tile colour, no rim, opaque) partially-transparent pixels occur only
//!   on the tile silhouette and are pure tile colour — `src/icon.rs` compiles
//!   the PNG in and its tests assert both; `icongen` re-asserts them.

/// Axis-aligned rounded rectangle over half-open `[x0,x1) x [y0,y1)`, y down.
/// Radii are per-corner `[TL, TR, BR, BL]`; `0.0` keeps that corner square.
pub struct RRect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub r: [f32; 4],
}

impl RRect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        if x < self.x0 || x >= self.x1 || y < self.y0 || y >= self.y1 {
            return false;
        }
        for (i, &r) in self.r.iter().enumerate() {
            if r <= 0.0 {
                continue;
            }
            let (cx, cy) = match i {
                0 => (self.x0 + r, self.y0 + r),
                1 => (self.x1 - r, self.y0 + r),
                2 => (self.x1 - r, self.y1 - r),
                _ => (self.x0 + r, self.y1 - r),
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

    /// The same shape inset by `d` on every side (radii shrink with it).
    fn inset(&self, d: f32) -> RRect {
        RRect {
            x0: self.x0 + d,
            y0: self.y0 + d,
            x1: self.x1 - d,
            y1: self.y1 - d,
            r: self.r.map(|r| (r - d).max(0.0)),
        }
    }
}

fn rr(x0: f32, y0: f32, x1: f32, y1: f32, r: [f32; 4]) -> RRect {
    RRect { x0, y0, x1, y1, r }
}

fn rq(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> RRect {
    rr(x0, y0, x1, y1, [r; 4])
}

pub struct Layout {
    pub tile: RRect,
    pub bowl: RRect,
    pub counter: RRect,
    pub stem: RRect,
    pub tail: RRect,
    pub cursor: RRect,
}

/// The sizes [`layout`] has a hand-hinted table for.
pub const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

/// The per-size geometry tables — the artwork itself. `None` for a size with
/// no table (see [`SIZES`]).
///
/// Hinting rules used to derive each row (kept so future edits stay coherent):
/// scale the 256 master by `size/256`, round the *parts* (strokes, counter,
/// gaps), then rebuild totals from parts so rounding can't drift; minimums are
/// stroke >= 1, counter >= 2x2, gaps >= 1, cursor >= 2x2; the ensemble is
/// centered with any odd leftover margin going right/bottom; the cursor's top
/// row always equals the bowl's top row; radii shrink with size and drop to 0
/// before a curve could occupy less than ~2px.
pub fn layout(size: u32) -> Option<Layout> {
    Some(match size {
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
        // The master. All strokes 20; ensemble bbox [56,200)x[71,184) centers
        // at (128, 127.5) with 39|39 side margins inside the tile.
        256 => Layout {
            tile: rq(17.0, 17.0, 239.0, 239.0, 44.0),
            bowl: rr(56.0, 71.0, 144.0, 141.0, [16.0, 0.0, 0.0, 16.0]),
            counter: rq(76.0, 91.0, 124.0, 121.0, 8.0),
            stem: rr(144.0, 71.0, 164.0, 184.0, [0.0, 10.0, 10.0, 0.0]),
            tail: rr(79.0, 161.0, 164.0, 184.0, [10.0, 0.0, 10.0, 10.0]),
            cursor: rq(174.0, 71.0, 200.0, 97.0, 4.0),
        },
        _ => return None,
    })
}

/// The colours an icon is drawn in.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Palette {
    /// Tile gradient stops, **bottom first** (upstream's
    /// `macos-icon-screen-color` order). One stop is a flat tile.
    pub tile: Vec<[u8; 3]>,
    /// Tile opacity, 255 = opaque (`glass` is the one translucent preset).
    pub tile_alpha: u8,
    pub glyph: [u8; 3],
    pub cursor: [u8; 3],
    /// A rim around the tile (upstream's `macos-icon-frame`); `None` = no rim.
    pub rim: Option<[u8; 3]>,
}

impl Palette {
    /// Tile `#101824`, glyph `#E6E8EB`, cursor `#6C8BFF` — `assets/icon.*`.
    pub fn official() -> Self {
        Palette {
            tile: vec![[0x10, 0x18, 0x24]],
            tile_alpha: 255,
            glyph: [0xE6, 0xE8, 0xEB],
            cursor: [0x6C, 0x8B, 0xFF],
            rim: None,
        }
    }

    /// The tile colour at `t` in 0..=1 from bottom to top.
    fn tile_at(&self, t: f32) -> [u8; 3] {
        match self.tile.len() {
            0 => Palette::official().tile[0],
            1 => self.tile[0],
            n => {
                let pos = t.clamp(0.0, 1.0) * (n - 1) as f32;
                let i = (pos.floor() as usize).min(n - 2);
                let f = pos - i as f32;
                let (a, b) = (self.tile[i], self.tile[i + 1]);
                std::array::from_fn(|c| (a[c] as f32 + (b[c] as f32 - a[c] as f32) * f).round() as u8)
            }
        }
    }
}

/// Render one size to straight-alpha RGBA8 with `ss`x`ss` supersampling.
///
/// Per pixel, each sample is classified cursor -> letter -> rim -> tile ->
/// empty; alpha is the alpha-weighted coverage and RGB the weighted mean of
/// the participating fills. `icongen` uses `ss = 16` (256 coverage levels);
/// the runtime uses less, since a 256px redraw at 16 is ~17M samples.
///
/// Returns `None` for a size with no geometry table.
pub fn render(size: u32, pal: &Palette, ss: u32) -> Option<Vec<u8>> {
    let l = layout(size)?;
    let ss = ss.max(1);
    let n = size as usize;
    // One pixel of rim at 16px, scaling up to ~8px at 256.
    let rim_w = (size as f32 / 32.0).round().max(1.0);
    let inner = l.tile.inset(rim_w);
    let (ty0, ty1) = (l.tile.y0, l.tile.y1);
    let mut out = vec![0u8; n * n * 4];
    for py in 0..n {
        // The gradient is per *row* — the tile colour of this pixel's centre.
        let t = 1.0 - ((py as f32 + 0.5 - ty0) / (ty1 - ty0));
        let tile_rgb = pal.tile_at(t);
        for px in 0..n {
            // (weight, rgb) accumulators: weight is sample count x alpha.
            let (mut wsum, mut acc) = (0u64, [0u64; 3]);
            for sy in 0..ss {
                let y = py as f32 + (sy as f32 + 0.5) / ss as f32;
                for sx in 0..ss {
                    let x = px as f32 + (sx as f32 + 0.5) / ss as f32;
                    let (rgb, a) = if l.cursor.contains(x, y) {
                        (pal.cursor, 255)
                    } else if (l.bowl.contains(x, y) && !l.counter.contains(x, y))
                        || l.stem.contains(x, y)
                        || l.tail.contains(x, y)
                    {
                        (pal.glyph, 255)
                    } else if !l.tile.contains(x, y) {
                        continue;
                    } else if let Some(rim) = pal.rim.filter(|_| !inner.contains(x, y)) {
                        (rim, 255)
                    } else {
                        (tile_rgb, pal.tile_alpha)
                    };
                    let w = u64::from(a);
                    wsum += w;
                    for c in 0..3 {
                        acc[c] += u64::from(rgb[c]) * w;
                    }
                }
            }
            if wsum == 0 {
                continue;
            }
            let i = (py * n + px) * 4;
            // Rounds exactly as the original all-opaque renderer did
            // (`(sum + cov/2) / cov`, scaled by 255), so the official palette
            // stays byte-identical to the shipped master.
            let half = (wsum / 255 / 2) * 255;
            for c in 0..3 {
                out[i + c] = ((acc[c] + half) / wsum) as u8;
            }
            let total = u64::from(ss * ss) * 255;
            out[i + 3] = ((wsum * 255 + total / 2) / total) as u8;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_size_has_a_table() {
        for s in SIZES {
            assert!(layout(s).is_some(), "{s}");
        }
        assert!(layout(17).is_none());
    }

    /// The official palette at the icongen sampling rate reproduces the
    /// shipped master byte for byte — the runtime and the asset are one art.
    #[test]
    fn official_palette_reproduces_the_shipped_master() {
        let rgba = render(256, &Palette::official(), 16).unwrap();
        let shipped = crate::icon::decode_master().expect("master decodes");
        assert_eq!(rgba.len(), shipped.rgba.len());
        assert!(rgba == shipped.rgba, "iconart drifted from assets/icon.png");
    }

    #[test]
    fn a_palette_tints_tile_glyph_and_cursor() {
        let pal = Palette {
            tile: vec![[200, 0, 0]],
            tile_alpha: 255,
            glyph: [0, 200, 0],
            cursor: [0, 0, 200],
            rim: None,
        };
        let px = render(32, &pal, 4).unwrap();
        let at = |x: usize, y: usize| &px[(y * 32 + x) * 4..(y * 32 + x) * 4 + 4];
        assert_eq!(at(4, 4), [200, 0, 0, 255], "tile interior");
        assert_eq!(at(22, 10), [0, 0, 200, 255], "cursor block");
        assert_eq!(at(19, 15), [0, 200, 0, 255], "stem");
        assert_eq!(at(0, 0)[3], 0, "outside the tile");
    }

    #[test]
    fn gradient_runs_bottom_to_top_and_rim_frames_the_tile() {
        let pal = Palette {
            tile: vec![[0, 0, 0], [255, 255, 255]],
            tile_alpha: 255,
            glyph: [9, 9, 9],
            cursor: [9, 9, 9],
            rim: Some([1, 2, 3]),
        };
        let px = render(64, &pal, 2).unwrap();
        let at = |x: usize, y: usize| px[(y * 64 + x) * 4];
        // Inside the rim, well away from the glyph: top brighter than bottom.
        assert!(at(8, 10) > at(8, 54), "top {} bottom {}", at(8, 10), at(8, 54));
        let rim = &px[(32 * 64 + 4) * 4..(32 * 64 + 4) * 4 + 3];
        assert_eq!(rim, [1, 2, 3], "left rim at mid-height");
    }

    #[test]
    fn a_translucent_tile_keeps_straight_alpha() {
        let mut pal = Palette::official();
        pal.tile_alpha = 128;
        let px = render(32, &pal, 4).unwrap();
        let i = (4 * 32 + 4) * 4;
        assert_eq!(&px[i..i + 4], [0x10, 0x18, 0x24, 128]);
    }
}
