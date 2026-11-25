// The helper passes of the camera looks (downscale, separable bilateral and gaussian, the
// segmentation's refine, skin optical density, alpha packing, the face paint drawn through the
// mesh), plus the solid fills the landmark mask is drawn with. Values are sRGB-encoded, as on
// the desktop; every sampler is linear, clamped.

struct Blur {
    // 1 / source size
    texel: vec2<f32>,
    // (1,0) horizontal, (0,1) vertical
    dir: vec2<f32>,
    // tap spacing in source pixels
    step_px: f32,
    // bilateral range sigma (RGB distance)
    sigma_r: f32,
    // drawn geometry: the frame size its pixel positions are in (gs_ortho(0, w, 0, h))
    ortho: vec2<f32>,
    // Paint: which face-liner.png channel is the chosen style (all 0 = none)
    liner_sel: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Blur;
@group(0) @binding(1) var image: texture_2d<f32>;
// Refine: the frame (half res) / SkinOD: refined mask / WithAlpha: alpha source / Paint: the
// eyeliner styles (face-liner.png)
@group(0) @binding(2) var guide: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

struct V {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// gs_draw_sprite: one triangle covering the target, uv 0..1, y down
@vertex
fn vs(@builtin(vertex_index) i: u32) -> V {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var o: V;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    o.uv = vec2<f32>(x, y);
    return o;
}

// Geometry in frame pixels (the face mesh; the landmark mask's fans and bands): position in px,
// uv (the mesh's) or the fill value (x)
@vertex
fn vs_geom(@location(0) p: vec2<f32>, @location(1) uv: vec2<f32>) -> V {
    var o: V;
    o.pos = vec4<f32>(p.x / u.ortho.x * 2.0 - 1.0, 1.0 - p.y / u.ortho.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}

fn tap(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(image, samp, uv, 0.0);
}

fn gtap(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(guide, samp, uv, 0.0);
}

// A plain copy
@fragment
fn fs_copy(i: V) -> @location(0) vec4<f32> {
    return tap(i.uv);
}

// 17-tap edge-preserving blur along one axis
@fragment
fn fs_bilateral(i: V) -> @location(0) vec4<f32> {
    let c0 = tap(i.uv);
    var sum = c0.rgb;
    var wsum = 1.0;
    let inv2sr2 = 1.0 / (2.0 * u.sigma_r * u.sigma_r);
    let o = u.dir * u.texel * u.step_px;
    for (var k = 1; k <= 8; k++) {
        let fi = f32(k);
        let ws = exp(-fi * fi / 24.5); // spatial sigma = 3.5 taps
        let a = tap(i.uv + o * fi).rgb;
        let b = tap(i.uv - o * fi).rgb;
        let da = a - c0.rgb;
        let db = b - c0.rgb;
        let wa = ws * exp(-dot(da, da) * inv2sr2);
        let wb = ws * exp(-dot(db, db) * inv2sr2);
        sum += a * wa + b * wb;
        wsum += wa + wb;
    }
    return vec4<f32>(sum / wsum, c0.a);
}

// 9-tap gaussian along one axis
@fragment
fn fs_gauss(i: V) -> @location(0) vec4<f32> {
    let o = u.dir * u.texel * u.step_px;
    var sum = tap(i.uv) * 0.2270270;
    sum += (tap(i.uv + o * 1.3846154) + tap(i.uv - o * 1.3846154)) * 0.3162162;
    sum += (tap(i.uv + o * 3.2307692) + tap(i.uv - o * 3.2307692)) * 0.0702703;
    return sum;
}

// Joint-bilateral upsampling of the low-res segmentation
// (image = mask, guide = frame)
@fragment
fn fs_refine(i: V) -> @location(0) vec4<f32> {
    let c = gtap(i.uv).rgb;
    var acc = vec4<f32>(0.0);
    var wsum = 0.0;
    for (var j = -2; j <= 2; j++) {
        for (var k = -2; k <= 2; k++) {
            let kk = vec2<f32>(f32(k), f32(j));
            let uv = i.uv + kk * u.texel;
            let g = gtap(uv).rgb;
            let d = g - c;
            let w = exp(-dot(kk, kk) / 4.5) * exp(-dot(d, d) / (2.0 * u.sigma_r * u.sigma_r)) + 1e-4;
            acc += tap(uv) * w;
            wsum += w;
        }
    }
    return acc / wsum;
}

// sRGB to linear
fn srgb_to_lin(c: vec3<f32>) -> vec3<f32> {
    return mix(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, step(c, vec3<f32>(0.04045)));
}

// Skin-weighted optical density, premultiplied
@fragment
fn fs_skin_od(i: V) -> @location(0) vec4<f32> {
    let c = tap(i.uv).rgb;
    let od = -log(max(srgb_to_lin(saturate(c)), vec3<f32>(0.001)));
    var w = gtap(i.uv).r;
    let mx = max(c.r, max(c.g, c.b));
    let mn = min(c.r, min(c.g, c.b));
    w *= step(mx, 0.97) * step(0.05, mx) * (1.0 - step(0.75, mx) * step(mx - mn, 0.25 * mx));
    return vec4<f32>(od * w, w);
}

// Rgb from image, alpha from guide
@fragment
fn fs_with_alpha(i: V) -> @location(0) vec4<f32> {
    return vec4<f32>(tap(i.uv).rgb, gtap(i.uv).a);
}

// Face paint drawn through the face mesh, the chosen
// eyeliner style in B (mipmapped maps: implicit level)
@fragment
fn fs_paint(i: V) -> @location(0) vec4<f32> {
    let p = textureSample(image, samp, i.uv);
    let l = textureSample(guide, samp, i.uv);
    return vec4<f32>(p.r, p.g, dot(l, u.liner_sel), p.a);
}

// A plain copy through the face mesh (for `mesh::render_facenorm`)
@fragment
fn fs_mesh_copy(i: V) -> @location(0) vec4<f32> {
    return textureSample(image, samp, i.uv);
}

// One colour: the value in uv.x on every channel (the landmark mask writes 0 or 1 into one
// channel at a time, blending off)
@fragment
fn fs_solid(i: V) -> @location(0) vec4<f32> {
    return vec4<f32>(i.uv.x);
}
