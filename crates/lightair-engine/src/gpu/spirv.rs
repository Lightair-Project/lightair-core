//! The two shaders of Android's zero-copy camera import, built as SPIR-V here: a combined
//! image sampler with an immutable Y′CbCr conversion can't be written in WGSL, and no shader
//! compiler is needed at build time. The vertex shader covers the target with one triangle and
//! turns its coordinates into the camera picture's (a push constant: `m` a 2×2 matrix, `o` an
//! offset, for the rotation and mirroring); the fragment shader samples the picture through the
//! conversion (the result is the sRGB-encoded colour, as the other feeds give).

use rspirv::dr::{Builder, Operand};
use rspirv::spirv::{
    AddressingModel, BuiltIn, Capability, Decoration, Dim, ExecutionMode, ExecutionModel,
    FunctionControl, ImageFormat, MemoryModel, StorageClass,
};

fn module(b: Builder) -> Vec<u32> {
    use rspirv::binary::Assemble;
    b.module().assemble()
}

fn start() -> Builder {
    let mut b = Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    b
}

/// The vertex shader: `uv = m · p + o` for `p` the corner (0–1), out at location 0.
pub fn vertex() -> Vec<u32> {
    let mut b = start();
    let void = b.type_void();
    let f32_ = b.type_float(32, None);
    let i32_ = b.type_int(32, 1);
    let v2 = b.type_vector(f32_, 2);
    let v4 = b.type_vector(f32_, 4);
    let fn_ty = b.type_function(void, vec![]);
    let c0 = b.constant_bit32(i32_, 0);
    let c1 = b.constant_bit32(i32_, 1);
    let c2 = b.constant_bit32(i32_, 2);
    let one = b.constant_bit32(f32_, 1.0f32.to_bits());
    let two = b.constant_bit32(f32_, 2.0f32.to_bits());
    let zero = b.constant_bit32(f32_, 0.0f32.to_bits());
    // inputs and outputs
    let in_i32 = b.type_pointer(None, StorageClass::Input, i32_);
    let vertex_index = b.variable(in_i32, None, StorageClass::Input, None);
    b.decorate(
        vertex_index,
        Decoration::BuiltIn,
        [Operand::BuiltIn(BuiltIn::VertexIndex)],
    );
    let out_v4 = b.type_pointer(None, StorageClass::Output, v4);
    let position = b.variable(out_v4, None, StorageClass::Output, None);
    b.decorate(
        position,
        Decoration::BuiltIn,
        [Operand::BuiltIn(BuiltIn::Position)],
    );
    let out_v2 = b.type_pointer(None, StorageClass::Output, v2);
    let uv_out = b.variable(out_v2, None, StorageClass::Output, None);
    b.decorate(uv_out, Decoration::Location, [Operand::LiteralBit32(0)]);
    // push constants: { vec4 m; vec4 o; }
    let block = b.type_struct([v4, v4]);
    b.decorate(block, Decoration::Block, []);
    b.member_decorate(block, 0, Decoration::Offset, [Operand::LiteralBit32(0)]);
    b.member_decorate(block, 1, Decoration::Offset, [Operand::LiteralBit32(16)]);
    let pc_ptr = b.type_pointer(None, StorageClass::PushConstant, block);
    let pc = b.variable(pc_ptr, None, StorageClass::PushConstant, None);
    let pc_v4 = b.type_pointer(None, StorageClass::PushConstant, v4);

    let main = b
        .begin_function(void, None, FunctionControl::NONE, fn_ty)
        .expect("a function");
    b.begin_block(None).expect("a block");
    let vi = b.load(i32_, None, vertex_index, None, []).expect("load");
    // x = float((vi << 1) & 2), y = float(vi & 2)
    let shifted = b.shift_left_logical(i32_, None, vi, c1).expect("shift");
    let xi = b.bitwise_and(i32_, None, shifted, c2).expect("and");
    let yi = b.bitwise_and(i32_, None, vi, c2).expect("and");
    let x = b.convert_s_to_f(f32_, None, xi).expect("convert");
    let y = b.convert_s_to_f(f32_, None, yi).expect("convert");
    // position = (x·2 − 1, y·2 − 1, 0, 1)
    let x2 = b.f_mul(f32_, None, x, two).expect("mul");
    let y2 = b.f_mul(f32_, None, y, two).expect("mul");
    let px = b.f_sub(f32_, None, x2, one).expect("sub");
    let py = b.f_sub(f32_, None, y2, one).expect("sub");
    let pos = b
        .composite_construct(v4, None, [px, py, zero, one])
        .expect("construct");
    b.store(position, pos, None, []).expect("store");
    // uv = (m.x·x + m.y·y + o.x, m.z·x + m.w·y + o.y)
    let m_ptr = b.access_chain(pc_v4, None, pc, [c0]).expect("chain");
    let o_ptr = b.access_chain(pc_v4, None, pc, [c1]).expect("chain");
    let m = b.load(v4, None, m_ptr, None, []).expect("load");
    let o = b.load(v4, None, o_ptr, None, []).expect("load");
    let part = |b: &mut Builder, i: u32| b.composite_extract(f32_, None, m, [i]).expect("extract");
    let (mx, my, mz, mw) = (
        part(&mut b, 0),
        part(&mut b, 1),
        part(&mut b, 2),
        part(&mut b, 3),
    );
    let ox = b.composite_extract(f32_, None, o, [0]).expect("extract");
    let oy = b.composite_extract(f32_, None, o, [1]).expect("extract");
    let ax = b.f_mul(f32_, None, mx, x).expect("mul");
    let bx = b.f_mul(f32_, None, my, y).expect("mul");
    let sx = b.f_add(f32_, None, ax, bx).expect("add");
    let u = b.f_add(f32_, None, sx, ox).expect("add");
    let ay = b.f_mul(f32_, None, mz, x).expect("mul");
    let by = b.f_mul(f32_, None, mw, y).expect("mul");
    let sy = b.f_add(f32_, None, ay, by).expect("add");
    let v = b.f_add(f32_, None, sy, oy).expect("add");
    let uv = b.composite_construct(v2, None, [u, v]).expect("construct");
    b.store(uv_out, uv, None, []).expect("store");
    b.ret().expect("return");
    b.end_function().expect("end");
    b.entry_point(
        ExecutionModel::Vertex,
        main,
        "main",
        [vertex_index, position, uv_out],
    );
    module(b)
}

