// The camera looks' main pass (face warp ->
// skin masks -> frequency-separated smoothing -> pigments -> teeth -> makeup -> bright eyes ->
// clarity) and the Finish pass (relight, then the makeup look's colour grade). Values are
// sRGB-encoded 0..1, as on the desktop, so the constants are the same. Every sampler is linear,
// clamped; data_tex is read texel by texel (Load).

// The pass's uniforms, as vec4s
struct U {
    // size.xy (output px), face_weight, warp_count
    a0: vec4<f32>,
    // x0, y0, x1, y1 (pixels)
    warp_bounds: vec4<f32>,
    // all_skin, smooth_amt, foundation_amt, skin_tone_amt
    skin0: vec4<f32>,
    // pouch_amt, teeth_amt, eyelight_amt, clarity_amt
    skin1: vec4<f32>,
    // debug_view, has_paint, filter_amt, makeup_amt
    misc: vec4<f32>,
    // exposure, contrast, saturation, warmth (-1..1)
    grade0: vec4<f32>,
    // tint, fade
    grade1: vec4<f32>,
    // rgb + opacity
    lip_color: vec4<f32>,
    blush_color: vec4<f32>,
    shadow_color: vec4<f32>,
    // -, contour, highlight, brow
    makeup_extra: vec4<f32>,
    // lip_gloss, light_amt, has_person, has_facenorm
    misc2: vec4<f32>,
    // xyz towards the light (x right, y down, z camera)
    light_dir: vec4<f32>,
    // rgb + strength
    light_key: vec4<f32>,
    light_rim: vec4<f32>,
    // fill, bg_dim, exposure, warmth
    light_misc: vec4<f32>,
    // person_texel.xy, has_seg, has_prot
    seg: vec4<f32>,
    // pig_mel.xyz, redness_amt
    pig_mel: vec4<f32>,
    // pig_hem.xyz, head_scale (px per canonical cm, 0 = no head pose this frame)
    pig_hem: vec4<f32>,
    // inverse basis rows: OD -> m, OD -> h
    pig_r0: vec4<f32>,
    pig_r1: vec4<f32>,
    // the canonical face axes in image space (head pose)
    face_rx: vec4<f32>,
    face_ry: vec4<f32>,
    face_rz: vec4<f32>,
    // head_center.xy (px)
    head_center: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: U;
// original frame (Finish: the main pass's output)
@group(0) @binding(1) var image: texture_2d<f32>;
// half-res bilateral
@group(0) @binding(2) var smooth_tex: texture_2d<f32>;
// 1/8-res gaussian of the frame; alpha = soft body height field (light)
@group(0) @binding(3) var tone_tex: texture_2d<f32>;
// warp primitives (texels 0..127) + region ellipses (128..191), RGBA32F
@group(0) @binding(4) var data_tex: texture_2d<f32>;
// half-res landmark raster: R lips, B eye interior + mouth opening, A brows
@group(0) @binding(5) var mask_tex: texture_2d<f32>;
// face-paint.png drawn through the tracked face mesh: R contour, G highlight, B eyeliner, A brow
@group(0) @binding(6) var paint_tex: texture_2d<f32>;
// refined segmentation: R skin, G face skin, B hair, A person
@group(0) @binding(7) var seg_tex: texture_2d<f32>;
// skin-weighted local average optical density (premultiplied, a = weight)
@group(0) @binding(8) var skin_od_tex: texture_2d<f32>;
// Finish: face-normal.png drawn through the tracked mesh
@group(0) @binding(9) var facenorm_tex: texture_2d<f32>;
@group(0) @binding(10) var samp: sampler;

fn size() -> vec2<f32> { return u.a0.xy; }
fn face_weight() -> f32 { return u.a0.z; }
fn warp_count() -> i32 { return i32(u.a0.w + 0.5); }
fn all_skin() -> f32 { return u.skin0.x; }
fn smooth_amt() -> f32 { return u.skin0.y; }
fn foundation_amt() -> f32 { return u.skin0.z; }
fn skin_tone_amt() -> f32 { return u.skin0.w; }
fn pouch_amt() -> f32 { return u.skin1.x; }
fn teeth_amt() -> f32 { return u.skin1.y; }
fn eyelight_amt() -> f32 { return u.skin1.z; }
fn clarity_amt() -> f32 { return u.skin1.w; }
fn debug_view() -> i32 { return i32(round(u.misc.x)); }
fn has_paint() -> f32 { return u.misc.y; }
fn filter_amt() -> f32 { return u.misc.z; }
fn makeup_amt() -> f32 { return u.misc.w; }
fn lip_gloss() -> f32 { return u.misc2.x; }
fn light_amt() -> f32 { return u.misc2.y; }
fn has_person() -> f32 { return u.misc2.z; }
fn has_facenorm() -> f32 { return u.misc2.w; }
fn person_texel() -> vec2<f32> { return u.seg.xy; }
fn has_seg() -> f32 { return u.seg.z; }
fn has_prot() -> f32 { return u.seg.w; }
fn pig_mel() -> vec3<f32> { return u.pig_mel.xyz; }
fn pig_hem() -> vec3<f32> { return u.pig_hem.xyz; }
fn pig_r0() -> vec3<f32> { return u.pig_r0.xyz; }
fn pig_r1() -> vec3<f32> { return u.pig_r1.xyz; }
fn redness_amt() -> f32 { return u.pig_mel.w; }
fn head_scale() -> f32 { return u.pig_hem.w; }

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

fn smp(t: texture_2d<f32>, uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(t, samp, uv, 0.0);
}

fn data(x: i32) -> vec4<f32> {
    return textureLoad(data_tex, vec2<i32>(x, 0), 0);
}

const U3 = vec3<f32>(0.57735027, 0.57735027, 0.57735027);

// pow for x >= 0 with pow(0, y > 0) = 0 (HLSL's on every libobs back end)
fn powp(x: f32, y: f32) -> f32 {
    return select(pow(x, y), 0.0, x <= 0.0);
}

// The head as a whole: an ellipsoid in the canonical face frame (cm, centre (0, 3, -4))
const HEAD_R = vec3<f32>(8.0, 12.0, 9.5);

struct HeadN {
    n: vec3<f32>,
    w: f32,
};

// head_normal: the head ellipsoid's surface direction seen at image
// point p (px), and how far inside its outline the point is; w = 0 when p misses the head
fn head_normal(p: vec2<f32>) -> HeadN {
    var r: HeadN;
    r.w = 0.0;
    r.n = vec3<f32>(0.0, 0.0, 1.0);
    if (head_scale() <= 0.0) {
        return r;
    }
    let rx = u.face_rx.xyz;
    let ry = u.face_ry.xyz;
    let rz = u.face_rz.xyz;
    // camera ray through p (orthographic, along z) in the canonical frame: u = a + t * b
    let q = (p - u.head_center.xy) / head_scale();
    let a = vec3<f32>(dot(rx.xy, q), dot(ry.xy, q), dot(rz.xy, q)); // R^T (q, 0)
    let b = vec3<f32>(rx.z, ry.z, rz.z); // R^T (0, 0, 1)
    let ir = 1.0 / HEAD_R;
    let ai = a * ir;
    let bi = b * ir;
    let A = dot(bi, bi);
    let B = 2.0 * dot(ai, bi);
    let C = dot(ai, ai) - 1.0;
    let disc = B * B - 4.0 * A * C;
    if (disc <= 0.0) {
        return r;
    }
    let sq = sqrt(disc);
    let t = (-B + sq) / (2.0 * A); // the side towards the camera
    let uu = a + t * b;
    let nu = uu * ir * ir; // ellipsoid gradient, canonical frame
    r.n = normalize(rx * nu.x + ry * nu.y + rz * nu.z);
    r.w = smoothstep(0.0, 0.8, sq / (2.0 * A) / 9.5); // half chord, in head radii
    return r;
}

// Rgb to YCbCr (BT.601, full range)
fn to_ycc(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(dot(c, vec3<f32>(0.299, 0.587, 0.114)),
                     0.5 + dot(c, vec3<f32>(-0.168736, -0.331264, 0.5)),
                     0.5 + dot(c, vec3<f32>(0.5, -0.418688, -0.081312)));
}

// YCbCr to rgb
fn to_rgb(y: vec3<f32>) -> vec3<f32> {
    let cb = y.y - 0.5;
    let cr = y.z - 0.5;
    return vec3<f32>(y.x + 1.402 * cr, y.x - 0.344136 * cb - 0.714136 * cr, y.x + 1.772 * cb);
}

// sRGB to linear
fn srgb2lin(c0: vec3<f32>) -> vec3<f32> {
    let c = saturate(c0);
    return mix(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, step(c, vec3<f32>(0.04045)));
}

// Linear to sRGB
fn lin2srgb(c0: vec3<f32>) -> vec3<f32> {
    let c = saturate(c0);
    let p = vec3<f32>(powp(c.x, 1.0 / 2.4), powp(c.y, 1.0 / 2.4), powp(c.z, 1.0 / 2.4));
    return mix(1.055 * p - 0.055, c * 12.92, step(c, vec3<f32>(0.0031308)));
}

// Optical density of an sRGB colour
fn od_of(srgb: vec3<f32>) -> vec3<f32> {
    return -log(max(srgb2lin(srgb), vec3<f32>(0.001)));
}

// Brightness of an optical density
fn od_bright(od: vec3<f32>) -> f32 {
    return dot(od, U3);
}

// Chroma of an optical density (its brightness removed)
fn od_chroma(od: vec3<f32>) -> vec3<f32> {
    return od - od_bright(od) * U3;
}

// 0 on specular shine (bright, colourless)
fn shine_keep(lin: vec3<f32>) -> f32 {
    let ll = dot(lin, vec3<f32>(0.2126, 0.7152, 0.0722));
    let lsat = max(lin.r, max(lin.g, lin.b)) - min(lin.r, min(lin.g, lin.b));
    return 1.0 - saturate((ll - 0.45) / 0.25) * saturate(1.0 - lsat / max(ll, 0.001) / 0.35);
}

// A cosmetic layer as an OD change (scatter, film, value)
fn deposit(od_ref: vec3<f32>, od_c: vec3<f32>, od_val_ref: vec3<f32>, scatter: f32, film: f32, value: f32) -> vec3<f32> {
    let c = od_chroma(od_c);
    return scatter * (c - od_chroma(od_ref)) + film * (c + 0.15 * od_bright(od_c) * U3) +
           value * (od_bright(od_c) - od_bright(od_val_ref)) * U3;
}

// Lifts pass up to 0.15, then roll off towards 0.55
fn soft_lift(d: vec3<f32>) -> vec3<f32> {
    let k = vec3<f32>(0.15);
    let over = min(d + k, vec3<f32>(0.0)); // how far past the knee (<= 0)
    return max(d, -k) + (-0.4 * (1.0 - exp(over / 0.4)));
}

// Soft ellipse mask for region `idx`; 0 when the region is unused
fn region(p: vec2<f32>, idx: i32, feather: f32) -> f32 {
    let a = data(128 + idx * 2);
    let b = data(129 + idx * 2);
    if (a.z <= 0.0) {
        return 0.0;
    }
    let q = p - a.xy;
    let l = vec2<f32>(q.x * b.x + q.y * b.y, -q.x * b.y + q.y * b.x);
    let t = length(l / a.zw);
    return 1.0 - smoothstep(1.0 - feather, 1.0, t);
}

// Rec. 601 luma
fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.299, 0.587, 0.114));
}

