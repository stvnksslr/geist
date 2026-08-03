//! `custom-shader`: translating Shadertoy-format GLSL into WGSL.
//!
//! Ghostty accepts shaders written for [shadertoy.com](https://shadertoy.com) —
//! a fragment shader defining `mainImage(out vec4, in vec2)` against a fixed set
//! of `i*` uniforms — and so does giest, so a shader written for one runs on the
//! other unchanged. Ghostty compiles the GLSL to SPIR-V and then to Metal;
//! giest's renderer speaks WGSL, so this module goes GLSL → naga IR → WGSL.
//!
//! **Why WGSL text rather than handing wgpu a `naga::Module`.** wgpu can consume
//! naga IR directly, but only from the *exact* naga version it vendors — a
//! mismatch is a type error, and wgpu bumps naga on its own schedule. Emitting
//! WGSL source costs one extra parse at shader-load time (once, not per frame)
//! and makes the two versions independent.
//!
//! The [`PREFIX`] is the contract: it declares the uniform block and the
//! `main()` wrapper that calls `mainImage`, so the user's file contains nothing
//! but their shader — exactly as on Shadertoy.

use anyhow::{Context, Result, anyhow};

/// Declarations prepended to every user shader.
///
/// Kept deliberately close to Ghostty's `shadertoy_prefix.glsl` so a shader
/// written for Ghostty compiles here unchanged; the uniform *names, order and
/// types* are the compatibility surface and must not be reordered (the block is
/// `std140`, and [`crate::render`] writes a matching struct).
///
/// The uniforms **must** stay inside a `layout(binding = ...)` block: naga's
/// GLSL frontend rejects a bare `uniform vec3 x;` outright ("uniform/buffer
/// blocks require layout(binding=X)"), so Ghostty's block form is not a style
/// choice here, it is the only thing that parses. `layout(binding = N)` maps to
/// WGSL `@group(0) @binding(N)`.
///
/// Two deltas from Ghostty, both forced:
///
/// - No `layout(location = 0) in vec4 gl_FragCoord;`. Redeclaring it is legal
///   GLSL, but naga treats `gl_FragCoord` as a built-in and rejects the shadow.
/// - `vec4 iChannelTime`, not `float iChannelTime[4]`. WebGPU requires every
///   array stride in the uniform address space to be a multiple of 16, and
///   `array<f32, 4>` has a stride of 4 — the module is rejected by the *device*,
///   long after naga's own validator has passed it (`tests/shader_gpu.rs` is
///   what catches this). A `vec4` indexes identically in GLSL, so
///   `iChannelTime[0]` still compiles. `iChannelResolution` needs no such
///   change: `vec3` already aligns to 16.
pub const PREFIX: &str = r#"#version 450 core

layout(binding = 1, std140) uniform Globals {
    vec3  iResolution;
    float iTime;
    float iTimeDelta;
    float iFrameRate;
    int   iFrame;
    vec4  iChannelTime;
    vec3  iChannelResolution[4];
    vec4  iMouse;
    vec4  iDate;
    float iSampleRate;
    vec4  iCurrentCursor;
    vec4  iPreviousCursor;
    vec4  iCurrentCursorColor;
    vec4  iPreviousCursorColor;
    int   iCurrentCursorStyle;
    int   iPreviousCursorStyle;
    int   iCursorVisible;
    float iTimeCursorChange;
    float iTimeFocus;
    int   iFocus;
    vec3  iBackgroundColor;
    vec3  iForegroundColor;
    vec3  iCursorColor;
    vec3  iCursorText;
    vec3  iSelectionForegroundColor;
    vec3  iSelectionBackgroundColor;
};

#define CURSORSTYLE_BLOCK        0
#define CURSORSTYLE_BLOCK_HOLLOW 1
#define CURSORSTYLE_BAR          2
#define CURSORSTYLE_UNDERLINE    3
#define CURSORSTYLE_LOCK         4

layout(binding = 0) uniform texture2D _iChannel0Tex;
layout(binding = 2) uniform sampler _iChannel0Smp;
#define iChannel0 sampler2D(_iChannel0Tex, _iChannel0Smp)

#define texture2D texture

layout(location = 0) out vec4 _fragColor;
"#;

