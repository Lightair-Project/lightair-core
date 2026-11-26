//! Each source's picture now, for lightair-render's scene renderer:
//! camera and media feeds from their rings, images and GIFs decoded once and kept as textures,
//! text, colours and timers made and kept while they're the same.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lightair_model::Source;
use lightair_render::media::Picture;
use lightair_render::scene::{Pictures, Shot};
use serde_json::Value;

use crate::feeds::{Feed, Picture as Pushed};
use crate::gpu::Gpu;
use crate::hot::{Hot, Program};
use crate::templates;

/// The source kinds Studio draws.
pub fn drawn(kind: &str) -> bool {
    matches!(
        kind,
        templates::CAMERA
            | templates::SCREEN
            | templates::MEDIA
            | templates::IMAGE
            | templates::TEXT
            | templates::COLOUR
            | templates::TIMER
    ) || kind == "lightair_camera_capture"
        || kind == crate::widgets::KIND
}

struct Animated {
    /// Each frame and when it ends (ms from the start).
    frames: Vec<(wgpu::Texture, u32)>,
    size: (u32, u32),
}

/// An image or GIF: being decoded, ready, or unreadable.
enum Slot {
    Loading(std::sync::mpsc::Receiver<Option<Animated>>),
    Ready(Arc<Animated>),
    Broken,
}

#[derive(Default)]
pub struct Cache {
    images: HashMap<PathBuf, Slot>,
    made: HashMap<String, (wgpu::Texture, (u32, u32), bool)>,
    /// A YUV feed's last converted frame: (serial, texture).
    converted: HashMap<u32, (u64, wgpu::Texture)>,
    /// Made pictures used this frame (the rest are dropped after it).
    used: HashSet<String>,
    /// Background effects: the passes, and each camera feed's textures.
    background: Option<crate::background::Pipelines>,
    backgrounds: HashMap<u32, crate::background::State>,
    /// Sharpen and the keyers: the pass, and each source's targets.
    keying: Option<crate::keying::Pipeline>,
    keyed: crate::keying::Targets,
    key_masks: crate::keying::Masks,
    /// The looks (beauty, light, grade): the passes, and each camera feed's tracker.
    looks: Option<crate::looks::render::Looks>,
    trackers: HashMap<u32, LookState>,
    /// The workspace whose sources the per-source textures were last kept for.
    sources_of: usize,
}

impl Cache {
    /// Drops made pictures no frame used lately (text that changed).
    pub fn end_frame(&mut self) {
        if self.made.len() > 64 {
            let used = std::mem::take(&mut self.used);
            self.made.retain(|k, _| used.contains(k));
        } else {
            self.used.clear();
        }
    }

    /// Whether an image is still being decoded.
    pub fn loading(&self) -> bool {
        self.images.values().any(|s| matches!(s, Slot::Loading(_)))
    }

    /// A feed that's gone (a camera stopped for another, a screen or media layer removed):
    /// its textures go with it. Each camera start is a new feed, so over a show they'd pile up
    /// (a background effect keeps camera-sized textures, a look its tracker).
    pub fn forget_feed(&mut self, feed: u32) {
        self.converted.remove(&feed);
        self.backgrounds.remove(&feed);
        self.trackers.remove(&feed);
    }

    /// The per-source textures (keyers, their masks, looks) kept only for `ws`'s sources: a
    /// layer deleted during a show takes its canvas-sized targets with it. Checked when the
    /// workspace changes.
    pub fn keep_sources(&mut self, ws: &Arc<lightair_model::Workspace>) {
        let at = Arc::as_ptr(ws) as usize;
        if at == self.sources_of {
            return;
        }
        self.sources_of = at;
        let keep: Vec<lightair_model::Id> = ws.sources.keys().copied().collect();
        // decoded images no source or effect names any more (an image layer deleted, a picture
        // swapped): a long show that changes pictures would otherwise keep every one in memory
        let mut named: Vec<&str> = Vec::new();
        for src in ws.sources.values() {
            strings(&src.settings, &mut named);
            for e in &src.effects {
                strings(&e.settings, &mut named);
            }
        }
        self.images
            .retain(|path, _| named.iter().any(|n| !n.is_empty() && (path.ends_with(n) || path == Path::new(n))));
        self.keyed.forget_others(&keep);
        self.key_masks.forget_others(&keep);
        if let Some(l) = self.looks.as_mut() {
            l.forget_others(&keep);
        }
    }
}