// The makeup look's grade
fn grade(c: vec3<f32>) -> vec3<f32> {
    var g = c * exp2(u.grade0.x * 0.8); // brightness
    g *= vec3<f32>(1.0 + u.grade0.w * 0.10, 1.0 - u.grade1.x * 0.07, 1.0 - u.grade0.w * 0.10); // warmth / tint
    g.r *= 1.0 + u.grade1.x * 0.03;
    g.b *= 1.0 + u.grade1.x * 0.03;
    g = saturate(g);
    let s = g * g * (3.0 - 2.0 * g); // S-curve
    g = select(mix(g, vec3<f32>(0.5), -u.grade0.y * 0.35), mix(g, s, u.grade0.y), u.grade0.y >= 0.0);
    let y = luma(g);
    g = mix(vec3<f32>(y), g, 1.0 + u.grade0.z); // saturation
    g = g * (1.0 - u.grade1.y * 0.18) + u.grade1.y * 0.10; // fade (lifted blacks)
    return saturate(g);
}

// Eye-surround mask for dark circles
fn eye_surround(p: vec2<f32>, idx: i32) -> f32 {
    let a = data(128 + idx * 2);
    let b = data(129 + idx * 2);
    if (a.z <= 0.0) {
        return 0.0;
    }
    let q = p - a.xy;
    let l = vec2<f32>(q.x * b.x + q.y * b.y, -q.x * b.y + q.y * b.x) / a.zw;
    return (1.0 - smoothstep(0.35, 1.0, length(l))) * mix(0.6, 1.0, smoothstep(-0.45, -0.15, l.y));
}

