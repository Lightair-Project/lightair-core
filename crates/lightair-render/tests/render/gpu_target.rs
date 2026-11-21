//! Frames kept on the GPU and pictures given as textures (a phone's Studio draws this way): the
//! same pixels as bytes in and a read-back out. Needs a GPU.

use std::collections::HashMap;
use std::sync::Arc;

use lightair_model::{Canvas, Id, Layout, Object, Scene, Source, Workspace};
use lightair_render::compose::read_texture;
use lightair_render::media::Picture;
use lightair_render::scene::{Pictures, SceneRenderer, Shot};
use serde_json::json;

const W: u32 = 160;
const H: u32 = 90;

/// A 64×64 picture: red left half, blue right half; as bytes, or as a texture made from them.
fn split() -> Vec<u8> {
    (0..64 * 64).flat_map(|i| if i % 64 < 32 { [255, 0, 0, 255] } else { [0, 0, 255, 255] }).collect()
}

struct Bytes;

impl Pictures for Bytes {
    fn picture(&mut self, _: &Source) -> Option<Shot> {
        Some(Shot::rgba(split(), (64, 64)))
    }
}

struct Textures(wgpu::Texture);

impl Pictures for Textures {
    fn picture(&mut self, _: &Source) -> Option<Shot> {
        Some(Shot { picture: Picture::Texture(self.0.clone()), size: (64, 64), premultiplied: false })
    }
}

fn texture() -> Option<wgpu::Texture> {
    let gpu = lightair_render::shared_gpu().ok()?;
    let size = wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 };
    let t = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        t.as_image_copy(),
        &split(),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(64 * 4), rows_per_image: Some(64) },
        size,
    );
    Some(t)
}

fn workspace() -> (Workspace, Id, Id) {
    let mut ws = Workspace::new("Test");
    ws.canvases = vec![Canvas { id: Id::new(), name: "Small".into(), width: W, height: H, fps_num: 30, fps_den: 1 }];
    let canvas = ws.canvases[0].id;
    let source = Source {
        id: Id::new(),
        name: "split".into(),
        kind: "image_source".into(),
        settings: json!({}),
        effects: vec![],
    };
    let mut scene = Scene::new("Scene");
    // turned a little and see-through, so sampling and blending are both compared
    let mut layout = Layout::boxed(20.0, 10.0, 120.0, 70.0);
    layout.rotation = 10.0;
    layout.opacity = 0.8;
    scene.objects = vec![Object::new("split", source.id, [(canvas, layout)])];
    let id = scene.id;
    ws.sources = HashMap::from([(source.id, Arc::new(source))]).into_iter().collect();
    ws.scenes.push(Arc::new(scene));
    (ws, id, canvas)
}

fn close(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (*x as i32 - *y as i32).abs() <= 2)
}

#[test]
fn a_texture_picture_draws_as_its_bytes_do() {
    let (Ok(mut r), Some(t)) = (SceneRenderer::new((W, H)), texture()) else {
        eprintln!("no GPU here; skipping");
        return;
    };
    let (ws, scene, canvas) = workspace();
    let from_bytes = r.frame(&ws, scene, canvas, &mut Bytes, 0.0);
    let from_texture = r.frame(&ws, scene, canvas, &mut Textures(t.clone()), 0.0);
    assert!(close(&from_bytes, &from_texture));
    // and the texture is the caller's still: bytes drawn after it don't land in it
    let _ = r.frame(&ws, scene, canvas, &mut Bytes, 0.0);
    assert_eq!(read_texture(&t).unwrap(), split());
}

#[test]
fn a_frame_kept_on_the_gpu_is_the_frame_read_back() {
    let Ok(mut r) = SceneRenderer::new((W, H)) else {
        eprintln!("no GPU here; skipping");
        return;
    };
    let (ws, scene, canvas) = workspace();
    let read = r.frame(&ws, scene, canvas, &mut Bytes, 0.0);
    r.keep_on_gpu(true);
    assert!(r.frame(&ws, scene, canvas, &mut Bytes, 0.0).is_empty());
    let target = r.target();
    assert!(target.usage().contains(wgpu::TextureUsages::TEXTURE_BINDING));
    assert_eq!((target.width(), target.height()), (W, H));
    assert!(close(&read_texture(target).unwrap(), &read));
    r.keep_on_gpu(false);
    assert!(close(&r.frame(&ws, scene, canvas, &mut Bytes, 0.0), &read));
}