/// Every string in a settings value (where file names are), depth first.
fn strings<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
        Value::Object(m) => m.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

/// One frame's pictures.
pub struct Frame<'a> {
    pub gpu: &'a Gpu,
    pub cache: &'a mut Cache,
    pub hot: &'a Hot,
    pub program: &'a Program,
    /// The frame's due time on the host clock (feeds' frames are picked at or before it).
    pub host_ns: i64,
    /// The frame's program time.
    pub pts_ns: i64,
    /// The feeds' frames this frame drew, one per feed (what a capture is matched by).
    pub used: Vec<SourceFrame>,
}

/// A feed's frame a program frame drew: the time and serial it was pushed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFrame {
    pub feed: u32,
    /// The host time the host pushed it with (its capture time).
    pub host_ns: i64,
    /// Its number among the feed's frames, from 1.
    pub serial: u64,
}

impl Frame<'_> {
    fn feed_picture(&mut self, feed: u32) -> Option<Shot> {
        let f = self.hot.feed(feed)?;
        let Feed::Video(v) = &*f else { return None };
        let (picture, serial, host_ns) = v.at(self.host_ns)?;
        if !self.used.iter().any(|u| u.feed == feed) {
            self.used.push(SourceFrame { feed, host_ns, serial });
        }
        match &*picture {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Pushed::Buffer(b, _) => {
                Some(Shot { picture: Picture::Buffer(b.clone()), size: b.size(), premultiplied: false })
            }
            #[cfg(target_os = "android")]
            Pushed::Hardware { held, size, rotation, mirrored } => {
                let upright = if rotation % 180 == 90 { (size.1, size.0) } else { *size };
                let old = self.cache.converted.remove(&feed);
                let t = match old {
                    Some((s, t)) if s == serial => t,
                    old => {
                        let target =
                            old.map(|(_, t)| t).filter(|t| (t.width(), t.height()) == upright).unwrap_or_else(|| {
                                let t = self.gpu.target(upright, wgpu::TextureFormat::Rgba8Unorm);
                                self.gpu.prime(&t);
                                t
                            });
                        let drawn = self.gpu.importer.as_ref().is_some_and(|i| {
                            i.lock().draw(
                                self.gpu.device(),
                                self.gpu.queue(),
                                held.clone(),
                                *rotation,
                                *mirrored,
                                &target,
                            )
                        });
                        if !drawn {
                            return None;
                        }
                        target
                    }
                };
                self.cache.converted.insert(feed, (serial, t.clone()));
                Some(Shot { picture: Picture::Texture(t), size: upright, premultiplied: false })
            }
            Pushed::Rgba(bytes, size) => {
                let t = match self.cache.converted.remove(&feed) {
                    Some((s, t)) if s == serial => t,
                    // the same size: written into again, not made anew each frame
                    Some((_, t)) if (t.width(), t.height()) == *size => {
                        self.gpu.write(&t, bytes);
                        t
                    }
                    _ => self.gpu.texture(bytes, *size),
                };
                self.cache.converted.insert(feed, (serial, t.clone()));
                Some(Shot { picture: Picture::Texture(t), size: *size, premultiplied: false })
            }
            Pushed::Texture(t) => {
                Some(Shot { picture: Picture::Texture(t.clone()), size: (t.width(), t.height()), premultiplied: false })
            }
            Pushed::Yuv(y) => {
                let t = match self.cache.converted.remove(&feed) {
                    Some((s, t)) if s == serial => t,
                    old => self.gpu.yuv_to_rgba(y, old.map(|(_, t)| t)),
                };
                let size = (t.width(), t.height());
                self.cache.converted.insert(feed, (serial, t.clone()));
                Some(Shot { picture: Picture::Texture(t), size, premultiplied: false })
            }
        }
    }

    /// A camera with its background effect: masks asked of the host while it's drawn;
    /// as it is until the first one comes. `shot` is the camera's picture so far: drawn on when
    /// it's a texture already (the looks drew it), else the feed's own picture is sampled.
    fn background(&mut self, feed: u32, shot: Shot, look: &crate::background::Look) -> Shot {
        let Some(f) = self.hot.feed(feed) else {
            return shot;
        };
        let Feed::Video(v) = &*f else { return shot };
        v.want_mask(look.quality, crate::clock::host_now_ns());
        let Some(mask) = v.mask.lock().clone() else {
            return shot;
        };
        // the camera as a texture to sample
        let Some((picture, _, _)) = v.at(self.host_ns) else {
            return shot;
        };
        let (camera, _kept) = match (&shot.picture, &*picture) {
            (Picture::Texture(t), _) => (t.clone(), None::<Box<dyn std::any::Any>>),
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            (_, Pushed::Buffer(_, k)) => match crate::gpu::apple::sampled(self.gpu.device(), &k.0) {
                Some(t) => (t, Some(Box::new(k.clone()) as Box<dyn std::any::Any>)),
                None => return shot,
            },
            _ => return shot,
        };
        let image = if look.mode == 4 {
            look.image.clone().and_then(|file| self.image(&file)).and_then(|s| match s.picture {
                Picture::Texture(t) => Some(t),
                _ => None,
            })
        } else {
            None
        };
        let gpu = self.gpu;
        let pipelines = self.cache.background.get_or_insert_with(|| crate::background::Pipelines::new(gpu.device()));
        let state = self.cache.backgrounds.entry(feed).or_default();
        let out = crate::background::draw(gpu, pipelines, state, &camera, &mask, image.as_ref(), look);
        Shot { picture: Picture::Texture(out), size: shot.size, premultiplied: false }
    }

    fn path(&self, s: &str) -> PathBuf {
        let p = Path::new(s);
        if p.is_absolute() { p.to_path_buf() } else { self.program.folder.join(p) }
    }

    fn image(&mut self, file: &str) -> Option<Shot> {
        let path = self.path(file);
        // decoded on a thread of its own (a big GIF takes seconds): not drawn until it's there
        let img = match self.cache.images.get_mut(&path) {
            Some(Slot::Ready(i)) => i.clone(),
            Some(Slot::Broken) => return None,
            Some(Slot::Loading(rx)) => match rx.try_recv() {
                Ok(Some(i)) => {
                    let i = Arc::new(i);
                    self.cache.images.insert(path.clone(), Slot::Ready(i.clone()));
                    i
                }
                Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    tracing::warn!(path = %path.display(), "an image that can't be read");
                    self.cache.images.insert(path, Slot::Broken);
                    return None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            },
            None => {
                let (tx, rx) = std::sync::mpsc::channel();
                let p = path.clone();
                let started = std::thread::Builder::new()
                    .name("lightair-image".into())
                    .spawn(move || {
                        let _ = tx.send(decode(&p));
                    })
                    .is_ok();
                self.cache.images.insert(path, if started { Slot::Loading(rx) } else { Slot::Broken });
                return None;
            }
        };
        let total = img.frames.last().map_or(0, |f| f.1).max(1);
        let at = ((self.pts_ns / 1_000_000).max(0) as u64 % total as u64) as u32;
        let frame = img.frames.iter().find(|f| at < f.1).unwrap_or(&img.frames[0]);
        Some(Shot { picture: Picture::Texture(frame.0.clone()), size: img.size, premultiplied: false })
    }

    fn made(&mut self, key: String, make: impl FnOnce() -> Option<(Vec<u8>, (u32, u32), bool)>) -> Option<Shot> {
        if !self.cache.made.contains_key(&key) {
            let (rgba, size, pre) = make()?;
            if size.0 == 0 || size.1 == 0 {
                return None;
            }
            let t = self.gpu.texture(&rgba, size);
            self.cache.made.insert(key.clone(), (t, size, pre));
        }
        self.cache.used.insert(key.clone());
        let (t, size, pre) = self.cache.made.get(&key)?;
        Some(Shot { picture: Picture::Texture(t.clone()), size: *size, premultiplied: *pre })
    }

    fn text(&mut self, s: &Value, text: String) -> Option<Shot> {
        let style = text_style(s, text);
        let key = format!("text:{}", serde_json::to_string(&style).ok()?);
        self.made(key, || {
            let r = lightair_render::generate::text(&style);
            Some((r.rgba, r.size, true))
        })
    }

    fn colour(&mut self, s: &Value) -> Option<Shot> {
        let abgr = s["color"].as_u64().unwrap_or(0xFFFF_FFFF) as u32;
        let w = s["width"].as_u64().unwrap_or(1920).clamp(1, 4096) as u32;
        let h = s["height"].as_u64().unwrap_or(1080).clamp(1, 4096) as u32;
        let key = format!("colour:{abgr:08x}:{w}x{h}");
        self.made(key, || {
            let px = [(abgr & 0xFF) as u8, (abgr >> 8 & 0xFF) as u8, (abgr >> 16 & 0xFF) as u8, (abgr >> 24) as u8];
            Some((px.repeat((w * h) as usize), (w, h), false))
        })
    }

    /// A widget: drawn again only when its settings, the chat or the alert change.
    fn widget(&mut self, s: &Value) -> Option<Shot> {
        let i = crate::widgets::instance(s)?;
        let size = (s["width"].as_u64().unwrap_or(800) as u32, s["height"].as_u64().unwrap_or(600) as u32);
        let chat = self.hot.chat.read();
        let alert = self.hot.alert.read();
        let uses_chat = i.kind != "counter" && i.kind != "alert-box" && i.kind != "subathon";
        // a subathon timer: what the widget server keeps for it, drawn again each second
        let now_ms =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        let stored = (i.kind == "subathon").then(|| crate::web::started().map(|w| w.storage(&i.id))).flatten();
        let tick = stored.as_ref().map_or(String::new(), |s| {
            let left = crate::widgets::subathon_left(&crate::widgets::values(&i), s, now_ms);
            format!("{}:{}", left / 1000, serde_json::Value::Object(s.clone()))
        });
        let key = format!(
            "widget:{}:{}:{:?}:{}:{}:{}",
            i.kind,
            i.fields,
            size,
            if uses_chat { chat.1 } else { 0 },
            alert.as_ref().map_or(0, |(a, _)| a.id),
            tick
        );
        let feed = crate::widgets::Feed {
            chat: &chat.0,
            alert: alert.as_ref().map(|(a, _)| a),
            stored: stored.as_ref(),
            now_ms,
        };
        self.made(key, || {
            let (px, size) = crate::widgets::draw(&i, size, &feed);
            Some((px, size, true))
        })
    }

    fn timer(&mut self, s: &Value) -> Option<Shot> {
        let shown = timer_text(s, self.pts_ns - self.program.on_air_ns, self.program.utc_offset_min);
        self.text(s, shown)
    }
}

