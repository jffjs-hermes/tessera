struct Globals {
    grid: vec2<u32>,       // cells
    cell_px: vec2<f32>,    // cell size in pixels
    screen: vec2<f32>,     // surface size in pixels
    atlas_cols: u32,
    atlas_rows: u32,
};
@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_samp: sampler;

struct Inst {
    @location(0) cell: vec2<u32>,
    @location(1) glyph: u32,
    @location(2) fg: vec4<f32>,
    @location(3) bg: vec4<f32>,
};
struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) fg: vec4<f32>,
    @location(2) bg: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, i: Inst) -> VOut {
    // Two triangles from 6 vertex indices.
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0));
    let c = corners[vi];
    let px = (vec2<f32>(i.cell) + c) * g.cell_px;
    let ndc = vec2(px.x / g.screen.x * 2.0 - 1.0, 1.0 - px.y / g.screen.y * 2.0);
    let col = f32(i.glyph % g.atlas_cols);
    let row = f32(i.glyph / g.atlas_cols);
    let uv = (vec2(col, row) + c) / vec2(f32(g.atlas_cols), f32(g.atlas_rows));
    var o: VOut;
    o.pos = vec4(ndc, 0.0, 1.0);
    o.uv = uv;
    o.fg = i.fg;
    o.bg = i.bg;
    return o;
}

@fragment
fn fs(v: VOut) -> @location(0) vec4<f32> {
    let a = textureSample(atlas, atlas_samp, v.uv).r;
    return vec4(mix(v.bg.rgb, v.fg.rgb, a), 1.0);
}