// Region with its radii scaled
fn region_scaled(p: vec2<f32>, idx: i32, feather: f32, scale: f32) -> f32 {
    let a = data(128 + idx * 2);
    let b = data(129 + idx * 2);
    if (a.z <= 0.0) {
        return 0.0;
    }
    let q = p - a.xy;
    let l = vec2<f32>(q.x * b.x + q.y * b.y, -q.x * b.y + q.y * b.x);
    let t = length(l / (a.zw * scale));
    return 1.0 - smoothstep(1.0 - feather, 1.0, t);
}

// Inverse-mapped liquify, where to sample the source
fn warp(p: vec2<f32>) -> vec2<f32> {
    var src = p;
    let wb = u.warp_bounds;
    if (warp_count() <= 0 || p.x < wb.x || p.y < wb.y || p.x > wb.z || p.y > wb.w) {
        return src;
    }
    for (var i = 0; i < 64; i++) {
        if (i < warp_count()) {
            let a = data(i * 2); // cx, cy, r, type
            let b = data(i * 2 + 1); // vx, vy, s, -
            let q = p - a.xy;
            let t2 = dot(q, q) / (a.z * a.z);
            if (t2 < 1.0) {
                let f = (1.0 - t2) * (1.0 - t2);
                if (a.w < 0.5) {
                    src -= b.xy * f; // translate
                } else if (a.w < 1.5) {
                    src -= q * (b.z * f); // scale
                } else {
                    src -= b.xy * (dot(q, b.xy) * b.z * f); // directional scale
                }
            }
        }
    }
    return src;
}