/// One camera feed's face tracking for the looks: the tracker logic on the host's pushes.
#[derive(Default)]
pub struct LookState {
    seg_serial: u64,
    tracker: crate::looks::face::Tracker,
    masks: crate::looks::masks::MaskFilter,
    pigment: crate::looks::pigment::Calibrator,
    weight: crate::looks::face::FaceWeight,
}

impl Pictures for Frame<'_> {
    fn picture(&mut self, source: &Source) -> Option<Shot> {
        let mut shot = self.picture_of(source)?;
        // the looks on a camera, before its other effects (as on the desktop)
        if let Some(mut settings) = crate::looks::render::Settings::from_source(source) {
            // hot (the governor's step 3): the face parts stop, Light and the grade stay
            if self.hot.governor_step.load(std::sync::atomic::Ordering::Relaxed) >= crate::looks::LIGHTER_STEP {
                settings.lighten();
            }
            if settings.active() {
                shot = self.looked(source, shot, &settings);
            }
        }
        // a camera's background after its looks, as the desktop's looks draw it: the light and
        // the grade colour the person, and the background replacing theirs stays as it is
        if source.kind != templates::MEDIA
            && source.kind != templates::SCREEN
            && let Some(look) = crate::background::look(source)
            && let Some(feed) = self.program.feeds.get(&source.id).copied()
        {
            shot = self.background(feed, shot, &look);
        }
        let steps = crate::keying::steps(source);
        if steps.is_empty() {
            return Some(shot);
        }
        Some(self.keyed(source, shot, &steps))
    }
}

