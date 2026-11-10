// Cada rectángulo y cada glifo es un quad instanciado, dibujado como triangle strip de 4 vértices.
// Las posiciones llegan en pixeles con origen arriba a la izquierda.

struct Globals {
    screen: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas: texture_2d<f32>;

// Esquinas del quad: (0,0) (1,0) (0,1) (1,1)
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

// --- Rectángulos (fondos, subrayados, cursor) ---

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

// --- Glifos ---
// El atlas guarda solo cobertura (R8). Como cada glifo se dibuja 1:1 sobre pixeles enteros,
// se lee con textureLoad en vez de muestrear: no hay filtrado ni coordenadas UV normalizadas.

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
