// The program texture drawn onto another target, scaled. The source
// is sampled as linear light (an sRGB texture); `encode` writes sRGB values by hand for targets
// whose format isn't an sRGB one (an encoder's BGRA buffer).

struct Params {
    // x: encode to sRGB (1) or not (0)
    p: vec4<f32>,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var src: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    // one triangle covering the target
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var o: Out;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    o.uv = vec2<f32>(x, y);
    return o;
}

fn to_srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
    let c = textureSample(src, samp, i.uv);
    if (params.p.x > 0.5) {
        return vec4<f32>(to_srgb(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0))), 1.0);
    }
    return vec4<f32>(c.rgb, 1.0);
}
