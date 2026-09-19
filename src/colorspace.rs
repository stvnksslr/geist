//! `window-colorspace = display-p3`: interpret terminal colours as Display P3.
//!
//! Upstream's key says how *terminal colours* (config colours and direct-colour
//! SGR) are to be interpreted; macOS then colour-manages the window onto the
//! display. giest does the same interpretation but maps it onto the swapchain
//! it actually has, which is **sRGB**, so P3 colours outside the sRGB gamut are
//! clipped to its boundary. On a wide-gamut panel with Windows' automatic
//! colour management the result is the sRGB-gamut rendering of the P3 colour —
//! the saturation beyond sRGB is not reachable.
//!
//! **Why the swapchain is not wide-gamut.** The only DXGI route is an FP16
//! (`Rgba16Float`) swapchain, which DWM composites as scRGB (linear, Rec.709
//! primaries, values outside 0..1 carrying the wider gamut). wgpu-hal 29's DX12
//! backend does advertise `Rgba16Float` but never calls
//! `IDXGISwapChain3::SetColorSpace1`, and egui-wgpu picks the surface format
//! itself (preferring 8-bit unorm) with no override in `WgpuConfiguration`.
//! Taking that route means vendoring egui-wgpu (a third patched crate), and
//! egui paints in *gamma* space — on a linear float target every egui widget
//! would render washed out unless the whole UI went through a conversion pass.
//! And without HDR / Advanced Color enabled on the display, DWM clamps scRGB
//! back to sRGB anyway, i.e. exactly this module's output.
//!
//! The conversion is applied where terminal colours become GPU colours
//! (`GpuResources::color`) and to the one egui-painted terminal fill; images
//! and emoji are sRGB-tagged content and are left alone, as upstream does.

use crate::engine::Rgb;

/// Linear Display P3 → linear sRGB (both D65), row-major.
const P3_TO_SRGB: [[f32; 3]; 3] = [
    [1.224_940_2, -0.224_940_4, 0.0],
    [-0.042_056_955, 1.042_057, 0.0],
    [-0.019_637_555, -0.078_636_05, 1.098_273_6],
];

/// The sRGB (and Display P3 — they share it) transfer function, decoded.
pub fn decode(c: u8) -> f32 {
    let s = c as f32 / 255.0;
    if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
}

fn encode(l: f32) -> u8 {
    let l = l.clamp(0.0, 1.0);
    let s = if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0).round() as u8
}

/// A P3-encoded colour as *linear* sRGB, clipped to the sRGB gamut.
///
/// Memoized on the last colour: the renderer converts per instance, and runs
/// of cells overwhelmingly share one foreground and one background, so this
/// turns six `powf`s per cell into a compare.
pub fn p3_to_linear_srgb(c: Rgb) -> [f32; 3] {
    thread_local! {
        static LAST: std::cell::Cell<Option<(Rgb, [f32; 3])>> =
            const { std::cell::Cell::new(None) };
    }
    if let Some((k, v)) = LAST.with(|l| l.get())
        && k == c
    {
        return v;
    }
    let v = convert(c);
    LAST.with(|l| l.set(Some((c, v))));
    v
}

fn convert(c: Rgb) -> [f32; 3] {
    let v = [decode(c.r), decode(c.g), decode(c.b)];
    let m = |r: [f32; 3]| (r[0] * v[0] + r[1] * v[1] + r[2] * v[2]).clamp(0.0, 1.0);
    [m(P3_TO_SRGB[0]), m(P3_TO_SRGB[1]), m(P3_TO_SRGB[2])]
}

/// A P3-encoded colour as sRGB-encoded 8-bit, clipped to the sRGB gamut.
pub fn p3_to_srgb(c: Rgb) -> Rgb {
    let [r, g, b] = p3_to_linear_srgb(c);
    Rgb::new(encode(r), encode(g), encode(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutrals_are_unchanged() {
        // D65 white point shared: every grey maps to itself.
        for v in [0u8, 1, 0x40, 0x80, 0xC0, 0xFF] {
            assert_eq!(p3_to_srgb(Rgb::new(v, v, v)), Rgb::new(v, v, v));
        }
    }

    #[test]
    fn p3_primaries_clip_to_the_srgb_primaries() {
        // P3 red is more saturated than sRGB red: it clips onto it.
        assert_eq!(p3_to_srgb(Rgb::new(255, 0, 0)), Rgb::new(255, 0, 0));
        assert_eq!(p3_to_srgb(Rgb::new(0, 255, 0)), Rgb::new(0, 255, 0));
    }

    #[test]
    fn in_gamut_colours_desaturate_toward_srgb_coordinates() {
        // A mid P3 red has *more* red and *less* green in sRGB terms.
        let c = p3_to_srgb(Rgb::new(0xC0, 0x60, 0x60));
        assert!(c.r > 0xC0 && c.g < 0x60, "{c:?}");
        // Hand-computed: P3 #C06060 is sRGB ~#CE595B.
        assert!((c.r as i32 - 0xCE).abs() <= 2 && (c.g as i32 - 0x59).abs() <= 2, "{c:?}");
    }
}
