//! Does wgpu actually *accept* the WGSL we generate for a custom shader?
//!
//! `src/shader.rs`'s unit tests prove naga can translate Shadertoy GLSL, but
//! naga's own validator does not enforce everything WebGPU does — in particular
//! the uniform address space requires every array element stride to be a
//! multiple of 16, which `array<f32, 4>` (from GLSL's `float iChannelTime[4]`)
//! violates. That failure appears only when a real device compiles the module,
//! so it needs a real device.
//!
//! This is the gate that decides whether the uniform block in
//! [`geist::shader::PREFIX`] is shippable at all. It skips cleanly when no GPU
//! adapter is available (headless CI), like the render bench.

use eframe::wgpu;

fn headless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("geist-shader-test-device"),
        ..Default::default()
    }))
    .ok()?;
    Some((device, queue))
}

/// Translate `glsl` and hand the result to a real device, returning whatever
/// the device said.
///
/// Only `create_shader_module` is scoped: that call is where WebGPU enforces
/// the address-space layout rules the uniform block has to satisfy, and it is
/// the whole point of this file. The pipeline is built separately below, where
/// a complaint about the *vertex* stage isn't a shader problem.
fn build_module(device: &wgpu::Device, glsl: &str) -> Result<(), String> {
    let wgsl = geist::shader::compile(glsl).map_err(|e| format!("translation failed: {e:#}"))?;

    // wgpu reports shader-creation errors through an error scope, not a Result.
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let _module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("custom-shader-test"),
        source: wgpu::ShaderSource::Wgsl(wgsl.into()),
    });
    match pollster::block_on(scope.pop()) {
        Some(e) => Err(format!("{e}")),
        None => Ok(()),
    }
}

/// Ghostty's CRT test shader (see the copy in `src/shader.rs`'s tests).
const CRT: &str = r#"
vec2 curve(vec2 uv) {
    uv = (uv - 0.5) * 2.0;
    uv *= 1.1;
    uv.x *= 1.0 + pow((abs(uv.y) / 5.0), 2.0);
    uv.y *= 1.0 + pow((abs(uv.x) / 4.0), 2.0);
    uv = (uv / 2.0) + 0.5;
    uv = uv * 0.92 + 0.04;
    return uv;
}
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 q = fragCoord.xy / iResolution.xy;
    vec2 uv = curve(q);
    vec3 col;
    float x = sin(0.3 * iTime + uv.y * 21.0) * 0.0017;
    col.r = texture(iChannel0, vec2(x + uv.x + 0.001, uv.y + 0.001)).x + 0.05;
    col.g = texture(iChannel0, vec2(x + uv.x + 0.000, uv.y - 0.002)).y + 0.05;
    col.b = texture(iChannel0, vec2(x + uv.x - 0.002, uv.y + 0.000)).z + 0.05;
    col = clamp(col * 0.6 + 0.4 * col * col, 0.0, 1.0);
    float vig = 16.0 * uv.x * uv.y * (1.0 - uv.x) * (1.0 - uv.y);
    col *= vec3(pow(vig, 0.3));
    float scans = clamp(0.35 + 0.35 * sin(3.5 * iTime + uv.y * iResolution.y * 1.5), 0.0, 1.0);
    col *= vec3(0.4 + 0.7 * pow(scans, 1.7));
    if (uv.x < 0.0 || uv.x > 1.0) col *= 0.0;
    col *= 1.0 - 0.65 * vec3(clamp((mod(fragCoord.x, 2.0) - 1.0) * 2.0, 0.0, 1.0));
    fragColor = vec4(col, 1.0);
}
"#;

/// A shader touching every uniform in the block, so no field's layout goes
/// unexercised — an unused uniform can be optimised away before the device ever
/// checks it.
const ALL_UNIFORMS: &str = r#"
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    float acc = iTime + iTimeDelta + iFrameRate + float(iFrame)
        + iChannelTime[0] + iChannelResolution[1].x + iMouse.x + iDate.w
        + iSampleRate + iCurrentCursor.x + iPreviousCursor.y
        + iCurrentCursorColor.z + iPreviousCursorColor.w
        + float(iCurrentCursorStyle) + float(iPreviousCursorStyle)
        + float(iCursorVisible) + iTimeCursorChange + iTimeFocus + float(iFocus);
    vec3 tint = iBackgroundColor + iForegroundColor + iCursorColor + iCursorText
        + iSelectionForegroundColor + iSelectionBackgroundColor;
    fragColor = vec4(texture(iChannel0, uv).rgb * tint * fract(acc), 1.0);
}
"#;

const TRIVIAL: &str = r#"
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    fragColor = texture(iChannel0, uv) * vec4(uv, 0.5 + 0.5 * sin(iTime), 1.0);
}
"#;

#[test]
fn generated_wgsl_is_accepted_by_a_real_device() {
    let Some((device, _queue)) = headless_device() else {
        eprintln!("skipped: no wgpu adapter available");
        return;
    };
    for (name, glsl) in [
        ("trivial", TRIVIAL),
        ("all-uniforms", ALL_UNIFORMS),
        ("ghostty CRT", CRT),
    ] {
        if let Err(e) = build_module(&device, glsl) {
            panic!("the device rejected the {name} shader:\n{e}");
        }
    }
}
