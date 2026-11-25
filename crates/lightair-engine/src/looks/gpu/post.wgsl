// FilterLut: the Colour grade's 3D LUT on the whole frame, as an (N·N)×N strip
// (x = r + b·N, y = g), sampled linear, clamped.

struct Lut {
    // filter_lut_size, filter_lut_amt
    p: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Lut;
@group(0) @binding(1) var image: texture_2d<f32>;
@group(0) @binding(2) var filter_lut_tex: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

struct V {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> V {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var o: V;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    o.uv = vec2<f32>(x, y);
    return o;
}

// The LUT applied to one pixel
@fragment
fn fs_filter_lut(i: V) -> @location(0) vec4<f32> {
    let c = textureSampleLevel(image, samp, i.uv, 0.0);
    let n = u.p.x;
    let b = saturate(c.b) * (n - 1.0);
    let b0 = floor(b);
    let b1 = min(b0 + 1.0, n - 1.0);
    let rg = (saturate(c.rg) * (n - 1.0) + 0.5) / vec2<f32>(n * n, n);
    let c0 = textureSampleLevel(filter_lut_tex, samp, rg + vec2<f32>(b0 / n, 0.0), 0.0).rgb;
    let c1 = textureSampleLevel(filter_lut_tex, samp, rg + vec2<f32>(b1 / n, 0.0), 0.0).rgb;
    return vec4<f32>(mix(c.rgb, mix(c0, c1, b - b0), u.p.y), c.a);
}