/// The entry point, appended **after** the user's shader.
///
/// Ghostty puts this in its prefix and forward-declares `mainImage`, which
/// works because glslang links. naga does not: its IR requires functions in
/// dependency order, and a `main` emitted before the `mainImage` it calls fails
/// validation with *"[0] of kind Function depends on [2] ... which has not been
/// processed yet"*. Appending it instead means the callee always exists first —
/// and, as a bonus, drops the forward declaration that would otherwise let a
/// misspelled `mainImage` compile into a call to nothing.
/// **`gl_FragCoord` is deliberately not flipped**, matching Ghostty.
///
/// Shadertoy's own convention is Y-up: `fragCoord.y == 0` is the bottom, and so
/// is `iChannel0`'s `uv.y == 0`. WGSL is Y-down for both. The tempting fix — flip
/// `gl_FragCoord` here — is *wrong*, and this was built and measured the wrong
/// way once before the reasoning was run properly: in a fullscreen post-process
/// the fragment writes to the pixel it is at, so flipping the coordinate flips
/// where the output lands relative to where the input was read. The two
/// conventions have to agree, and no combination of flips makes them agree at
/// Y-up without flipping the `v` of every `texture(iChannel0, …)` call — which
/// is the user's code and can't be intercepted.
///
/// So both are Y-down. Ghostty lands in exactly the same place: its prefix
/// doesn't flip either, and Metal's `[[position]]` is top-left like WGSL's. A
/// shader written for Ghostty therefore behaves identically here, which is the
/// thing that actually matters — even though a shader ported straight from
/// shadertoy.com may be mirrored vertically on *both*.
pub const SUFFIX: &str = r#"
void main() { mainImage(_fragColor, gl_FragCoord.xy); }
"#;

/// The uniform block [`PREFIX`] declares, laid out to match byte for byte.
///
/// The offsets are **naga's**, not hand-derived from the std140 rules — a
/// mistake here doesn't fail, it feeds shaders quietly wrong numbers, which is
/// the worst way for this to break. `uniform_layout_matches_naga` re-derives
/// them from the compiled module and fails if the two ever drift.
///
/// `vec3` members are stored as `[f32; 4]`: WGSL aligns a `vec3` to 16 bytes, so
/// the trailing word is padding either way and spelling it out keeps every
/// offset visible.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Globals {
    /// Viewport size in px; `.z` is the pixel aspect ratio (always 1.0).
    pub resolution: [f32; 4],
    pub time_delta: f32,
    pub frame_rate: f32,
    pub frame: i32,
    _pad0: u32,
    pub channel_time: [f32; 4],
    pub channel_resolution: [[f32; 4]; 4],
    pub mouse: [f32; 4],
    pub date: [f32; 4],
    pub sample_rate: f32,
    _pad1: [u32; 3],
    pub current_cursor: [f32; 4],
    pub previous_cursor: [f32; 4],
    pub current_cursor_color: [f32; 4],
    pub previous_cursor_color: [f32; 4],
    pub current_cursor_style: i32,
    pub previous_cursor_style: i32,
    pub cursor_visible: i32,
    pub time_cursor_change: f32,
    pub time_focus: f32,
    pub focus: i32,
    _pad2: [u32; 2],
    pub background_color: [f32; 4],
    pub foreground_color: [f32; 4],
    pub cursor_color: [f32; 4],
    pub cursor_text: [f32; 4],
    pub selection_foreground_color: [f32; 4],
    pub selection_background_color: [f32; 4],
}

impl Globals {
    /// `iResolution.xyz` and `iTime` share the first 16 bytes: `iResolution` is
    /// a `vec3` at offset 0 and `iTime` the `float` packed into its tail. They
    /// are one field here so that packing can't be got wrong.
    pub fn set_resolution(&mut self, width: f32, height: f32) {
        self.resolution[0] = width;
        self.resolution[1] = height;
        self.resolution[2] = 1.0;
    }

    pub fn set_time(&mut self, secs: f32) {
        self.resolution[3] = secs;
    }
}

/// Size of the uniform block, in bytes. A uniform buffer binding must be a
/// multiple of 16; 352 is.
pub const GLOBALS_SIZE: usize = 352;
const _: () = assert!(size_of::<Globals>() == GLOBALS_SIZE);
const _: () = assert!(GLOBALS_SIZE.is_multiple_of(16));

