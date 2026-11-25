// Sharpen and the keyers on a source's picture, matching the desktop's sharpness, chroma, colour
// and luma key filters: one effect per pass.
// Values stay sRGB-encoded, as the pictures' are. Out: straight (not premultiplied) RGBA.

struct Params {
    // x the effect (1 sharpen, 2 chroma key, 3 colour key, 4 luma key), y the picture comes in
    // premultiplied, zw one texel
    a: vec4<f32>,
    // the key colour (rgb)
    key: vec4<f32>,
    // sharpen: x amount. chroma/colour: x similarity, y smoothness, z spill (0–1).
    // luma: x the darks removed below, y kept up to, z the edge's softness
    b: vec4<f32>,
    // grade: x contrast, y saturation, z warmth (−1–1), w tint (−1–1)
    d: vec4<f32>,
    // grade: x exposure (stops), y fade (0–1), z intensity (0–1), w mono (0/1)
    e: vec4<f32>,
    // grade: the shadows' tint (rgb, added), and the highlights' in key
    // light: the rim's colour
    f: vec4<f32>,
    // the person mask: x quarter turns, y mirrored, z there is one
    g: vec4<f32>,
};

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var src: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var mask: texture_2d<f32>;

// where an upright point is in the mask as computed (turned and mirrored as the picture was)
fn mask_uv(uv: vec2<f32>) -> vec2<f32> {
    var u = uv;
    if (p.g.y > 0.5) {
        u.x = 1.0 - u.x;
    }
    let q = u32(p.g.x + 0.5) % 4u;
    if (q == 1u) {
        return vec2<f32>(u.y, 1.0 - u.x);
    } else if (q == 2u) {
        return vec2<f32>(1.0 - u.x, 1.0 - u.y);
    } else if (q == 3u) {
        return vec2<f32>(1.0 - u.y, u.x);
    }
    return u;
}

fn person(uv: vec2<f32>) -> f32 {
    if (p.g.z < 0.5) {
        return 1.0;
    }
    return textureSample(mask, samp, mask_uv(uv)).r;
}

fn to_linear(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3<f32>(0.0)), vec3<f32>(2.2));
}

fn to_srgb(c: vec3<f32>) -> vec3<f32> {
    return pow(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / 2.2));
}

// how much a colour looks like skin (YCbCr's usual skin range, soft-edged)
fn skin(c: vec3<f32>) -> f32 {
    let y = dot(c, vec3<f32>(0.299, 0.587, 0.114));
    let cb = (c.b - y) * 0.564 + 0.5;
    let cr = (c.r - y) * 0.713 + 0.5;
    let a = smoothstep(0.50, 0.54, cr) * (1.0 - smoothstep(0.66, 0.70, cr));
    let b = smoothstep(0.30, 0.34, cb) * (1.0 - smoothstep(0.48, 0.52, cb));
    return a * b * smoothstep(0.08, 0.15, y);
}

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

fn straight(c: vec4<f32>) -> vec4<f32> {
    if (p.a.y > 0.5 && c.a > 0.0) {
        return vec4<f32>(c.rgb / c.a, c.a);
    }
    return c;
}

fn tap(uv: vec2<f32>) -> vec4<f32> {
    return straight(textureSample(src, samp, uv));
}

const LUMA: vec3<f32> = vec3<f32>(0.299, 0.587, 0.114);

// BT.601 chroma (Cb, Cr), as keyers measure colour apart from brightness
fn chroma(c: vec3<f32>) -> vec2<f32> {
    let y = dot(c, LUMA);
    return vec2<f32>((c.b - y) * 0.565, (c.r - y) * 0.713);
}