impl Frame<'_> {
    /// A camera through the looks (beauty, light, grade), with
    /// the faces and masks its host pushed.
    fn looked(&mut self, source: &Source, shot: Shot, settings: &crate::looks::render::Settings) -> Shot {
        let Some(feed) = self.program.feeds.get(&source.id).copied() else {
            return shot;
        };
        let Some(f) = self.hot.feed(feed) else {
            return shot;
        };
        let Feed::Video(v) = &*f else { return shot };
        let now = crate::clock::host_now_ns();
        v.want_face(now);
        v.want_mask(1, now);
        let frame_ns = v.at(self.host_ns).map_or(now, |(_, _, t)| t);
        let mut _kept: Option<Box<dyn std::any::Any>> = None;
        let texture = match &shot.picture {
            Picture::Texture(t) => t.clone(),
            #[allow(unreachable_patterns)]
            _ => {
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                {
                    let Some((picture, _, _)) = v.at(self.host_ns) else {
                        return shot;
                    };
                    match &*picture {
                        Pushed::Buffer(_, k) => match crate::gpu::apple::sampled(self.gpu.device(), &k.0) {
                            Some(t) => {
                                _kept = Some(Box::new(k.clone()));
                                t
                            }
                            None => return shot,
                        },
                        _ => return shot,
                    }
                }
                #[cfg(not(any(target_os = "macos", target_os = "ios")))]
                return shot;
            }
        };
        // the host's newest faces and segmentation into the tracker logic, with a ≤720 copy of
        // the frame for what reads pixels (lid snapping, cheek colour, mask motion, pigment), read
        // back only when something new came: each analysed frame is read back once
        let new_faces: Vec<Arc<crate::feeds::FacePush>> = {
            let last = self.cache.trackers.get(&feed).and_then(|s| s.tracker.last_serial()).unwrap_or(0);
            v.faces.lock().iter().filter(|p| p.serial > last).cloned().collect()
        };
        let seg = v.segmentation.lock().clone();
        let new_seg =
            seg.as_ref().is_some_and(|s| self.cache.trackers.get(&feed).is_none_or(|st| st.seg_serial != s.serial));
        let copy = (!new_faces.is_empty() || new_seg).then(|| self.small_copy(&texture)).flatten();
        let luma = copy.as_ref().map(|(rgba, w, h)| crate::looks::face::Luma::from_rgba(rgba, *w, *h));
        let state = self.cache.trackers.entry(feed).or_default();
        for p in &new_faces {
            state.tracker.push(p, luma.as_ref());
        }
        if let Some(seg) = seg.filter(|_| new_seg) {
            state.seg_serial = seg.serial;
            state.masks.push(&seg, luma.as_ref());
            if let Some((rgba, w, h)) = &copy {
                let picture = crate::looks::pigment::Picture { rgba, width: *w, height: *h };
                state.pigment.push(&seg, &picture, now);
            }
        }
        let face = state.tracker.get_at(frame_ns, now);
        let weight = state.weight.update(face.present, now);
        let face_box =
            face.present.then(|| v.faces.lock().iter().rev().find(|p| p.present).and_then(|p| p.face_box())).flatten();
        *v.looks_seen.lock() = Some(crate::feeds::LooksSeen {
            at_ns: now,
            analysed: state.tracker.last_serial().is_some(),
            present: face.present,
            face_box,
        });
        let masks = state.masks.masks();
        let pigment = state.pigment.pigment();
        let person = v.mask.lock().clone();
        let gpu = self.gpu;
        let looks = self.cache.looks.get_or_insert_with(|| crate::looks::render::Looks::new(gpu));
        let out = looks.draw(
            gpu,
            source.id,
            &texture,
            settings,
            face.valid.then_some(&face),
            weight,
            &masks,
            &pigment,
            person.as_deref(),
        );
        Shot { picture: Picture::Texture(out), size: shot.size, premultiplied: false }
    }

    /// A picture scaled to at most 720 on its short side, read back as RGBA (the looks'
    /// analysis copy).
    fn small_copy(&self, texture: &wgpu::Texture) -> Option<(Vec<u8>, u32, u32)> {
        let (w, h) = (texture.width(), texture.height());
        let short = w.min(h).max(1);
        let (sw, sh) = if short > 720 { ((w * 720 / short).max(1), (h * 720 / short).max(1)) } else { (w, h) };
        let small = self.gpu.target((sw, sh), wgpu::TextureFormat::Rgba8Unorm);
        let mut encoder = self.gpu.device().create_command_encoder(&Default::default());
        let view = small.create_view(&Default::default());
        self.gpu.blit(&mut encoder, texture, &view, wgpu::TextureFormat::Rgba8Unorm);
        self.gpu.queue().submit([encoder.finish()]);
        lightair_render::compose::read_texture(&small).map(|px| (px, sw, sh))
    }

    /// A source's picture through its sharpen and keyer passes, straight alpha.
    fn keyed(&mut self, source: &Source, shot: Shot, steps: &[crate::keying::Step]) -> Shot {
        // an Apple camera's buffer, sampled as a texture (kept until it's drawn)
        let mut _kept: Option<Box<dyn std::any::Any>> = None;
        let texture = match &shot.picture {
            Picture::Texture(t) => t.clone(),
            #[allow(unreachable_patterns)]
            _ => {
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                {
                    let Some(v) = self.program.feeds.get(&source.id).and_then(|f| self.hot.feed(*f)) else {
                        return shot;
                    };
                    let Feed::Video(v) = &*v else { return shot };
                    let Some((picture, _, _)) = v.at(self.host_ns) else {
                        return shot;
                    };
                    match &*picture {
                        Pushed::Buffer(_, k) => match crate::gpu::apple::sampled(self.gpu.device(), &k.0) {
                            Some(t) => {
                                _kept = Some(Box::new(k.clone()));
                                t
                            }
                            None => return shot,
                        },
                        _ => return shot,
                    }
                }
                #[cfg(not(any(target_os = "macos", target_os = "ios")))]
                return shot;
            }
        };
        let gpu = self.gpu;
        // a light or the skin smoothing follows the person: the host's mask, asked for while
        // it's drawn (drawn without it until the first comes)
        let mask = if steps.iter().any(|s| s.mode == 6 || s.mode == 7) {
            self.program.feeds.get(&source.id).and_then(|f| self.hot.feed(*f)).and_then(|f| match &*f {
                Feed::Video(v) => {
                    let now = crate::clock::host_now_ns();
                    v.want_mask(1, now);
                    // the looks follow the face
                    v.want_face(now);
                    v.mask.lock().clone()
                }
                _ => None,
            })
        } else {
            None
        };
        let mask_t = mask.as_ref().map(|m| self.cache.key_masks.texture(gpu, source.id, m));
        let mask_in = mask.as_ref().zip(mask_t.as_ref()).map(|(m, t)| crate::keying::MaskIn {
            texture: t,
            rotation: m.rotation,
            mirrored: m.mirrored,
        });
        let pipeline = self.cache.keying.get_or_insert_with(|| crate::keying::Pipeline::new(gpu.device()));
        let out = crate::keying::draw(
            gpu,
            pipeline,
            &mut self.cache.keyed,
            source.id,
            &texture,
            shot.premultiplied,
            steps,
            mask_in,
        );
        Shot { picture: Picture::Texture(out), size: shot.size, premultiplied: false }
    }

    fn picture_of(&mut self, source: &Source) -> Option<Shot> {
        let s = &source.settings;
        match source.kind.as_str() {
            templates::CAMERA | "lightair_camera_capture" | templates::SCREEN | templates::MEDIA => {
                let feed = *self.program.feeds.get(&source.id)?;
                self.feed_picture(feed)
            }
            templates::IMAGE => self.image(s["file"].as_str()?),
            templates::TEXT => {
                let t = s["text"].as_str().unwrap_or("").to_string();
                self.text(s, t)
            }
            templates::COLOUR => self.colour(s),
            templates::TIMER => self.timer(s),
            // a web layer (an imported widget, a page): the host's web view's frames
            crate::widgets::KIND => match self.program.feeds.get(&source.id) {
                Some(f) => self.feed_picture(*f),
                None => self.widget(s),
            },
            _ => None,
        }
    }
}