/// Translate one Shadertoy-format GLSL shader into WGSL.
///
/// The returned source declares its entry point as `main`. Errors carry the
/// GLSL compiler's own message, because that is the only thing that helps
/// someone fix their shader — and a shader failing to compile must stay a
/// *reported* error, never a silent fallback to a blank screen.
pub fn compile(source: &str) -> Result<String> {
    // naga does not link-check: the prefix *forward-declares* `mainImage`, so a
    // file that misspells it (or isn't a Shadertoy shader at all) compiles
    // cleanly into a call to nothing and renders a blank screen with no error.
    // Catch it here, where we can say what's actually wrong.
    if !source.contains("mainImage") {
        return Err(anyhow!(
            "no `mainImage` function — a custom shader must define \
             `void mainImage(out vec4 fragColor, in vec2 fragCoord)`"
        ));
    }
    let full = format!("{PREFIX}\n{source}\n{SUFFIX}");

    let mut frontend = naga::front::glsl::Frontend::default();
    let options = naga::front::glsl::Options {
        stage: naga::ShaderStage::Fragment,
        defines: Default::default(),
    };
    let module = frontend.parse(&options, &full).map_err(|e| {
        // The frontend reports every error it found; the first is almost always
        // the real one and the rest are cascade.
        anyhow!("{}", e.emit_to_string(&full))
    })?;

    // `emit_to_string` is the only rendering that carries the *location*; the
    // Display impl is a bare category name like "Entry point main is invalid",
    // which tells a user nothing about which line to look at.
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .map_err(|e| anyhow!("{}", e.emit_to_string(&full)))
    .context("shader failed validation")?;

    naga::back::wgsl::write_string(&module, &info, naga::back::wgsl::WriterFlags::empty())
        .context("could not emit WGSL")
}

#[cfg(test)]
mod tests {
    use super::compile;

    /// The smallest thing that is still a real Shadertoy shader.
    const TRIVIAL: &str = r#"
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    fragColor = vec4(uv, 0.5 + 0.5 * sin(iTime), 1.0);
}
"#;

    #[test]
    fn a_trivial_shadertoy_shader_compiles() {
        let wgsl = compile(TRIVIAL).expect("should compile");
        // It must produce a fragment entry point we can point a pipeline at.
        assert!(wgsl.contains("fn main"), "{wgsl}");
        assert!(wgsl.contains("@fragment"), "{wgsl}");
    }

    #[test]
    fn the_channel_texture_is_reachable() {
        // Sampling iChannel0 is the whole point of a terminal post-process
        // effect: without it a shader can't see the terminal it's filtering.
        let wgsl = compile(
            r#"
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    fragColor = texture(iChannel0, fragCoord / iResolution.xy);
}
"#,
        )
        .expect("should compile");
        assert!(wgsl.contains("textureSample"), "{wgsl}");
    }

    /// Ghostty's own CRT test shader, verbatim (its
    /// `renderer/shaders/test_shadertoy_crt.glsl`, loosely after Inigo Quilez).
    ///
    /// Inlined rather than read from the vendored Ghostty checkout because that
    /// tree is a build artifact — `cargo clean` deletes it. This is the real
    /// feasibility bar: a trivial shader proves the plumbing, a shader like this
    /// proves the *translation*, since it exercises swizzles, `mod`, `pow`,
    /// `smoothstep`, repeated texture sampling and early-out conditionals.
    const CRT: &str = r#"
