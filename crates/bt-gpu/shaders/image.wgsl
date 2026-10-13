// The `image` pipeline: one picture of the kitty graphics protocol, one quad,
// one texture. Read by `crate::renderer`; the top edge's ramp (`edge.wgsl`) is
// appended at compile time, as it is to `cell_bg.wgsl` and `cell.wgsl`, so a
// picture fades under the content's top edge like the text beside it.
//
// There is no vertex buffer: a picture is a draw of its own (each has its own
// texture), so its rectangle rides in the immediates, like the caret's.
//
// The Rust twin of `Immediates` is `crate::renderer::ImageImmediates`:
// rect@0, uv@16, viewport_px@32, edge_px@40; 44 bytes round up to the struct's
// 16-byte alignment, size 48, and the trailing 4 bytes are the Rust side's
// explicit `pad`.
//
//   rect        = the picture's x, y, width, height in pixels, in the
//                 viewport's own space (the grid's or the fill band's: the
//                 viewport carries the scroll offset, as it does for cells).
//                 The rectangle is already cut to its surface
//                 (`Frame::picture_quad`), so nothing here clips.
//   uv          = the part of the texture shown: u0, v0, u1, v1, normalised,
//                 top left first (the image's rows run top down, like the
//                 window's).
//   viewport_px = the texture's size: the NDC scale, as in `cell_bg.wgsl`.
//   edge_px     = the content's top fade (`edge.wgsl`).

struct Immediates {
    rect: vec4<f32>,
    uv: vec4<f32>,
    viewport_px: vec2<f32>,
    edge_px: f32,
}

var<immediate> imm: Immediates;

// The picture, `Rgba8UnormSrgb`: the hardware decodes it to linear on sampling
// and the target encodes the fragment's linear output again, so the colour
// comes out as the program sent it. The sampler is linear and clamps to the
// edge: a picture drawn at another size than its own scales smoothly.
@group(0) @binding(0) var picture: texture_2d<f32>;
@group(0) @binding(1) var picture_sampler: sampler;

struct Out {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn image_vertex(@builtin(vertex_index) vid: u32) -> Out {
    // vid 0..3 → (0,0) (1,0) (0,1) (1,1): the strip covers the unit square.
    let corner = vec2<f32>(f32(vid & 1u), f32(vid >> 1u));
    let at = imm.rect.xy + corner * imm.rect.zw;
    let ndc = at / imm.viewport_px * 2.0 - 1.0;
    var o: Out;
    // Pixel space starts at the top left, NDC at the bottom left: flip y.
    o.position = vec4<f32>(ndc.x, -ndc.y, 0.0, 1.0);
    o.uv = mix(imm.uv.xy, imm.uv.zw, corner);
    return o;
}

@fragment
fn image_fragment(in: Out) -> @location(0) vec4<f32> {
    // Straight alpha, like every pipeline's output here (`renderer::pipeline`
    // blends `SrcAlpha`); the pixels were never premultiplied.
    let texel = textureSampleLevel(picture, picture_sampler, in.uv, 0.0);
    return vec4<f32>(texel.rgb, texel.a * edge_alpha(in.position.y, imm.edge_px));
}