// The main pass
@fragment
fn fs_main(v_in: V) -> @location(0) vec4<f32> {
    let sz = size();
    let p = v_in.uv * sz;
    let sp = warp(p);
    let uv = sp / sz;

    let orig = smp(image, uv);
    var col = orig.rgb;
    let sm = smp(smooth_tex, uv).rgb;
    let tone = smp(tone_tex, uv).rgb;

    var ycc = to_ycc(col);
    let ycc_orig = ycc;
    let tone_ycc = to_ycc(tone);

    // ---------- masks (evaluated in source space so they follow the warp) ----------
    let fw = face_weight();
    let face = region(sp, 0, 0.35) * fw;
    let eyes = max(region(sp, 1, 0.5), region(sp, 2, 0.5)) * fw;
    let brows = max(region(sp, 3, 0.5), region(sp, 4, 0.5)) * fw;
    let lips = region(sp, 5, 0.45) * fw;
    let mouth_in = region(sp, 6, 0.4) * fw;
    let ue_l = eye_surround(sp, 7);
    let ue_r = eye_surround(sp, 8);
    let under_eye = max(ue_l, ue_r) * fw;

    let sod = smp(skin_od_tex, uv);
    let has_sod = has_seg() > 0.5 && sod.a > 0.03;
    let od_low = sod.rgb / max(sod.a, 0.001);
    let tone_s = select(tone, lin2srgb(exp(-od_low)), has_sod); // average colour of nearby skin only
    let ts = to_ycc(tone_s);

    // Protected features from the exact landmark outlines (R lips, B eyes + mouth opening,
    // A brow band)
    var lips_p: f32;
    var eyes_p: f32;
    var eye_core: f32;
    var brow_hair: f32;
    if (has_prot() > 0.5) {
        let pr = smp(mask_tex, uv);
        lips_p = saturate(pr.r * 1.5) * fw;
        eyes_p = saturate(pr.b * 1.5) * fw;
        eye_core = smoothstep(0.55, 0.9, pr.b) * fw; // the eye opening itself, no soft rim
        brow_hair = saturate(pr.a * 1.5) * smoothstep(0.05, 0.14, ts.x - ycc_orig.x) * fw;
    } else {
        lips_p = lips;
        eyes_p = eyes;
        eye_core = eyes * 0.8;
        brow_hair = brows * smoothstep(0.05, 0.14, ts.x - ycc_orig.x);
    }
    let protect = max(max(lips_p, eyes_p), brow_hair); // smoothing keeps features crisp
    let protect_tone = max(eye_core, lips_p * 0.5); // tone reaches lids, brows, half the lips

    // Learned segmentation: R skin, G face skin, B hair, A person
    let sg = smp(seg_tex, uv);
    let seg_on = has_seg() > 0.5;
    let skin_all = select(face, smoothstep(0.12, 0.55, sg.r), seg_on);
    let skin_face = select(face, smoothstep(0.12, 0.55, sg.g), seg_on);
    let area = select(saturate(skin_face * 1.5), 1.0, all_skin() > 0.5);
    let not_hair = select(1.0, 1.0 - smoothstep(0.2, 0.7, sg.b), seg_on);
    let skin_base = saturate(skin_all * area) * not_hair;
    let skin = skin_base * (1.0 - protect); // smoothing / foundation / shadows
    let skin_tone_m = skin_base * (1.0 - protect_tone); // skin tone
    let skin_lo = skin_face * (1.0 - protect); // face skin: makeup placement

    // ---------- frequency separation ----------
    let px1 = 1.0 / sz;
    let box = (smp(image, uv + px1 * vec2<f32>(0.75, 0.75)).rgb +
               smp(image, uv + px1 * vec2<f32>(-0.75, 0.75)).rgb +
               smp(image, uv + px1 * vec2<f32>(0.75, -0.75)).rgb +
               smp(image, uv + px1 * vec2<f32>(-0.75, -0.75)).rgb) * 0.25;
    let fine = col - box;
    let mid = box - sm;

    let b = to_ycc(sm);
    let y0 = b.x;

    // Dark circles: the reference is the skin in a ring around each pixel
    var dc = 0.0;
    var od_ue_ref = vec3<f32>(0.0);
    if (pouch_amt() > 0.0 && under_eye > 0.0 && has_sod) {
        let ra4 = data(128 + select(8, 7, ue_l >= ue_r) * 2);
        let rr = vec2<f32>(ra4.w, ra4.w) * 0.8 / sz; // ring radius: most of the eye surround's depth
        var acc = vec4<f32>(0.0);
        for (var i = 0; i < 12; i++) {
            let an = f32(i) * 0.5235988;
            acc += smp(skin_od_tex, uv + vec2<f32>(cos(an), sin(an)) * rr);
        }
        if (acc.a > 0.3) {
            od_ue_ref = acc.rgb / acc.a;
            dc = under_eye * pouch_amt() * skin;
        }
    }

    // Smooth: drop the blemish band, keep most of the grain
    let sa = smooth_amt() * skin;
    var det = mid * (1.0 - 0.92 * sa) + fine * (1.0 - 0.30 * sa);
    det *= clamp(b.x / max(y0, 0.02), 0.35, 1.0); // darker skin: texture contrast scales down too
    col = saturate(to_rgb(b) + det);

    // ---------- pigments: Skin tone, Foundation (even tone + glow), Redness, Dark circles ----------
    let fnd = foundation_amt() * skin;
    let rda = redness_amt() * skin;
    if (skin_tone_amt() != 0.0 || fnd > 0.0 || rda > 0.0 || dc > 0.0) {
        var lin = srgb2lin(col);
        let od = -log(max(lin, vec3<f32>(0.001)));
        let pm = dot(pig_r0(), od);
        let ph = dot(pig_r1(), od);
        let mh_low = select(vec2<f32>(pm, ph), vec2<f32>(dot(pig_r0(), od_low), dot(pig_r1(), od_low)), has_sod);
        // shine (bright, colourless) is a surface reflection, not pigment: never re-tint it
        let ll = dot(lin, vec3<f32>(0.2126, 0.7152, 0.0722));
        let lsat = max(lin.r, max(lin.g, lin.b)) - min(lin.r, min(lin.g, lin.b));
        let keep = 1.0 - saturate((ll - 0.45) / 0.25) * saturate(1.0 - lsat / max(ll, 0.001) / 0.35);

        let mel = pig_mel();
        let hem = pig_hem();
        let mel_c = mel - dot(mel, U3) * U3; // colour-plane parts
        let hem_c = hem - dot(hem, U3) * U3;
        // Skin tone: +-0.7 OD of melanin
        var d_od = (-skin_tone_amt() * 0.7 * skin_tone_m) * mel;
        // Foundation, even tone
        d_od += (-(pm - mh_low.x) * 0.9 * fnd) * (mel_c + 0.35 * (mel - mel_c));
        d_od += (-(ph - mh_low.y) * 0.9 * fnd) * hem_c;
        // Redness, on the lightly denoised pixel
        let ph_dn = dot(pig_r1(), -log(max(srgb2lin(col - fine * (1.0 - 0.30 * sa)), vec3<f32>(0.001))));
        d_od += (-(max(ph_dn - mh_low.y, 0.0) * 0.95 + 0.04) * rda) * (hem_c + 0.5 * (hem - hem_c));
        // Dark circles: take the dip below the surrounding skin out
        if (dc > 0.0) {
            let mh_ref = vec2<f32>(dot(pig_r0(), od_ue_ref), dot(pig_r1(), od_ue_ref));
            let em = clamp(mh_low.x - mh_ref.x, 0.0, 0.8);
            let eh = clamp(mh_low.y - mh_ref.y, 0.0, 0.8);
            let pig_ex = em * mel + eh * hem;
            let shade_ex = clamp(od_bright(od_low - od_ue_ref) - od_bright(pig_ex), 0.0, 0.6);
            d_od -= (pig_ex + shade_ex * U3) * dc;
        }
        lin = exp(-(od + d_od * keep));
        // Foundation glow
        lin *= 1.0 + 0.18 * fnd;
        col = lin2srgb(lin);
    }
    ycc = to_ycc(col);

    // ---------- teeth ----------
    let satc = length(ycc.yz - 0.5);
    let toothy = smoothstep(0.30, 0.55, ycc.x) * (1.0 - smoothstep(0.10, 0.22, satc));
    let tm = mouth_in * toothy * teeth_amt();
    if (tm > 0.0) {
        let tl = srgb2lin(col);
        var tod = -log(max(tl, vec3<f32>(0.001)));
        tod.b -= max(tod.b - 0.5 * (tod.r + tod.g), 0.0) * 0.75 * tm;
        col = saturate(lin2srgb(exp(-tod) * (1.0 + 0.25 * tm)));
    }
    col = saturate(col);

    // ---------- makeup ----------
    if (makeup_amt() > 0.0) {
        let mk = smp(mask_tex, uv).rgb * fw;
        let amt = makeup_amt();

        let paint = select(vec4<f32>(0.0), smp(paint_tex, uv) * fw, has_paint() > 0.5);
        // (blush, eyeshadow) not on things much darker than the surrounding skin
        let on_skin = 1.0 - smoothstep(0.15, 0.30, ts.x - luma(col));
        // contour: a shadow in the user's own skin brown
        let cw = saturate(paint.r * skin_lo * u.makeup_extra.y * amt);
        if (cw > 0.0) {
            let lin = srgb2lin(col);
            let d = 0.48 * U3 + 0.93 * pig_mel();
            col = lin2srgb(lin * exp(-d * cw * shine_keep(lin)));
        }
        // highlight: light caught by the product
        let hm = saturate(paint.g * skin_lo * u.makeup_extra.z * amt);
        if (hm > 0.0) {
            let e = dot(srgb2lin(sm), vec3<f32>(0.2126, 0.7152, 0.0722));
            col = lin2srgb(srgb2lin(col) + vec3<f32>(1.0, 0.94, 0.85) * (0.65 * e * hm * hm * (3.0 - 2.0 * hm)));
        }

        // blush
        let blush = max(region_scaled(sp, 9, 1.0, 1.3), region_scaled(sp, 10, 1.0, 1.3)) * skin_lo *
                    u.blush_color.a * amt * fw * on_skin;
        if (blush > 0.0) {
            let lin = srgb2lin(col);
            let od = -log(max(lin, vec3<f32>(0.001)));
            let d = deposit(select(od, od_low, has_sod), od_of(u.blush_color.rgb), od, 0.65, 0.1855, 0.0);
            col = lin2srgb(exp(-(od + d * (1.05 * saturate(blush) * shine_keep(lin)))));
        }

        // eyeshadow (kept off the eyeball and the brows)
        let shm = max(region(sp, 11, 0.9), region(sp, 12, 0.9)) * (1.0 - saturate(mk.b * 1.5)) *
                  (1.0 - brows * 0.8) * u.shadow_color.a * amt * fw;
        let shw = saturate(shm * 1.3) * on_skin;
        if (shw > 0.0) {
            let lin = srgb2lin(col);
            let od = -log(max(lin, vec3<f32>(0.001)));
            var d = deposit(select(od_of(sm), od_low, has_sod), od_of(u.shadow_color.rgb),
                            od_of(vec3<f32>(0.69, 0.50, 0.42)), 0.80, 0.20, 0.55);
            d += max(-0.12 - od_bright(d), 0.0) * U3; // lift <= 0.12 OD
            d = soft_lift(d);
            col = lin2srgb(exp(-(od + d * (shw * shine_keep(lin)))));
        }

        // eyeliner: a dense dark film (OD) along the lash line
        let lw = paint.b * amt;
        if (lw > 0.0) {
            col = lin2srgb(srgb2lin(col) * exp(-3.2 * lw * vec3<f32>(1.0, 1.04, 1.06)));
        }

        // brow: powder in the brow's own shape
        let bw = paint.a * u.makeup_extra.w * amt;
        if (bw > 0.0) {
            let hair = saturate((tone_ycc.x - luma(sm)) * 5.0);
            let bod = od_of(col);
            col = lin2srgb(exp(-(bod * (1.0 + 0.5 * bw * hair) + bw * (1.0 - hair) * 0.55 * pig_mel())));
        }

        // lips: luminance-preserving tint + gloss on the lip's own highlights
        let ycl = to_ycc(col);
        let csat = length(ycl.yz - 0.5);
        let teeth_px = smoothstep(0.38, 0.55, ycl.x) * (1.0 - smoothstep(0.07, 0.16, csat));
        let cavity_px = 1.0 - smoothstep(0.10, 0.22, ycl.x);
        let open_zone = region(sp, 6, 0.6) * fw;
        var lip_mask = saturate(mk.r * 1.3) * (1.0 - open_zone * saturate(teeth_px + cavity_px));
        // only on the face (the segmenter labels lips as face skin)
        if (seg_on) {
            lip_mask *= smoothstep(0.15, 0.50, sg.g);
        }

        let lm = lip_mask * mix(0.75, 1.0, smoothstep(0.4, 0.95, mk.r)) * saturate(u.lip_color.a * 1.25) * amt;
        let ly = ycl.x;
        if (lm > 0.0) {
            let lin = srgb2lin(col);
            let od = -log(max(lin, vec3<f32>(0.001)));
            var d = deposit(od_of(sm), od_of(u.lip_color.rgb), od_of(vec3<f32>(0.61, 0.34, 0.33)), 0.90, 0.15, 0.65);
            d = soft_lift(d);
            col = lin2srgb(exp(-(od + d * (lm * mix(0.4, 1.0, shine_keep(lin))))));
        }
        let spec = saturate((ly - luma(tone) - 0.02) * 5.0) * lip_gloss() * lip_mask * amt;
        col += spec * 0.40 * vec3<f32>(1.0, 0.97, 0.97);
    }

    // ---------- bright eyes + clarity (unsharp mask from a small box blur) ----------
    let t = 1.0 / sz;
    let blur = (smp(image, uv + t * vec2<f32>(1.5, 1.5)).rgb +
                smp(image, uv + t * vec2<f32>(-1.5, 1.5)).rgb +
                smp(image, uv + t * vec2<f32>(1.5, -1.5)).rgb +
                smp(image, uv + t * vec2<f32>(-1.5, -1.5)).rgb) * 0.25;
    let detail = orig.rgb - blur;
    let el = eyes * eyelight_amt();
    col += detail * el * 1.6;
    col *= 1.0 + el * 0.10;

    // Contrast: local contrast against the 1/8-res blur, plus some crispness; luma only
    let cm = clarity_amt() * max(face, 0.5 * all_skin() * skin_base);
    let yl = luma(col);
    col += (luma(tone) - yl) * (-0.7 * cm);
    col += luma(detail) * cm * (1.0 - smooth_amt() * skin * 0.5);

    col = saturate(col);

    let dv = debug_view();
    if (dv == 3) {
        let mk = smp(mask_tex, uv).rgb;
        let pt = select(vec4<f32>(0.0), smp(paint_tex, uv), has_paint() > 0.5);
        let bl = max(region(sp, 9, 1.0), region(sp, 10, 1.0));
        let shd = max(region(sp, 11, 0.9), region(sp, 12, 0.9));
        col = luma(orig.rgb) * 0.35 + vec3<f32>(mk.r + bl * 0.6, pt.g * 0.6, shd * 0.6 + pt.b) +
              vec3<f32>(pt.r, pt.r * 0.5, 0.0) * 0.6 + vec3<f32>(pt.a) * 0.3;
    } else if (dv == 1) {
        col = vec3<f32>(skin);
    } else if (dv == 2) {
        col = col * 0.6 + vec3<f32>(face, 0.0, 0.0) * 0.25 + vec3<f32>(0.0, eyes + brows, 0.0) * 0.4 +
              vec3<f32>(0.0, 0.0, lips + mouth_in) * 0.5 + vec3<f32>(under_eye, under_eye, 0.0) * 0.4;
    }

    return vec4<f32>(col, orig.a);
}