pub use lightair_overlay::text::{text_style, timer_text};

/// A GIF's frames are at most this long on a side…
const GIF_SIDE: u32 = 640;
/// …and take at most this much memory together (a long one is cut short).
const GIF_BYTES: usize = 96 << 20;

/// Decodes an image (or every frame of a GIF, within the limits above) into textures on the
/// shared device. Runs on a thread of its own.
fn decode(path: &Path) -> Option<Animated> {
    decode_within(path, GIF_SIDE, GIF_BYTES)
}

fn decode_within(path: &Path, side: u32, cap: usize) -> Option<Animated> {
    use image::AnimationDecoder;
    let gpu = lightair_render::shared_gpu().ok()?;
    let texture = |rgba: &[u8], size: (u32, u32)| crate::gpu::texture_on(&gpu.device, &gpu.queue, rgba, size);
    let bytes = std::fs::read(path).ok()?;
    if bytes.starts_with(b"GIF8") {
        let dec = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&bytes)).ok()?;
        let mut frames = Vec::new();
        let (mut end, mut size, mut used) = (0u32, (0, 0), 0usize);
        for f in dec.into_frames() {
            let f = f.ok()?;
            let (n, d) = f.delay().numer_denom_ms();
            let mut buf = f.into_buffer();
            if buf.width().max(buf.height()) > side {
                let k = side as f32 / buf.width().max(buf.height()) as f32;
                let (w, h) = (((buf.width() as f32 * k) as u32).max(1), ((buf.height() as f32 * k) as u32).max(1));
                buf = image::imageops::thumbnail(&buf, w, h);
            }
            let bytes = buf.as_raw().len();
            if used + bytes > cap {
                tracing::warn!(path = %path.display(), frames = frames.len(), "a GIF cut short: too big to keep whole");
                break;
            }
            used += bytes;
            end += (n / d.max(1)).max(20);
            size = buf.dimensions();
            frames.push((texture(buf.as_raw(), size), end));
        }
        return (!frames.is_empty()).then_some(Animated { frames, size });
    }
    let img = image::load_from_memory(&bytes).ok()?;
    // at most 4096 on a side (a GPU's limit and a phone's memory)
    let img = if img.width() > 4096 || img.height() > 4096 { img.thumbnail(4096, 4096) } else { img };
    let rgba = img.to_rgba8();
    let size = rgba.dimensions();
    Some(Animated { frames: vec![(texture(rgba.as_raw(), size), u32::MAX)], size })
}