// Spill suppression: the part of the pixel's chroma that points the way the key colour does is
// taken out, by `amount` (1 at the key colour, fading with distance from it). Brightness is kept:
// the colour is rebuilt from the same BT.601 luma, so a green-lit edge turns neutral, not dark.
fn suppress_spill(rgb: vec3<f32>, key: vec2<f32>, amount: f32) -> vec3<f32> {
    let k = length(key);
    if (k < 0.0001 || amount <= 0.0) {
        return rgb;
    }
    let dir = key / k;
    let y = dot(rgb, LUMA);
    let c = chroma(rgb);
    let c2 = c - dir * max(dot(c, dir), 0.0) * amount;
    // back to RGB from luma and the new chroma
    let r = y + c2.y / 0.713;
    let b = y + c2.x / 0.565;
    let g = (y - LUMA.x * r - LUMA.z * b) / LUMA.y;
    return clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
    let c = tap(i.uv);
    let mode = u32(p.a.x + 0.5);
    let t = p.a.zw;
    if (mode == 1u) {
        // unsharp: the picture plus its difference from its four neighbours
        let n = tap(i.uv + vec2<f32>(0.0, -t.y)).rgb + tap(i.uv + vec2<f32>(0.0, t.y)).rgb
            + tap(i.uv + vec2<f32>(-t.x, 0.0)).rgb + tap(i.uv + vec2<f32>(t.x, 0.0)).rgb;
        let rgb = c.rgb + (c.rgb * 4.0 - n) * p.b.x;
        return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), c.a);
    }
    if (mode == 2u || mode == 3u) {
        var dist: f32;
        if (mode == 2u) {
            dist = distance(chroma(c.rgb), chroma(p.key.rgb));
        } else {
            dist = distance(c.rgb, p.key.rgb);
        }
        // opacity: clear within `similarity` of the key, opaque past `similarity + smoothness`,
        // an eased ramp between
        let sim = p.b.x;
        let alpha = smoothstep(sim, sim + max(p.b.y, 0.0001), dist);
        var rgb = c.rgb;
        if (mode == 2u) {
            rgb = suppress_spill(rgb, chroma(p.key.rgb), 1.0 - smoothstep(sim, sim + max(p.b.z, 0.0001), dist));
        }
        return vec4<f32>(rgb, c.a * alpha);
    }
    if (mode == 5u) {
        // a colour grade (the phone's own, by the desktop's grade names)
        var rgb = c.rgb * exp2(p.e.x);
        rgb = (rgb - 0.5) * p.d.x + 0.5;
        // warmth: red up and blue down; tint: green against magenta
        rgb = rgb + vec3<f32>(p.d.z * 0.06, -p.d.w * 0.04, -p.d.z * 0.06);
        let y = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
        rgb = mix(vec3<f32>(y), rgb, p.d.y);
        if (p.e.w > 0.5) {
            rgb = vec3<f32>(y);
        }
        // split toning: shadows toward f, highlights toward key
        let lum = clamp(y, 0.0, 1.0);
        rgb = rgb + p.f.rgb * (1.0 - lum) * (1.0 - lum) + p.key.rgb * lum * lum;
        // fade: lifted blacks, softened whites
        rgb = mix(rgb, rgb * 0.88 + 0.08, p.e.y);
        rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
        return vec4<f32>(mix(c.rgb, rgb, p.e.z), c.a);
    }
    if (mode == 6u) {
        // skin smoothing: a soft ring of samples, mixed in where it's skin on the person
        let r = (2.0 + 6.0 * p.b.x) * t;
        var blur = vec3<f32>(0.0);
        for (var k = 0; k < 12; k++) {
            let a = f32(k) * 0.5235988;
            blur += tap(i.uv + vec2<f32>(cos(a), sin(a)) * r).rgb;
        }
        blur = blur / 12.0;
        let s = skin(c.rgb) * person(i.uv);
        var rgb = mix(c.rgb, blur, p.b.x * 0.85 * s);
        // foundation: an even tone (brightness from the smoothed picture)
        let y = max(dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722)), 0.001);
        let yb = dot(blur, vec3<f32>(0.2126, 0.7152, 0.0722));
        rgb = rgb * mix(1.0, yb / y, p.b.y * 0.5 * s);
        // redness: red over the skin's own taken down
        let red = max(rgb.r - (rgb.g + rgb.b) * 0.5 - 0.12, 0.0);
        rgb.r = rgb.r - red * p.b.z * s;
        // clarity: the picture's detail brought up
        rgb = rgb + (c.rgb - blur) * p.d.x * 0.6;
        return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), c.a);
    }
    if (mode == 7u) {
        // a light on the person: the key from its side, a fill, a rim at the silhouette's edge,
        // the background dimmed; in linear light so it reads as light, not paint
        let m = person(i.uv);
        let amount = p.e.z;
        var lin = to_linear(c.rgb);
        let towards = vec2<f32>(p.d.x, -p.d.y);
        let k = clamp(0.5 + 1.2 * dot(i.uv - vec2<f32>(0.5), towards), 0.0, 1.0);
        lin = lin * (vec3<f32>(1.0) + amount * m * (p.d.z * k * p.key.rgb * 1.6 + vec3<f32>(p.d.w)));
        let rim = smoothstep(0.15, 0.45, m) * (1.0 - smoothstep(0.55, 0.9, m));
        lin = lin + amount * p.e.x * rim * p.f.rgb * 0.6;
        lin = lin * (1.0 - amount * p.e.y * (1.0 - m));
        return vec4<f32>(to_srgb(lin), c.a);
    }
    if (mode == 4u) {
        let y = dot(c.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
        let soft = max(p.b.z, 0.0001);
        let lo = smoothstep(p.b.x - soft, p.b.x + soft, y);
        let hi = 1.0 - smoothstep(p.b.y - soft, p.b.y + soft, y);
        return vec4<f32>(c.rgb, c.a * lo * hi);
    }
    return c;
}