// The Finish pass: light + the look's colour grade, after everything else
@fragment
fn fs_finish(v_in: V) -> @location(0) vec4<f32> {
    let c = smp(image, v_in.uv);
    if (debug_view() != 0) {
        return c;
    }
    var col = c.rgb;
    let sz = size();
    let p = v_in.uv * sz;
    let sp = warp(p);
    let uv = sp / sz;
    let fw = face_weight();

    // ---------- light: whole-body virtual lighting ----------
    if (light_amt() > 0.0) {
        let la = light_amt();
        let L = normalize(u.light_dir.xyz);

        var person: f32;
        var n = vec3<f32>(0.0, 0.0, 1.0);
        var n_body = n; // the silhouette alone: the rim light belongs on the outline
        if (has_person() > 0.5) {
            person = smp(seg_tex, uv).a;
            let o = person_texel() * 3.0;
            let hx = smp(tone_tex, uv + vec2<f32>(o.x, 0.0)).a - smp(tone_tex, uv - vec2<f32>(o.x, 0.0)).a;
            let hy = smp(tone_tex, uv + vec2<f32>(0.0, o.y)).a - smp(tone_tex, uv - vec2<f32>(0.0, o.y)).a;
            n = normalize(vec3<f32>(-hx * 3.5, -hy * 3.5, 0.55));
            n_body = n;
        } else {
            person = max(region(sp, 13, 0.55), region(sp, 14, 0.6)) * fw;
        }

        // the body's silhouette, then the head (an ellipsoid turned with the head)
        let head = head_normal(sp);
        let nh = head.n;
        let hw = head.w * fw;
        let base_face = normalize(mix(n, nh, hw)); // the shape the face sits on, right here
        if (hw > 0.0) {
            n = normalize(mix(n, normalize(nh + vec3<f32>(0.0, 0.0, 0.35)), hw)); // softened, like skin
        }

        if (has_facenorm() > 0.5) {
            // the face mesh adds only its detail on top of that base
            let fnm = smp(facenorm_tex, uv);
            let cov = fnm.a * fw;
            let nc = fnm.rgb * 2.0 - 1.0;
            let nf = normalize(u.face_rx.xyz * nc.x + u.face_ry.xyz * nc.y + u.face_rz.xyz * nc.z);
            n = normalize(n + (nf - base_face) * (0.75 * cov));
            person = max(person, cov);
        } else if (fw > 0.0) {
            // no mesh: an ellipsoid fitted to the landmarks
            let fa = data(128); // face ellipse: centre, radii
            let fb = data(129); // (cos, sin) of head roll
            let q = sp - fa.xy;
            var l = vec2<f32>(q.x * fb.x + q.y * fb.y, -q.x * fb.y + q.y * fb.x) / max(fa.zw, vec2<f32>(1.0));
            l.y *= 0.92;
            let r = length(l);
            let face_m = (1.0 - smoothstep(0.75, 1.2, r)) * fw;
            let rn = min(r, 1.0);
            let ln = l / max(r, 1.0);
            var nf = normalize(vec3<f32>(ln.x, ln.y, sqrt(saturate(1.0 - rn * rn)) * 1.15 + 0.08));
            nf = vec3<f32>(nf.x * fb.x - nf.y * fb.y, nf.x * fb.y + nf.y * fb.x, nf.z);
            n = normalize(mix(n, nf, face_m));
            person = max(person, face_m);
        }

        // wrapped: light falls off gently round the form
        let lam = saturate((dot(n, L) + 0.35) / 1.35);
        let key = 1.0 + u.light_key.rgb * (lam * lam) * u.light_key.a * 0.9 * person * la;
        let shade = 1.0 - u.light_misc.x * (1.0 - lam) * person * la * 0.7;
        col = col * key * shade;

        // rim light: a band along the outline on the side away from the key
        let rim_side = normalize(vec2<f32>(-L.x, L.y * 0.5) + vec2<f32>(0.0001, 0.0));
        let outline = select(1.0, 1.0 - smoothstep(0.55, 0.85, smp(tone_tex, uv).a), has_person() > 0.5);
        let edge = powp(saturate(1.0 - n_body.z), 1.6) * outline;
        let rim_dir = saturate(dot(normalize(n_body.xy + 0.0001), rim_side) * 0.8 + 0.3);
        col += u.light_rim.rgb * edge * rim_dir * person * u.light_rim.a * 0.7 * la;

        // background: dimmed outside the real silhouette
        col *= 1.0 - u.light_misc.y * (1.0 - person) * la;
        col *= exp2(u.light_misc.z * la * 0.6);
        col.r *= 1.0 + u.light_misc.w * 0.10 * la;
        col.b *= 1.0 - u.light_misc.w * 0.10 * la;
        col = saturate(col);
    }

    // ---------- the look's colour grade on the whole frame ----------
    if (filter_amt() > 0.0) {
        col = mix(col, grade(col), filter_amt());
    }
    return vec4<f32>(col, c.a);
}
