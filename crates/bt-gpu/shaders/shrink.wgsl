// The `shrink` pipeline: one pass of a still's reduction chain. A single
// triangle covers the target and each of its pixels is the average of four
// taps of a rectangle of the source — the step that halves a texture, and
// the last step that brings what is left to the exact size of the tile.
// Read by `crate::renderer`; the Rust twin of `Immediates` is
// `crate::renderer::ShrinkImmediates` (src@0, tap@16; 24 bytes round up to
// the struct's 16-byte alignment, size 32, and the trailing 8 bytes are the
// Rust side's explicit `pad`).
//
//   src = the part of the source shown: u0, v0, u1, v1, normalised, top left
//         first (the last pass crops with it; the halvings show it all).
//   tap = where the four taps stand from a target pixel's centre, in the
//         source's uv. A pass whose source holds `r` texels per target pixel
//         (2 for a halving) puts them ((r - 1) / 2) texels away, clamped to
//         [0, 0.5]: at r = 2 each tap lands on a texel's centre, so the four
//         are exactly the 2×2 block under the pixel; at r = 1 they all land
//         on the pixel's own texel and the pass is a copy.
//
// The source is `Bgra8UnormSrgb`: the hardware decodes it to linear on
// sampling, so the average is taken in linear light, and the target (the same
// format) encodes it again. The sampler is linear and clamps to the edge.

struct Immediates {
    src: vec4<f32>,
    tap: vec2<f32>,
    pad: vec2<f32>,
}

var<immediate> imm: Immediates;

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

struct Out {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn shrink_vertex(@builtin(vertex_index) vid: u32) -> Out {
    // vid 0..2 → (0,0) (2,0) (0,2): one triangle that covers the unit square
    // (and the target with it); `corner` is the unit-square coordinate.
    let corner = vec2<f32>(f32((vid << 1u) & 2u), f32(vid & 2u));
    var o: Out;
    // Pixel space starts at the top left, NDC at the bottom left: flip y.
    o.position = vec4<f32>(corner.x * 2.0 - 1.0, 1.0 - corner.y * 2.0, 0.0, 1.0);
    o.uv = mix(imm.src.xy, imm.src.zw, corner);
    return o;
}

@fragment
fn shrink_fragment(in: Out) -> @location(0) vec4<f32> {
    let a = textureSampleLevel(source, source_sampler, in.uv + vec2<f32>(-imm.tap.x, -imm.tap.y), 0.0);
    let b = textureSampleLevel(source, source_sampler, in.uv + vec2<f32>(imm.tap.x, -imm.tap.y), 0.0);
    let c = textureSampleLevel(source, source_sampler, in.uv + vec2<f32>(-imm.tap.x, imm.tap.y), 0.0);
    let d = textureSampleLevel(source, source_sampler, in.uv + vec2<f32>(imm.tap.x, imm.tap.y), 0.0);
    // Opaque: the pipeline blends with `SrcAlpha`, so an alpha of one writes
    // the colour as it is.
    return vec4<f32>((a.rgb + b.rgb + c.rgb + d.rgb) * 0.25, 1.0);
}
