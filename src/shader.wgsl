// Every rectangle and every glyph is an instanced quad, drawn as a 4-vertex triangle strip.
// Positions arrive in pixels, with the origin at the top left.

struct Globals {
    screen: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas: texture_2d<f32>;

// Quad corners: (0,0) (1,0) (0,1) (1,1)
fn corner(index: u32) -> vec2<f32> {
    return vec2<f32>(f32(index & 1u), f32(index >> 1u));
}

fn to_clip(p: vec2<f32>) -> vec4<f32> {
    let ndc = vec2<f32>(
        p.x / globals.screen.x * 2.0 - 1.0,
        1.0 - p.y / globals.screen.y * 2.0,
    );
    return vec4<f32>(ndc, 0.0, 1.0);
}

// --- Rectangles (backgrounds, underlines, cursor) ---

struct RectOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_rect(
    @builtin(vertex_index) vertex: u32,
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
) -> RectOut {
    var out: RectOut;
    out.clip = to_clip(pos + corner(vertex) * size);
    out.color = color;
    return out;
}

@fragment
fn fs_rect(v: RectOut) -> @location(0) vec4<f32> {
    return v.color;
}

// --- Glyphs ---
// The atlas stores coverage only (R8). Since each glyph is drawn 1:1 on whole pixels, it is
// read with textureLoad instead of sampled: no filtering and no normalized UV coordinates.

struct GlyphOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) texel: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_glyph(
    @builtin(vertex_index) vertex: u32,
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) atlas_pos: vec2<f32>,
    @location(3) color: vec4<f32>,
) -> GlyphOut {
    let c = corner(vertex);
    var out: GlyphOut;
    out.clip = to_clip(pos + c * size);
    out.texel = atlas_pos + c * size;
    out.color = color;
    return out;
}

@fragment
fn fs_glyph(v: GlyphOut) -> @location(0) vec4<f32> {
    let coverage = textureLoad(atlas, vec2<i32>(floor(v.texel)), 0).r;
    return vec4<f32>(v.color.rgb, v.color.a * coverage);
}