vec2 curve(vec2 uv)
{
	uv = (uv - 0.5) * 2.0;
	uv *= 1.1;
	uv.x *= 1.0 + pow((abs(uv.y) / 5.0), 2.0);
	uv.y *= 1.0 + pow((abs(uv.x) / 4.0), 2.0);
	uv  = (uv / 2.0) + 0.5;
	uv =  uv *0.92 + 0.04;
	return uv;
}
void mainImage( out vec4 fragColor, in vec2 fragCoord )
{
    vec2 q = fragCoord.xy / iResolution.xy;
    vec2 uv = q;
    uv = curve( uv );
    vec3 oricol = texture( iChannel0, vec2(q.x,q.y) ).xyz;
    vec3 col;
	float x =  sin(0.3*iTime+uv.y*21.0)*sin(0.7*iTime+uv.y*29.0)*sin(0.3+0.33*iTime+uv.y*31.0)*0.0017;

    col.r = texture(iChannel0,vec2(x+uv.x+0.001,uv.y+0.001)).x+0.05;
    col.g = texture(iChannel0,vec2(x+uv.x+0.000,uv.y-0.002)).y+0.05;
    col.b = texture(iChannel0,vec2(x+uv.x-0.002,uv.y+0.000)).z+0.05;
    col.r += 0.08*texture(iChannel0,0.75*vec2(x+0.025, -0.027)+vec2(uv.x+0.001,uv.y+0.001)).x;
    col.g += 0.05*texture(iChannel0,0.75*vec2(x+-0.022, -0.02)+vec2(uv.x+0.000,uv.y-0.002)).y;
    col.b += 0.08*texture(iChannel0,0.75*vec2(x+-0.02, -0.018)+vec2(uv.x-0.002,uv.y+0.000)).z;

    col = clamp(col*0.6+0.4*col*col*1.0,0.0,1.0);

    float vig = (0.0 + 1.0*16.0*uv.x*uv.y*(1.0-uv.x)*(1.0-uv.y));
	col *= vec3(pow(vig,0.3));

    col *= vec3(0.95,1.05,0.95);
	col *= 2.8;

	float scans = clamp( 0.35+0.35*sin(3.5*iTime+uv.y*iResolution.y*1.5), 0.0, 1.0);

	float s = pow(scans,1.7);
	col = col*vec3( 0.4+0.7*s) ;

    col *= 1.0+0.01*sin(110.0*iTime);
	if (uv.x < 0.0 || uv.x > 1.0)
		col *= 0.0;
	if (uv.y < 0.0 || uv.y > 1.0)
		col *= 0.0;

	col*=1.0-0.65*vec3(clamp((mod(fragCoord.x, 2.0)-1.0)*2.0,0.0,1.0));

    float comp = smoothstep( 0.1, 0.9, sin(iTime) );

    fragColor = vec4(col,1.0);
}
"#;

    #[test]
    fn a_real_shadertoy_shader_compiles() {
        let wgsl =
            compile(CRT).unwrap_or_else(|e| panic!("Ghostty's CRT shader must compile:\n{e:#}"));
        assert!(wgsl.contains("@fragment"), "{wgsl}");
        // It samples the terminal several times over; the translation must keep
        // every one of them.
        assert!(wgsl.matches("textureSample").count() >= 6, "{wgsl}");
    }

    #[test]
    fn shaders_using_ghosttys_extra_uniforms_compile() {
        // The cursor and colour uniforms are giest/Ghostty extensions to the
        // Shadertoy set, so a shader written against Ghostty leans on them.
        let wgsl = compile(
            r#"
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    vec4 cur = iCurrentCursor;
    float near = step(distance(fragCoord, cur.xy), 20.0) * float(iCursorVisible);
    vec3 tint = mix(iBackgroundColor, iCursorColor, near);
    if (iCurrentCursorStyle == CURSORSTYLE_BAR) { tint = iForegroundColor; }
    fragColor = vec4(texture(iChannel0, uv).rgb * tint, 1.0);
}
"#,
        )
        .expect("should compile");
        assert!(wgsl.contains("@fragment"), "{wgsl}");
    }

    /// Re-derive the uniform layout from a compiled module and check it against
    /// [`super::Globals`], field by field.
    ///
    /// A drift here is silent — shaders get wrong numbers, not an error — so
    /// this asserts every offset rather than just the total size. It is also
    /// what makes the `PREFIX` block safe to edit: reorder a uniform and this
    /// fails immediately instead of at a user's screen.
    #[test]
    fn uniform_layout_matches_naga() {
        use std::mem::offset_of;

        let full = format!(
            "{}\nvoid mainImage(out vec4 c, in vec2 p) {{ c = vec4(iResolution, iTime); }}\n{}",
            super::PREFIX,
            super::SUFFIX
        );
        let module = naga::front::glsl::Frontend::default()
            .parse(
                &naga::front::glsl::Options {
                    stage: naga::ShaderStage::Fragment,
                    defines: Default::default(),
                },
                &full,
            )
            .expect("prefix must parse");

        let (members, span) = module
            .types
            .iter()
            .find_map(|(_, ty)| match (&ty.name, &ty.inner) {
                (Some(n), naga::TypeInner::Struct { members, span }) if n == "Globals" => {
                    Some((members.clone(), *span))
                }
                _ => None,
            })
            .expect("the prefix must declare a `Globals` block");

        assert_eq!(
            span as usize,
            super::GLOBALS_SIZE,
            "GLOBALS_SIZE is stale: naga now lays the block out as {span} bytes"
        );

        // GLSL name -> byte offset of the matching Rust field. `iResolution` and
        // `iTime` deliberately share one field (see `Globals::set_time`).
        let expected: &[(&str, usize)] = &[
            ("iResolution", offset_of!(super::Globals, resolution)),
            ("iTime", offset_of!(super::Globals, resolution) + 12),
            ("iTimeDelta", offset_of!(super::Globals, time_delta)),
            ("iFrameRate", offset_of!(super::Globals, frame_rate)),
            ("iFrame", offset_of!(super::Globals, frame)),
            ("iChannelTime", offset_of!(super::Globals, channel_time)),
            ("iChannelResolution", offset_of!(super::Globals, channel_resolution)),
            ("iMouse", offset_of!(super::Globals, mouse)),
            ("iDate", offset_of!(super::Globals, date)),
            ("iSampleRate", offset_of!(super::Globals, sample_rate)),
            ("iCurrentCursor", offset_of!(super::Globals, current_cursor)),
            ("iPreviousCursor", offset_of!(super::Globals, previous_cursor)),
            ("iCurrentCursorColor", offset_of!(super::Globals, current_cursor_color)),
            ("iPreviousCursorColor", offset_of!(super::Globals, previous_cursor_color)),
            ("iCurrentCursorStyle", offset_of!(super::Globals, current_cursor_style)),
            ("iPreviousCursorStyle", offset_of!(super::Globals, previous_cursor_style)),
            ("iCursorVisible", offset_of!(super::Globals, cursor_visible)),
            ("iTimeCursorChange", offset_of!(super::Globals, time_cursor_change)),
            ("iTimeFocus", offset_of!(super::Globals, time_focus)),
            ("iFocus", offset_of!(super::Globals, focus)),
            ("iBackgroundColor", offset_of!(super::Globals, background_color)),
            ("iForegroundColor", offset_of!(super::Globals, foreground_color)),
            ("iCursorColor", offset_of!(super::Globals, cursor_color)),
            ("iCursorText", offset_of!(super::Globals, cursor_text)),
            ("iSelectionForegroundColor", offset_of!(super::Globals, selection_foreground_color)),
            ("iSelectionBackgroundColor", offset_of!(super::Globals, selection_background_color)),
        ];

        assert_eq!(
            members.len(),
            expected.len(),
            "the uniform block gained or lost a member; update `Globals` to match"
        );
        for (m, (name, want)) in members.iter().zip(expected) {
            assert_eq!(
                m.name.as_deref(),
                Some(*name),
                "uniform order changed: naga has {:?} where {name} was expected",
                m.name
            );
            assert_eq!(
                m.offset as usize, *want,
                "`{name}` is at byte {} in the shader but {want} in `Globals`",
                m.offset
            );
        }
    }

    #[test]
    fn a_syntax_error_is_reported_not_swallowed() {
        let err = compile("void mainImage(out vec4 c, in vec2 p) { this is not glsl }")
            .expect_err("should fail");
        // The message has to be the compiler's own, or it can't be acted on.
        assert!(!err.to_string().trim().is_empty());
    }

    #[test]
    fn a_missing_main_image_is_an_error() {
        // The prefix forward-declares `mainImage` and calls it, so a file that
        // never defines one must fail rather than link to nothing.
        assert!(compile("float unused = 1.0;").is_err());
    }
}