/// Tests: an image's decoded size and frame count.
#[cfg(test)]
pub fn decode_for_test(path: &Path, side: u32, cap: usize) -> Option<((u32, u32), usize)> {
    decode_within(path, side, cap).map(|a| (a.size, a.frames.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A long show that changes pictures: a decoded image nothing names any more is let go when
    /// the project changes; the ones its layers and effects still use stay.
    #[test]
    fn a_picture_nothing_names_is_let_go_when_the_project_changes() {
        let mut c = Cache::default();
        for p in ["/show/media/logo.png", "/show/media/old.png", "/show/media/frame.png"] {
            c.images.insert(PathBuf::from(p), Slot::Broken);
        }
        let mut ws = lightair_model::Workspace::new("Show");
        let logo = lightair_model::Source::new("Logo", "image_source", json!({ "file": "media/logo.png" }));
        let mut cam = lightair_model::Source::new("Cam", "lightair_camera", json!({}));
        cam.effects.push(lightair_model::Effect::new("Frame", "fx.frame", json!({ "image": "/show/media/frame.png" })));
        for s in [logo, cam] {
            ws.sources.insert(s.id, Arc::new(s));
        }
        c.keep_sources(&Arc::new(ws));
        let mut kept: Vec<_> = c.images.keys().map(|p| p.to_string_lossy().into_owned()).collect();
        kept.sort();
        assert_eq!(kept, ["/show/media/frame.png", "/show/media/logo.png"]);
    }

    #[test]
    fn a_feed_that_goes_takes_its_textures() {
        let mut c = Cache::default();
        c.backgrounds.insert(7, Default::default());
        c.trackers.insert(7, Default::default());
        c.backgrounds.insert(8, Default::default());
        c.forget_feed(7);
        assert!(!c.backgrounds.contains_key(&7) && !c.trackers.contains_key(&7));
        assert!(c.backgrounds.contains_key(&8), "the others stay");
    }

    #[test]
    fn timers_count_as_the_desktop_does() {
        let s = json!({ "mode": "countdown", "seconds": 90, "end_text": "Live soon" });
        assert_eq!(timer_text(&s, 0, 0), "1:30");
        assert_eq!(timer_text(&s, 31_000_000_000, 0), "0:59");
        assert_eq!(timer_text(&s, 95_000_000_000, 0), "Live soon");
        assert_eq!(timer_text(&json!({ "mode": "stopwatch" }), 3_725_000_000_000, 0), "1:02:05");
    }

    #[test]
    fn text_settings_read_back_as_the_style() {
        let s = templates::text_settings("Hi", Some("Inter"), 64.0, 0xFF8800, true, 0, Some(0x000000), None);
        let t = text_style(&s, "Hi".into());
        assert_eq!((t.color, t.bold, t.align, t.size), (0xFF8800, true, 0, 64.0));
        assert_eq!(t.outline, Some(0));
        assert_eq!(t.font.unwrap().family, "Inter");
    }
}
