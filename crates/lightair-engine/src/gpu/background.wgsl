// Background blur, removal and replace: a camera picture, the host's person mask, and
// what goes behind the person. Values stay sRGB-encoded, as the feeds' are.

struct Params {
    // down/blur: xy one texel of the source, zw the direction (blur) in texels
    // composite: x mode (1 blur, 2 remove, 3 colour, 4 image), y threshold, z softness
    a: vec4<f32>,
    // composite: the colour behind (mode 3)
    b: vec4<f32>,
    // composite: x the mask's quarter turns, y mirrored, zw the image's cover scale
    c: vec4<f32>,
    // blur: x the tap spacing in texels
    d: vec4<f32>,
};

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var src: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var mask: texture_2d<f32>;
@group(0) @binding(4) var behind: texture_2d<f32>;

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

// a quarter of the size: four bilinear taps, a 4×4 box
@fragment
fn fs_down(i: Out) -> @location(0) vec4<f32> {
    let t = p.a.xy;
    var c = textureSample(src, samp, i.uv + vec2<f32>(-t.x, -t.y));
    c += textureSample(src, samp, i.uv + vec2<f32>(t.x, -t.y));
    c += textureSample(src, samp, i.uv + vec2<f32>(-t.x, t.y));
    c += textureSample(src, samp, i.uv + vec2<f32>(t.x, t.y));
    return vec4<f32>(c.rgb * 0.25, 1.0);
}

// one direction of a 9-tap Gaussian
@fragment
fn fs_blur(i: Out) -> @location(0) vec4<f32> {
    let w = array<f32, 5>(0.227027, 0.1945946, 0.1216216, 0.054054, 0.016216);
    let step = p.a.xy * p.a.zw * p.d.x;
    var c = textureSample(src, samp, i.uv).rgb * w[0];
    for (var k = 1; k < 5; k++) {
        let o = step * f32(k);
        c += textureSample(src, samp, i.uv + o).rgb * w[k];
        c += textureSample(src, samp, i.uv - o).rgb * w[k];
    }
    return vec4<f32>(c, 1.0);
}

// where an upright point is in the mask as computed (turned and mirrored as the picture was)
fn mask_uv(uv: vec2<f32>) -> vec2<f32> {
    var u = uv;
    if (p.c.y > 0.5) {
        u.x = 1.0 - u.x;
    }
    let q = u32(p.c.x + 0.5) % 4u;
    if (q == 1u) {
        return vec2<f32>(u.y, 1.0 - u.x);
    } else if (q == 2u) {
        return vec2<f32>(1.0 - u.x, 1.0 - u.y);
    } else if (q == 3u) {
        return vec2<f32>(1.0 - u.y, u.x);
    }
    return u;
}

@fragment
fn fs_composite(i: Out) -> @location(0) vec4<f32> {
    let cam = textureSample(src, samp, i.uv);
    let m = textureSample(mask, samp, mask_uv(i.uv)).r;
    let person = smoothstep(p.a.y - p.a.z, p.a.y + p.a.z, m);
    let mode = u32(p.a.x + 0.5);
    if (mode == 2u) {
        return vec4<f32>(cam.rgb, cam.a * person);
    }
    var back = p.b;
    if (mode == 1u) {
        back = textureSample(behind, samp, i.uv);
    } else if (mode == 4u) {
        back = textureSample(behind, samp, (i.uv - vec2<f32>(0.5)) * p.c.zw + vec2<f32>(0.5));
    }
    return vec4<f32>(mix(back.rgb, cam.rgb, person), 1.0);
}