#[cfg(test)]
mod dump {
    /// Not a test of behaviour — a way to *see* the WGSL naga emits, which is
    /// the ground truth for the uniform struct the renderer must write.
    /// `cargo test --lib shader::dump -- --ignored --nocapture`
    /// Print the byte offset naga assigns each uniform. This is the ground
    /// truth the renderer's `Globals` struct must match, and hand-computing it
    /// from the std140/WGSL rules is exactly the kind of arithmetic that yields
    /// silently garbled uniforms.
    /// `cargo test --lib shader::dump::print_uniform_offsets -- --ignored --nocapture`
    #[test]
    #[ignore = "diagnostic; prints the uniform block layout"]
    fn print_uniform_offsets() {
        let full = format!(
            "{}\n{}\n{}",
            super::PREFIX,
            "void mainImage(out vec4 c, in vec2 p) { c = vec4(iResolution, iTime); }",
            super::SUFFIX
        );
        let module = naga::front::glsl::Frontend::default()
            .parse(
                &naga::front::glsl::Options {
                    stage: naga::ShaderStage::Fragment,
                    defines: Default::default(),
                },
                &full,
            )
            .expect("parse");
        for (_, ty) in module.types.iter() {
            if let naga::TypeInner::Struct { members, span } = &ty.inner {
                if ty.name.as_deref() != Some("Globals") {
                    continue;
                }
                println!("struct Globals: {span} bytes");
                for m in members {
                    println!("  {:>4}  {}", m.offset, m.name.as_deref().unwrap_or("?"));
                }
            }
        }
    }

    #[test]
    #[ignore = "diagnostic; prints the generated WGSL"]
    fn print_generated_wgsl() {
        let wgsl = super::compile(
            "void mainImage(out vec4 c, in vec2 p) { c = texture(iChannel0, p / iResolution.xy); }",
        )
        .unwrap();
        println!("{wgsl}");
    }
}
