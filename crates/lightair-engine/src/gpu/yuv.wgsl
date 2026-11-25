// YUV 4:2:0 (a luma texture and an interleaved chroma texture) to RGBA, turned upright and
// mirrored as the feed asks. The result holds sRGB-encoded values, as
// uploaded pictures do.

struct Params {
    // coefficients: R = Y + x·Cr, G = Y − y·Cb − z·Cr, B = Y + w·Cb
    k: vec4<f32>,
    // x: limited range (1) or full (0); y: rotation clockwise in quarter turns; z: mirrored
    o: vec4<f32>,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var luma: texture_2d<f32>;
@group(0) @binding(2) var chroma: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var o: Out;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    o.uv = vec2<f32>(x, y);
    return o;
}

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
    var u = i.uv;
    if (params.o.z > 0.5) {
        u.x = 1.0 - u.x;
    }
    // where this upright point was in the picture as captured
    let q = u32(params.o.y + 0.5) % 4u;
    var s = u;
    if (q == 1u) {
        s = vec2<f32>(u.y, 1.0 - u.x);
    } else if (q == 2u) {
        s = vec2<f32>(1.0 - u.x, 1.0 - u.y);
    } else if (q == 3u) {
        s = vec2<f32>(1.0 - u.y, u.x);
    }
    var y = textureSample(luma, samp, s).r;
    var c = textureSample(chroma, samp, s).rg - vec2<f32>(0.5, 0.5);
    if (params.o.x > 0.5) {
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        c = c * (255.0 / 224.0);
    }
    let k = params.k;
    let rgb = vec3<f32>(y + k.x * c.y, y - k.y * c.x - k.z * c.y, y + k.w * c.x);
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