/// The fragment shader: the picture through the combined image sampler at set 0, binding 0,
/// opaque (a Y′CbCr sample's alpha isn't 1 on every driver: Mali gave about half).
pub fn fragment() -> Vec<u32> {
    let mut b = start();
    let void = b.type_void();
    let f32_ = b.type_float(32, None);
    let v2 = b.type_vector(f32_, 2);
    let v4 = b.type_vector(f32_, 4);
    let one = b.constant_bit32(f32_, 1.0f32.to_bits());
    let fn_ty = b.type_function(void, vec![]);
    let image = b.type_image(f32_, Dim::Dim2D, 0, 0, 0, 1, ImageFormat::Unknown, None);
    let sampled = b.type_sampled_image(image);
    let sampled_ptr = b.type_pointer(None, StorageClass::UniformConstant, sampled);
    let picture = b.variable(sampled_ptr, None, StorageClass::UniformConstant, None);
    b.decorate(
        picture,
        Decoration::DescriptorSet,
        [Operand::LiteralBit32(0)],
    );
    b.decorate(picture, Decoration::Binding, [Operand::LiteralBit32(0)]);
    let in_v2 = b.type_pointer(None, StorageClass::Input, v2);
    let uv_in = b.variable(in_v2, None, StorageClass::Input, None);
    b.decorate(uv_in, Decoration::Location, [Operand::LiteralBit32(0)]);
    let out_v4 = b.type_pointer(None, StorageClass::Output, v4);
    let colour = b.variable(out_v4, None, StorageClass::Output, None);
    b.decorate(colour, Decoration::Location, [Operand::LiteralBit32(0)]);

    let main = b
        .begin_function(void, None, FunctionControl::NONE, fn_ty)
        .expect("a function");
    b.begin_block(None).expect("a block");
    let uv = b.load(v2, None, uv_in, None, []).expect("load");
    let s = b.load(sampled, None, picture, None, []).expect("load");
    let c = b
        .image_sample_implicit_lod(v4, None, s, uv, None, [])
        .expect("sample");
    let opaque = b.composite_insert(v4, None, one, c, [3]).expect("alpha");
    b.store(colour, opaque, None, []).expect("store");
    b.ret().expect("return");
    b.end_function().expect("end");
    b.entry_point(ExecutionModel::Fragment, main, "main", [uv_in, colour]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, []);
    module(b)
}

/// The push constant for a picture `rotation` clockwise degrees from upright, `mirrored` after
/// turning (as `push_yuv`'s): where each upright point is in the picture as captured.
pub fn transform(rotation: u32, mirrored: bool) -> [f32; 8] {
    // as gpu/yuv.wgsl: mirror first, then the quarter turns
    let (m, o): ([f32; 4], [f32; 2]) = match rotation % 360 / 90 {
        1 => ([0.0, 1.0, -1.0, 0.0], [0.0, 1.0]),
        2 => ([-1.0, 0.0, 0.0, -1.0], [1.0, 1.0]),
        3 => ([0.0, -1.0, 1.0, 0.0], [1.0, 0.0]),
        _ => ([1.0, 0.0, 0.0, 1.0], [0.0, 0.0]),
    };
    if !mirrored {
        return [m[0], m[1], m[2], m[3], o[0], o[1], 0.0, 0.0];
    }
    // s = M·(F·u + f) + o with F = diag(−1, 1), f = (1, 0)
    [-m[0], m[1], -m[2], m[3], m[0] + o[0], m[2] + o[1], 0.0, 0.0]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parses(words: &[u32]) {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let module =
            naga::front::spv::parse_u8_slice(&bytes, &Default::default()).expect("valid SPIR-V");
        naga::valid::Validator::new(Default::default(), naga::valid::Capabilities::all())
            .validate(&module)
            .expect("a valid module");
    }

    #[test]
    fn both_shaders_are_valid_spirv() {
        parses(&vertex());
        // naga reads no combined image-sampler variables (what GLSL's `sampler2D` is, and what a
        // Y′CbCr conversion needs): the fragment shader is checked as a module only
        let mut l = rspirv::dr::Loader::new();
        rspirv::binary::parse_words(fragment(), &mut l).expect("a SPIR-V module");
        let m = l.module();
        assert_eq!(m.entry_points.len(), 1);
        assert!(
            m.types_global_values
                .iter()
                .any(|i| i.class.opcode == rspirv::spirv::Op::TypeSampledImage)
        );
    }

    #[test]
    fn the_transform_matches_the_yuv_shaders() {
        let at =
            |t: [f32; 8], x: f32, y: f32| (t[0] * x + t[1] * y + t[4], t[2] * x + t[3] * y + t[5]);
        // upright top-left comes from…
        assert_eq!(at(transform(0, false), 0.0, 0.0), (0.0, 0.0));
        assert_eq!(at(transform(90, false), 0.0, 0.0), (0.0, 1.0));
        assert_eq!(at(transform(180, false), 0.0, 0.0), (1.0, 1.0));
        assert_eq!(at(transform(270, false), 0.0, 0.0), (1.0, 0.0));
        // mirrored: x flipped before turning
        assert_eq!(at(transform(0, true), 0.0, 0.0), (1.0, 0.0));
        assert_eq!(at(transform(90, true), 0.0, 0.0), (0.0, 0.0));
        assert_eq!(at(transform(90, true), 1.0, 0.0), (0.0, 1.0));
    }
}
