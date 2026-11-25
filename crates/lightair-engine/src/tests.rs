//! The engine end to end on the Mac's GPU, with a stand-in host: a camera's BGRA buffers and a
//! microphone's sound pushed in, a recording's frames and sound pulled out as the iOS host does.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::host::*;
use crate::session::{NewLayer, Session, TextSpec};
use crate::{DeviceInfo, Projects, ffi};

#[derive(Default)]
struct FakeHost {
    calls: Mutex<Vec<String>>,
    camera: Mutex<Option<u32>>,
    mic: Mutex<Option<u32>>,
    recording: Mutex<Option<(u32, RecordingRequest)>>,
    recordings: Mutex<Vec<(u32, RecordingRequest)>>,
    /// Set by a test: the encoders are pulled from inside their open calls, as iOS's are.
    studio: std::sync::atomic::AtomicU64,
    /// What those pulls got (-1 would mean "closed").
    pulled_in_open: Mutex<Vec<i32>>,
    /// set_video_bitrate calls: (encoder, kbps).
    bitrates: Mutex<Vec<(u32, u32)>>,
    /// open_video_encoder calls: (encoder, config).
    opened: Mutex<Vec<(u32, VideoEncoderConfig)>>,
    /// Set by a test: each video encoder gets its own pull loop, started inside its open call
    /// as iOS's `StudioVideoEncoder.open` does, that sends a packet back for every frame.
    pull_loops: AtomicBool,
    loops: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// What those loops pulled, by encoder, and the encoders whose loop was told "closed".
    frames: Arc<Mutex<std::collections::HashMap<u32, u64>>>,
    ended: Arc<Mutex<Vec<u32>>>,
    /// Set by a test: start_recording doesn't return until it's cleared (a host call stuck).
    stuck: AtomicBool,
}

impl Host for FakeHost {
    fn start_camera(&self, feed: u32, r: CameraRequest) -> Result<(), HostError> {
        self.calls.lock().push(format!(
            "start_camera {feed} {}p{} {:?} {:?}",
            r.height, r.fps, r.facing, r.device
        ));
        *self.camera.lock() = Some(feed);
        Ok(())
    }
    fn stop_camera(&self, feed: u32) {
        self.calls.lock().push(format!("stop_camera {feed}"));
    }
    fn start_microphone(&self, feed: u32, _: Option<String>) -> Result<(), HostError> {
        *self.mic.lock() = Some(feed);
        Ok(())
    }
    fn stop_microphone(&self, _: u32) {}
    fn audio_inputs(&self) -> Vec<AudioInput> {
        vec![AudioInput {
            id: "builtin".into(),
            name: "iPhone Microphone".into(),
            kind: InputKind::BuiltIn,
            latency_ms: 0,
        }]
    }
    fn open_media(&self, _: u32, _: MediaRequest) -> Result<(), HostError> {
        Ok(())
    }
    fn close_media(&self, _: u32) {}
    fn open_video_encoder(
        &self,
        id: u32,
        c: VideoEncoderConfig,
    ) -> Result<EncoderTarget, HostError> {
        let studio = self.studio.load(Ordering::Acquire);
        if studio != 0 {
            self.pulled_in_open
                .lock()
                .push(ffi::next_video_frame(studio, id, 0).0);
        }
        self.opened.lock().push((id, c.clone()));
        if studio != 0 && self.pull_loops.load(Ordering::Acquire) {
            let (frames, ended) = (self.frames.clone(), self.ended.clone());
            let packet = vec![0u8; c.bitrate_kbps as usize * 1000 / 8 / 30];
            let (first, wait) = std::sync::mpsc::channel();
            self.loops.lock().push(std::thread::spawn(move || {
                let mut told = Some(first);
                loop {
                    // as fast as it can until its first frame (the open is still returning)
                    let timeout = if frames.lock().contains_key(&id) {
                        50
                    } else {
                        0
                    };
                    let (r, buf, pts, key) = ffi::next_video_frame(studio, id, timeout);
                    if let Some(t) = told.take() {
                        let _ = t.send(());
                    }
                    if r < 0 {
                        ended.lock().push(id);
                        break;
                    }
                    if r == 1 {
                        crate::encode::release_buffer(buf);
                        *frames.lock().entry(id).or_default() += 1;
                        ffi::video_packet(studio, id, &packet, true, pts, key);
                    }
                }
            }));
            // the loop is pulling before the open returns
            let _ = wait.recv();
        }
        Ok(EncoderTarget::PixelBufferPool(
            crate::gpu::apple::test_pool(c.width, c.height),
        ))
    }
    fn set_video_bitrate(&self, e: u32, kbps: u32) {
        self.bitrates.lock().push((e, kbps));
    }
    fn close_video_encoder(&self, _: u32) {}
    fn open_audio_encoder(&self, id: u32, _: AudioEncoderConfig) -> Result<(), HostError> {
        let studio = self.studio.load(Ordering::Acquire);
        if studio != 0 {
            let mut out = [0f32; 64];
            self.pulled_in_open
                .lock()
                .push(ffi::read_program_audio(studio, id, &mut out, 0).0);
        }
        Ok(())
    }
    fn close_audio_encoder(&self, _: u32) {}
    fn start_recording(&self, id: u32, r: RecordingRequest) -> Result<(), HostError> {
        self.recordings.lock().push((id, r.clone()));
        self.recording.lock().get_or_insert((id, r));
        while self.stuck.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
    fn stop_recording(&self, id: u32) {
        self.calls.lock().push(format!("stop_recording {id}"));
    }
    fn keep_screen_on(&self, _: bool) {}
}

#[derive(Default)]
struct Heard(Mutex<Vec<Event>>);

impl Events for Heard {
    fn event(&self, e: Event) {
        self.0.lock().push(e);
    }
}

/// The tests that run a whole session on the GPU take turns: four at once on a busy machine (a
/// workspace check, an emulator, other builds) starve each other's render threads.
static GPU_TURN: Mutex<()> = parking_lot::const_mutex(());

/// A session test's turn on the GPU, or `None` without one (the test is skipped).
fn gpu_turn() -> Option<parking_lot::MutexGuard<'static, ()>> {
    lightair_render::shared_gpu().ok()?;
    Some(GPU_TURN.lock())
}

/// Waits for `f`, generously: these tests check what happens, not how fast on a loaded machine.
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "waited too long for {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A BGRA IOSurface buffer of one colour from `pool`, given as a +1 pointer.
fn buffer(pool: &crate::gpu::apple::PoolTarget, bgra: [u8; 4]) -> *mut std::ffi::c_void {
    use objc2_core_video::*;
    let gpu = lightair_render::shared_gpu().unwrap();
    let (b, _) = pool.next(&gpu.device).unwrap();
    unsafe { CVPixelBufferLockBaseAddress(&b, CVPixelBufferLockFlags(0)) };
    let (w, h) = (CVPixelBufferGetWidth(&b), CVPixelBufferGetHeight(&b));
    let base = CVPixelBufferGetBaseAddress(&b) as *mut u8;
    let stride = CVPixelBufferGetBytesPerRow(&b);
    for y in 0..h {
        for x in 0..w {
            unsafe {
                std::ptr::copy_nonoverlapping(bgra.as_ptr(), base.add(y * stride + x * 4), 4)
            };
        }
    }
    unsafe { CVPixelBufferUnlockBaseAddress(&b, CVPixelBufferLockFlags(0)) };
    crate::gpu::apple::into_raw(b) as *mut std::ffi::c_void
}

/// The BGRA pixel a sixth of the way down the middle of a pulled buffer (above the text), and
/// the buffer released.
fn centre(raw: usize) -> [u8; 4] {
    use objc2_core_video::*;
    let b = unsafe {
        objc2_core_foundation::CFRetained::from_raw(
            std::ptr::NonNull::new(raw as *mut CVPixelBuffer).unwrap(),
        )
    };
    unsafe { CVPixelBufferLockBaseAddress(&b, CVPixelBufferLockFlags::ReadOnly) };
    let (w, h) = (CVPixelBufferGetWidth(&b), CVPixelBufferGetHeight(&b));
    let stride = CVPixelBufferGetBytesPerRow(&b);
    let base = CVPixelBufferGetBaseAddress(&b) as *const u8;
    let p = unsafe { std::slice::from_raw_parts(base.add(h / 6 * stride + w / 2 * 4), 4) };
    let out = [p[0], p[1], p[2], p[3]];
    unsafe { CVPixelBufferUnlockBaseAddress(&b, CVPixelBufferLockFlags::ReadOnly) };
    out
}

#[test]
fn a_show_records_with_camera_microphone_text_and_the_privacy_cut() {
    let Some(_turn) = gpu_turn() else {
        eprintln!("no GPU here; skipping");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Test show").unwrap();
    let host = Arc::new(FakeHost::default());
    let heard = Arc::new(Heard::default());
    let session: Arc<Session> = projects
        .open_session(&p.id, host.clone(), heard.clone())
        .unwrap();
    let studio = session.handle;
    wait_for("the camera and microphone", || {
        host.camera.lock().is_some() && host.mic.lock().is_some()
    });
    let (cam, mic) = (host.camera.lock().unwrap(), host.mic.lock().unwrap());

    // the camera (red) and the microphone (a quiet tone) keep coming, as a phone's would
    let running = Arc::new(AtomicBool::new(true));
    let pusher = {
        let running = running.clone();
        std::thread::spawn(move || {
            let pool = unsafe {
                crate::gpu::apple::PoolTarget::from_raw(crate::gpu::apple::test_pool(64, 36))
            }
            .unwrap();
            let mut n = 0u64;
            while running.load(Ordering::Acquire) {
                let now = crate::clock::host_now_ns();
                let tone: Vec<f32> = (0..480)
                    .map(|i| ((n * 480 + i) as f32 * 0.05).sin() * 0.1)
                    .collect();
                // (a full ring on a starved machine drops the block: fine here)
                let _ = ffi::push_audio(studio, mic, &tone, 1, 48_000, now);
                if n.is_multiple_of(3) {
                    assert!(unsafe {
                        ffi::push_pixel_buffer(studio, cam, buffer(&pool, [0, 0, 255, 255]), now)
                    });
                }
                n += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };

    session.add_layer(NewLayer::Text {
        text: TextSpec {
            text: "Hello".into(),
            font_family: None,
            size: 64.0,
            color: 0xFFFFFF,
            bold: true,
            align: 1,
            outline: None,
            background: None,
        },
    });
    wait_for("the text layer", || {
        heard
            .0
            .lock()
            .iter()
            .any(|e| matches!(e, Event::LayerAdded { .. }))
    });
    session.start_recording(false);
    wait_for("the recording", || host.recording.lock().is_some());
    let (rec, req) = host.recording.lock().clone().unwrap();
    let (video, audio) = (req.video_encoder, req.audio_encoder.unwrap());

    // pull as the iOS host does: frames from the camera, red at the centre
    let mut pts = Vec::new();
    let mut colours = Vec::new();
    // a debug build draws slower (wgpu's checks), the more so beside other tests: frames late by
    // more than one are skipped, so wait for enough rather than a fixed time
    let wanted = if cfg!(debug_assertions) { 6 } else { 15 };
    let started = Instant::now();
    while pts.len() < wanted && started.elapsed() < Duration::from_secs(30) {
        let (r, buf, t, _) = ffi::next_video_frame(studio, video, 100);
        if r == 1 {
            colours.push(centre(buf));
            pts.push(t);
        }
    }
    let e = session.hot().video_encoder(video).unwrap();
    assert!(
        pts.len() >= wanted,
        "{} frames in {:?}; drawn {}, dropped {}, encoder dropped {}",
        pts.len(),
        started.elapsed(),
        session.hot().stats.frames.load(Ordering::Relaxed),
        session.hot().stats.dropped.load(Ordering::Relaxed),
        e.dropped.load(Ordering::Relaxed)
    );
    // program times exactly a frame apart, never backwards
    let on_grid = |t: i64| crate::clock::frame_ns((t as f64 * 30.0 / 1e9).round() as u64, 30) == t;
    assert!(
        pts.windows(2).all(|w| w[1] > w[0]) && pts.iter().all(|t| on_grid(*t)),
        "{pts:?}"
    );
    let red = colours
        .iter()
        .filter(|c| c[2] > 200 && c[0] < 40 && c[1] < 40)
        .count();
    assert!(red * 2 > colours.len(), "{colours:?}");

    // the program's sound, with program times that follow on
    let mut out = vec![0f32; 2048 * 2];
    let (n, first) = ffi::read_program_audio(studio, audio, &mut out, 200);
    assert!(n > 0 && first >= 0);
    let (n2, second) = ffi::read_program_audio(studio, audio, &mut out, 200);
    assert!(n2 > 0);
    assert_eq!(second, first + crate::clock::samples_to_ns(n as u64));
    assert!(
        out[..n2 as usize * 2].iter().any(|s| s.abs() > 0.01),
        "silence"
    );

    // the privacy cut: the picture is covered and the microphone silenced
    session.set_privacy_cut(true);
    std::thread::sleep(Duration::from_millis(150));
    let mut covered = 0;
    let until = Instant::now() + Duration::from_secs(30);
    while covered <= 3 && Instant::now() < until {
        let (r, buf, _, _) = ffi::next_video_frame(studio, video, 100);
        if r == 1 && centre(buf)[2] < 60 {
            covered += 1;
        }
    }
    assert!(covered > 3);
    // drain what was mixed before the cut, then it's silent
    while ffi::read_program_audio(studio, audio, &mut out, 0).0 > 0 {}
    std::thread::sleep(Duration::from_millis(100));
    let (n, _) = ffi::read_program_audio(studio, audio, &mut out, 200);
    assert!(n > 0 && out[480..n as usize * 2].iter().all(|s| s.abs() < 1e-3));
    session.set_privacy_cut(false);

    // stop: the pulls end, the host finishes the file, the index lists it
    session.stop_recording();
    wait_for("the end of the frames", || {
        ffi::next_video_frame(studio, video, 10).0 == -1
    });
    let file = std::path::PathBuf::from(&req.path);
    std::fs::write(&file, b"mp4").unwrap();
    session.recording_finished(
        rec,
        RecordingResult::Finished {
            duration_ms: 1500,
            bytes: 3,
        },
    );
    let index = crate::record::folder(tmp.path()).join("index.json");
    wait_for("the index", || index.exists());
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
    assert_eq!(doc["items"][0]["duration_ms"], 1500);
    assert_eq!(doc["items"][0]["width"], 720);

    running.store(false, Ordering::Release);
    pusher.join().unwrap();
    session.close();
    assert!(
        !ffi::push_audio(studio, mic, &[0.0; 4], 1, 48_000, 0),
        "a closed session takes nothing"
    );
    let st = heard.0.lock();
    assert!(st.iter().any(|e| matches!(e, Event::Meters { .. })));
    assert!(session.hot().stats.frames.load(Ordering::Relaxed) > 0);
}

#[test]
fn yuv_planes_come_out_upright_in_rgb() {
    let Ok(gpu) = crate::gpu::Gpu::new() else {
        return;
    };
    // 4×2 luma, full range: left half white-ish... use a flat colour: Y 0.5, Cb 0, Cr +0.5 (red)
    let f = crate::feeds::Yuv {
        width: 4,
        height: 2,
        y: vec![128; 8],
        uv: [128u8, 255].repeat(2),
        rotation: 90,
        mirrored: false,
        full_range: true,
        matrix: 0,
    };
    let t = gpu.yuv_to_rgba(&f, None);
    assert_eq!((t.width(), t.height()), (2, 4));
    let px = lightair_render::compose::read_texture(&t).unwrap();
    // Y ½, Cr +½: R = ½ + 1.402·½ (clipped), G = ½ − 0.714·½, B = ½
    assert!(
        px[0] == 255 && (30..45).contains(&px[1]) && (120..136).contains(&px[2]),
        "{:?}",
        &px[..4]
    );
}

#[test]
fn a_fade_blends_a_gesture_undoes_whole_and_a_quiet_camera_dims() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Fades").unwrap();
    let host = Arc::new(FakeHost::default());
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, host.clone(), heard.clone())
        .unwrap();
    let studio = session.handle;
    wait_for("the camera", || host.camera.lock().is_some());
    let cam = host.camera.lock().unwrap();
    let running = Arc::new(AtomicBool::new(true));
    let pusher = {
        let running = running.clone();
        std::thread::spawn(move || {
            let pool = unsafe {
                crate::gpu::apple::PoolTarget::from_raw(crate::gpu::apple::test_pool(64, 36))
            }
            .unwrap();
            while running.load(Ordering::Acquire) {
                let now = crate::clock::host_now_ns();
                unsafe {
                    ffi::push_pixel_buffer(studio, cam, buffer(&pool, [0, 0, 255, 255]), now)
                };
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };
    let version = |s: &Session| s.state().version;

    // scene 2: the camera with blue over it; scene 1 fades in over a second
    let one = session.state().program;
    let v = version(&session);
    session.add_scene("Two", Some(one));
    wait_for("scene 2", || {
        version(&session) > v && session.state().workspace.scenes.len() == 2
    });
    let two = session.state().workspace.scenes[1].id;
    session.switch_scene(two);
    wait_for("the switch", || session.state().program == two);
    session.add_layer(NewLayer::Colour { argb: 0xFF00_00FF });
    wait_for("the blue", || {
        heard
            .0
            .lock()
            .iter()
            .any(|e| matches!(e, Event::LayerAdded { .. }))
    });
    session.set_transition(one, lightair_model::TransitionKind::Fade, 1000);
    session.start_recording(false);
    wait_for("the recording", || host.recording.lock().is_some());
    let (rec, req) = host.recording.lock().clone().unwrap();
    let video = req.video_encoder;
    let pull = |until: Duration| {
        let mut out = Vec::new();
        let start = Instant::now();
        while start.elapsed() < until {
            let (r, buf, _, _) = ffi::next_video_frame(studio, video, 100);
            if r == 1 {
                out.push(centre(buf));
            }
        }
        out
    };
    // (the switch to scene 2 moved over half a second first)
    let blue = |c: &[u8; 4]| c[0] > 200 && c[2] < 40;
    let started = Instant::now();
    let mut before = Vec::new();
    while !before.last().is_some_and(blue) && started.elapsed() < Duration::from_secs(30) {
        before.extend(pull(Duration::from_millis(100)));
    }
    assert!(before.last().is_some_and(blue), "blue first: {before:?}");
    session.switch_scene(one);
    // pulled until the fade has ended in red (however slowly the frames come)
    let red = |c: &[u8; 4]| c[2] > 200 && c[0] < 40;
    let mut during = Vec::new();
    let started = Instant::now();
    while !(during.len() >= 3 && during.iter().rev().take(3).all(red))
        && started.elapsed() < Duration::from_secs(30)
    {
        during.extend(pull(Duration::from_millis(100)));
    }
    let mixed = during
        .iter()
        .filter(|c| (40..215).contains(&c[0]) && (40..215).contains(&c[2]))
        .count();
    assert!(mixed > 0, "a blend between blue and red: {during:?}");
    assert!(
        during.iter().rev().take(3).all(red),
        "red at the end: {during:?}"
    );

    // a drag is one undo step
    session.attach_preview(0, 720, 1280, 2.0);
    let camera_layer = session.state().scene().unwrap().objects[0].id;
    let x0 = |s: &Session| {
        let st = s.state();
        st.scene().unwrap().object(camera_layer).unwrap().layouts[&st.canvas].x
    };
    assert_eq!(x0(&session), 0.0);
    session.begin_gesture(camera_layer);
    for dx in [3.0, 6.0, 10.0] {
        let f = session
            .update_gesture(crate::gesture::Delta {
                dx,
                dy: 0.0,
                scale: 1.0,
                rotation: 0.0,
                snap_threshold: 0.0,
            })
            .unwrap();
        assert_eq!(f.frame.cx, 180.0 + dx);
    }
    let v = version(&session);
    session.end_gesture();
    wait_for("the drag", || version(&session) > v);
    assert_eq!(x0(&session), 20.0);
    assert_eq!(session.state().undo.as_deref(), Some("Move object"));
    let v = version(&session);
    session.undo();
    wait_for("the undo", || version(&session) > v);
    assert_eq!(x0(&session), 0.0);

    // the camera goes quiet: its last frame stays, dimmed
    running.store(false, Ordering::Release);
    pusher.join().unwrap();
    let dimmed = |c: &[u8; 4]| c[2] > 40 && c[2] < 200 && c[0] < 40;
    let mut quiet = Vec::new();
    let started = Instant::now();
    while !quiet.last().is_some_and(dimmed) && started.elapsed() < Duration::from_secs(30) {
        quiet.extend(pull(Duration::from_millis(100)));
    }
    assert!(
        quiet.last().is_some_and(dimmed),
        "dimmed red: {:?}",
        quiet.last()
    );
    wait_for("the issue", || {
        heard
            .0
            .lock()
            .iter()
            .any(|e| matches!(e, Event::Issue { id, .. } if id == "camera-paused"))
    });

    session.stop_recording();
    session.recording_finished(
        rec,
        RecordingResult::Failed {
            reason: "test".into(),
        },
    );
    session.close();
}

#[test]
fn a_desktop_project_comes_in_with_what_phones_dont_draw_marked() {
    use lightair_model::{Layout, Object, Scene};
    let tmp = tempfile::tempdir().unwrap();
    let mut ws = crate::templates::project("Desk", 1080, 60);
    let web = crate::templates::source(
        "Overlay",
        "browser_source",
        serde_json::json!({ "url": "https://example.com" }),
    );
    let canvas = ws.canvases[0].id;
    let mut s = (*ws.scenes[0]).clone();
    s.objects.push(Object::new(
        "Overlay",
        web.id,
        [(canvas, Layout::boxed(0.0, 0.0, 100.0, 100.0))],
    ));
    ws.scenes[0] = Arc::new(s);
    ws.sources.insert(web.id, Arc::new(web));
    let _ = Scene::new("unused");
    let desk = tmp.path().join("desk");
    lightair_store::save(&desk, &ws).unwrap();
    let projects = Projects::open(
        &tmp.path().join("phone"),
        None,
        tmp.path(),
        DeviceInfo::default(),
    )
    .unwrap();
    let p = projects.import_portable(&desk).unwrap();
    let ws = lightair_store::portable::load_current(&projects.dir(&p.id).unwrap()).unwrap();
    let marked: Vec<String> = ws.scenes[0]
        .objects
        .iter()
        .filter_map(|o| crate::session::unsupported(&ws, o))
        .collect();
    assert_eq!(
        marked,
        vec!["Not shown on phones yet (browser_source)".to_string()]
    );
}

#[test]
fn the_camera_clean_records_beside_the_program_and_projects_have_pictures() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    // a class B phone: two encodes at once
    let device = DeviceInfo {
        platform: "ios".into(),
        model: "iPhone15,4".into(),
        memory_mb: 6000,
        ..Default::default()
    };
    let projects = Projects::open(tmp.path(), None, tmp.path(), device).unwrap();
    let p = projects.create("Clean").unwrap();
    let host = Arc::new(FakeHost::default());
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, host.clone(), heard.clone())
        .unwrap();
    let studio = session.handle;
    wait_for("the camera", || host.camera.lock().is_some());
    let cam = host.camera.lock().unwrap();
    let running = Arc::new(AtomicBool::new(true));
    let pusher = {
        let running = running.clone();
        std::thread::spawn(move || {
            let pool = unsafe {
                crate::gpu::apple::PoolTarget::from_raw(crate::gpu::apple::test_pool(64, 36))
            }
            .unwrap();
            while running.load(Ordering::Acquire) {
                let now = crate::clock::host_now_ns();
                unsafe {
                    ffi::push_pixel_buffer(studio, cam, buffer(&pool, [0, 0, 255, 255]), now)
                };
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };
    // the program: blue over the camera; the camera clean: the red camera alone
    session.add_layer(NewLayer::Colour { argb: 0xFF00_00FF });
    wait_for("the blue", || {
        heard
            .0
            .lock()
            .iter()
            .any(|e| matches!(e, Event::LayerAdded { .. }))
    });
    session.start_recording(true);
    wait_for("both files", || host.recordings.lock().len() == 2);
    let recs = host.recordings.lock().clone();
    assert!(
        recs[1].1.path.ends_with("(camera).mp4"),
        "{}",
        recs[1].1.path
    );
    let first = |encoder: u32, want: fn(&[u8; 4]) -> bool| {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            let (r, buf, _, _) = ffi::next_video_frame(studio, encoder, 100);
            if r == 1 && want(&centre(buf)) {
                return true;
            }
        }
        false
    };
    assert!(
        first(recs[0].1.video_encoder, |c| c[0] > 200 && c[2] < 40),
        "the program is blue"
    );
    assert!(
        first(recs[1].1.video_encoder, |c| c[2] > 200 && c[0] < 40),
        "the camera clean is red"
    );
    // its own sound
    let mut out = vec![0f32; 4096];
    let clean_audio = recs[1].1.audio_encoder.unwrap();
    assert_ne!(clean_audio, recs[0].1.audio_encoder.unwrap());

    session.stop_recording();
    for (id, req) in &recs {
        wait_for("the end of the frames", || {
            ffi::next_video_frame(studio, req.video_encoder, 10).0 == -1
        });
        while ffi::read_program_audio(studio, req.audio_encoder.unwrap(), &mut out, 0).0 > 0 {}
        std::fs::write(&req.path, b"mp4").unwrap();
        session.recording_finished(
            *id,
            RecordingResult::Finished {
                duration_ms: 1000,
                bytes: 3,
            },
        );
    }
    wait_for("idle", || {
        matches!(session.state().recording, RecordingState::Idle)
    });
    let index = crate::record::folder(tmp.path()).join("index.json");
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&index).unwrap()).unwrap();
    assert_eq!(doc["items"].as_array().unwrap().len(), 2);
    running.store(false, Ordering::Release);
    pusher.join().unwrap();
    session.close();

    // the project's picture: its program, about 480 on the long side
    wait_for("the thumbnail", || projects.list()[0].thumbnail.is_some());
    let t = projects.list()[0].thumbnail.clone().unwrap();
    wait_for("the thumbnail written", || image::open(&t).is_ok());
    let img = image::open(&t).unwrap().to_rgb8();
    assert_eq!(img.dimensions(), (270, 480));
    // a new project gets one of its own too
    let q = projects.create("Fresh").unwrap();
    wait_for("the new project's thumbnail", || {
        projects.get(&q.id).unwrap().thumbnail.is_some()
    });
}

/// A project whose one format is 7680×4320 at 120 fps: every frame is late on any machine.
fn heavy_project(projects: &Projects) -> String {
    let p = projects.create("Heavy").unwrap();
    let dir = projects.dir(&p.id).unwrap();
    let mut ws = lightair_store::portable::load_current(&dir).unwrap();
    for c in &mut ws.canvases {
        (c.width, c.height, c.fps_num, c.fps_den) = (7680, 4320, 120, 1);
    }
    lightair_store::save(&dir, &ws).unwrap();
    p.id
}

#[test]
fn a_late_renderer_still_pauses_and_closes_and_a_closed_recording_is_indexed() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let id = heavy_project(&projects);
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    wait_for("frames", || {
        session.hot().stats.frames.load(Ordering::Relaxed) > 2
    });
    // into the background: back within the limit, and nothing drawn after
    // (it returns once the render thread has paused, 2 s at most: it never hangs)
    let t = Instant::now();
    session.lifecycle(false);
    assert!(
        t.elapsed() < Duration::from_millis(2500),
        "{:?}",
        t.elapsed()
    );
    let frames = || session.hot().stats.frames.load(Ordering::Relaxed);
    wait_for("drawing to stop", || {
        let before = frames();
        std::thread::sleep(Duration::from_millis(300));
        frames() == before
    });
    let drawn = frames();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(frames(), drawn, "nothing drawn in the background");
    session.lifecycle(true);
    wait_for("frames again", || {
        session.hot().stats.frames.load(Ordering::Relaxed) > drawn
    });

    // closing while recording: the host finishes the file (its pulls still work) and the index
    // lists it before close returns
    session.start_recording(false);
    wait_for("the recording", || host.recording.lock().is_some());
    let (rec, req) = host.recording.lock().clone().unwrap();
    let finisher = {
        let session = session.clone();
        let studio = session.handle;
        std::thread::spawn(move || {
            while ffi::next_video_frame(studio, req.video_encoder, 50).0 != -1 {}
            std::fs::write(&req.path, b"mp4").unwrap();
            session.recording_finished(
                rec,
                RecordingResult::Finished {
                    duration_ms: 500,
                    bytes: 3,
                },
            );
        })
    };
    let t = Instant::now();
    session.close();
    // a recording's finish is waited for 5 s at most; the rest is the threads ending
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
    finisher.join().unwrap();
    let index = crate::record::folder(tmp.path()).join("index.json");
    assert!(index.exists(), "the closed recording is in the index");
}

#[test]
fn the_privacy_cut_survives_a_republish_and_engine_trouble_is_said() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Private").unwrap();
    let host = Arc::new(FakeHost::default());
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, host.clone(), heard.clone())
        .unwrap();
    wait_for("the camera", || host.camera.lock().is_some());
    session.set_privacy_cut(true);
    // a change to the document republishes everything: the cut stays
    session.add_scene("Later", None);
    wait_for("the change", || session.state().workspace.scenes.len() == 2);
    let at = session.hot().clock.sample_at(crate::clock::host_now_ns());
    assert!(session.hot().privacy_at(at + 48_000), "still covered");
    assert!(session.state().privacy);
    // from the sample it was cut at, for the picture and the sound alike
    let (on, from) = session.hot().privacy();
    assert!(on && !session.hot().privacy_at(from - 1) && session.hot().privacy_at(from));
    session.set_privacy_cut(false);
    assert!(!session.hot().privacy_at(at + 480_000));

    // a scene switch arrives in one go: the new scene never without its transition
    let one = session.state().program;
    session.set_transition(one, lightair_model::TransitionKind::Fade, 400);
    let later = session.state().workspace.scenes[1].id;
    session.switch_scene(later);
    wait_for("on the new scene", || {
        session.hot().program().scene == later
    });
    let before = session.hot().clock.drawn_ns();
    session.switch_scene(one);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let p = session.hot().program();
        if p.scene == one {
            let sw = p
                .switch
                .as_ref()
                .expect("the switch is published with the scene");
            assert!(
                sw.start_ns >= before,
                "it starts from the frame being drawn"
            );
            break;
        }
    }

    // trouble on an engine thread reaches the interface once
    session.hot().fail("The graphics stopped: test".into());
    session.hot().fail("again".into());
    wait_for("the failure", || {
        heard
            .0
            .lock()
            .iter()
            .any(|e| matches!(e, Event::Failed { reason } if reason.contains("graphics")))
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !heard
            .0
            .lock()
            .iter()
            .any(|e| matches!(e, Event::Failed { reason } if reason == "again"))
    );
    session.close();
}

#[test]
fn a_huge_gif_is_scaled_and_cut_to_fit() {
    let Some(_turn) = gpu_turn() else { return };
    use image::codecs::gif::GifEncoder;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("big.gif");
    {
        let f = std::fs::File::create(&path).unwrap();
        let mut enc = GifEncoder::new_with_speed(f, 30);
        let frame = image::RgbaImage::from_pixel(256, 144, image::Rgba([200, 10, 10, 255]));
        let frames = (0..12).map(|_| image::Frame::new(frame.clone()));
        enc.encode_frames(frames).unwrap();
    }
    // (the app's limits are 640 a side and 96 MB: here 128 and 200 kB, the same code)
    let a = crate::pictures::decode_for_test(&path, 128, 200_000).unwrap();
    assert_eq!(a.0, (128, 72));
    assert!(a.1 * 128 * 72 * 4 <= 200_000 && a.1 < 12, "{} frames", a.1);
}

#[test]
fn encoders_pulled_while_opening_wait_and_a_new_camera_is_waited_for() {
    let Some(_turn) = gpu_turn() else { return };
    use lightair_model::{Fit, Scene, TransitionKind, TransitionSettings};
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Two cameras").unwrap();
    // scene 2: the front camera, cut to
    let dir = projects.dir(&p.id).unwrap();
    let mut ws = lightair_store::portable::load_current(&dir).unwrap();
    let front = crate::templates::source(
        "Front",
        crate::templates::CAMERA,
        serde_json::json!({ "facing": "front" }),
    );
    let mut two = Scene::new("Two");
    two.objects = vec![crate::templates::object_filling(&ws, &front, Fit::Fill)];
    two.transition = Some(TransitionSettings {
        kind: TransitionKind::Cut,
        duration_ms: 0,
        ..Default::default()
    });
    let two_id = two.id;
    ws.sources.insert(front.id, Arc::new(front));
    ws.scenes.push(Arc::new(two));
    lightair_store::save(&dir, &ws).unwrap();

    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    let studio = session.handle;
    host.studio.store(studio, Ordering::Release);
    wait_for("the back camera", || host.camera.lock().is_some());
    let back = host.camera.lock().unwrap();
    let pool =
        unsafe { crate::gpu::apple::PoolTarget::from_raw(crate::gpu::apple::test_pool(64, 36)) }
            .unwrap();
    unsafe {
        ffi::push_pixel_buffer(
            studio,
            back,
            buffer(&pool, [0, 0, 255, 255]),
            crate::clock::host_now_ns(),
        )
    };

    // the host pulls inside its open calls: told "nothing yet", never "closed"
    session.start_recording(false);
    wait_for("the recording", || host.recording.lock().is_some());
    assert_eq!(*host.pulled_in_open.lock(), vec![0, 0]);

    // switching to the front camera's scene: scene 1 stays until the front camera's first frame
    let one = session.state().program;
    session.switch_scene(two_id);
    wait_for("the front camera", || {
        host.camera.lock().is_some_and(|c| c != back)
    });
    let new = host.camera.lock().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let p = session.hot().program();
    assert_eq!(p.scene, one, "held until the new camera sends");
    assert_eq!(
        p.frozen,
        vec![back],
        "the old camera's last frame kept, not dimmed"
    );
    assert_eq!(
        session.state().program,
        two_id,
        "the interface already shows the switch"
    );
    unsafe {
        ffi::push_pixel_buffer(
            studio,
            new,
            buffer(&pool, [0, 255, 0, 255]),
            crate::clock::host_now_ns(),
        )
    };
    wait_for("the switch", || session.hot().program().scene == two_id);
    // and the old camera's feed goes once the switch is over
    wait_for("the old feed gone", || session.hot().feed(back).is_none());
    session.stop_recording();
    let (rec, _) = host.recording.lock().clone().unwrap();
    session.recording_finished(
        rec,
        RecordingResult::Failed {
            reason: "test".into(),
        },
    );
    session.close();
}

#[test]
fn going_live_to_the_test_destination_steps_with_the_network_and_the_heat() {
    let Some(_turn) = gpu_turn() else { return };
    use crate::governor::Thermal;
    use crate::outputs::{Destination, DestinationKind, OutputState};
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Live").unwrap();
    let host = Arc::new(FakeHost::default());
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, host.clone(), heard.clone())
        .unwrap();
    let studio = session.handle;
    // a 4 Mbps stream into a network that carries 1 Mbps
    let dest = Destination {
        id: "test".into(),
        name: "Test stream".into(),
        kind: DestinationKind::Test {
            upload_kbps: Some(1000),
        },
        video_kbps: 4000,
        audio_kbps: 128,
        height: Some(540),
        fps: None,
        visibility: None,
        network: None,
        delay_s: None,
        give_up_after_s: None,
        format: None,
    };
    session.go_live(vec![dest], false);
    wait_for("the stream's encoder", || !host.opened.lock().is_empty());
    let (video, config) = host.opened.lock()[0].clone();
    assert_eq!(
        (config.width, config.height, config.bitrate_kbps),
        (540, 960, 4000),
        "scaled to 540p"
    );
    // the host encodes: each pulled frame becomes a 4 Mbps-sized packet
    let running = Arc::new(AtomicBool::new(true));
    let encoder = {
        let running = running.clone();
        std::thread::spawn(move || {
            let packet = vec![0u8; 4_000_000 / 8 / 30];
            while running.load(Ordering::Acquire) {
                let (r, buf, pts, key) = ffi::next_video_frame(studio, video, 50);
                if r == 1 {
                    crate::encode::release_buffer(buf);
                    ffi::video_packet(studio, video, &packet, true, pts, key);
                }
                if r == -1 {
                    break;
                }
            }
        })
    };
    wait_for("live", || {
        matches!(session.state().live, LiveState::Live { .. })
    });
    // the queue backs up: congested, the bitrate stepped down through the host
    wait_for("congestion", || {
        host.bitrates
            .lock()
            .iter()
            .any(|(e, k)| *e == video && *k < 4000)
    });
    let reports = || -> Vec<HealthReport> {
        heard
            .0
            .lock()
            .iter()
            .filter_map(|e| match e {
                Event::Health { report } => Some(report.clone()),
                _ => None,
            })
            .collect()
    };
    wait_for("a congested health report", || {
        reports().iter().any(|r| {
            r.outputs
                .first()
                .is_some_and(|o| o.state == OutputState::Congested && o.target_kbps < 4000)
        })
    });
    assert!(
        heard
            .0
            .lock()
            .iter()
            .any(|e| matches!(e, Event::Issue { text, .. } if text.contains("slowed down")))
    );

    // the heat: critical takes step 6 at once (the preview at a quarter of its rate)
    session.device_state(DeviceState {
        thermal: Thermal::Critical,
        ..Default::default()
    });
    wait_for("the governor", || {
        reports().last().is_some_and(|r| r.governor_step == 6)
    });
    assert_eq!(session.hot().preview_every.load(Ordering::Relaxed), 4);
    assert!(
        heard
            .0
            .lock()
            .iter()
            .any(|e| matches!(e, Event::Issue { id, .. } if id == "thermal"))
    );

    session.end_live();
    wait_for("off", || session.state().live == LiveState::Off);
    // the stream's lines go with it
    let open: std::collections::HashSet<String> =
        heard
            .0
            .lock()
            .iter()
            .fold(Default::default(), |mut open, e| {
                match e {
                    Event::Issue { id, .. } => {
                        open.insert(id.clone());
                    }
                    Event::IssueCleared { id } => {
                        open.remove(id);
                    }
                    _ => {}
                }
                open
            });
    assert!(!open.iter().any(|id| id.starts_with("output-")), "{open:?}");
    assert!(session.hot().outputs.read().is_empty());
    running.store(false, Ordering::Release);
    encoder.join().unwrap();
    session.close();
}

#[test]
fn chat_reaches_the_interface_in_batches() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Chat").unwrap();
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, Arc::new(FakeHost::default()), heard.clone())
        .unwrap();
    let channel = lightair_live::Channel {
        platform: "twitch".into(),
        channel: "someone".into(),
    };
    for text in ["hello", "hi there"] {
        let mut e =
            lightair_live::ChatEvent::new("twitch", "someone", lightair_live::Kind::Message);
        e.text = text.into();
        session.chat().hub.inject(channel.clone(), e);
    }
    let got = || -> Vec<String> {
        heard
            .0
            .lock()
            .iter()
            .filter_map(|e| match e {
                Event::Chat { messages } => {
                    Some(messages.iter().map(|m| m.text.clone()).collect::<Vec<_>>())
                }
                _ => None,
            })
            .flatten()
            .collect()
    };
    wait_for("the chat", || got().len() == 2);
    assert_eq!(got(), ["hello", "hi there"]);
    // nothing twice
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(got().len(), 2);
    session.close();
}

#[test]
fn a_follow_in_chat_becomes_an_alert_with_its_sound_feed() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Alerts").unwrap();
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, Arc::new(FakeHost::default()), heard.clone())
        .unwrap();
    // short alerts, so two show in turn
    let mut s = session.alert_settings();
    for r in &mut s.rules {
        r.duration_ms = 1000;
    }
    session.set_alert_settings(s.clone());
    let mut e = lightair_live::ChatEvent::new("twitch", "someone", lightair_live::Kind::Follow);
    e.user = crate::alerts::test_event("follow").user;
    session.chat().hub.inject(
        lightair_live::Channel {
            platform: "twitch".into(),
            channel: "someone".into(),
        },
        e,
    );
    session.test_alert("paid");
    let alerts = || -> Vec<(String, u32)> {
        heard
            .0
            .lock()
            .iter()
            .filter_map(|e| match e {
                Event::Alert { alert, sound_feed } => Some((alert.title.clone(), *sound_feed)),
                _ => None,
            })
            .collect()
    };
    wait_for("the first alert", || !alerts().is_empty());
    assert!(
        alerts()[0].1 > 0 && session.hot().feed(alerts()[0].1).is_some(),
        "an audio feed for its sound"
    );
    assert!(session.hot().alert.read().is_some(), "on for the alert box");
    // one at a time: the second after the first's second
    wait_for("both", || alerts().len() == 2);
    let mut titles: Vec<String> = alerts().into_iter().map(|a| a.0).collect();
    titles.sort();
    assert_eq!(titles, ["Lightair followed!", "Lightair sent $5.00!"]);
    session.close();
    assert_eq!(
        crate::alerts::Settings::load(&projects.dir(&p.id).unwrap()),
        s,
        "kept with the project"
    );
}

#[test]
fn a_layers_effects_are_added_changed_reordered_and_removed() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Effects").unwrap();
    let session = projects
        .open_session(
            &p.id,
            Arc::new(FakeHost::default()),
            Arc::new(Heard::default()),
        )
        .unwrap();
    let layer = session.state().scene().unwrap().objects[0].id;
    let effects = |s: &Session| {
        let st = s.state();
        let src = st.scene().unwrap().objects[0].source.unwrap();
        st.workspace.sources[&src].effects.clone()
    };
    let v = session.state().version;
    session.add_effect(
        layer,
        "fx.vignette",
        Some(serde_json::json!({ "amount": 0.3 })),
    );
    session.add_effect(layer, "color_filter_v2", None);
    wait_for("two effects", || {
        session.state().version > v && effects(&session).len() == 2
    });
    let e = effects(&session);
    assert_eq!(
        e[0].settings["amount"], 0.3,
        "the preset's setting over the defaults"
    );
    session.set_effect(layer, e[1].id, serde_json::json!({ "saturation": -1.0 }));
    session.set_effect_enabled(layer, e[0].id, false);
    session.move_effect(layer, e[1].id, 0);
    wait_for("reordered", || effects(&session)[0].id == e[1].id);
    let now = effects(&session);
    assert_eq!(now[0].settings["saturation"], -1.0);
    assert!(!now[1].enabled);
    assert_eq!(session.state().undo.as_deref(), Some("Move effect"));
    session.remove_effect(layer, e[0].id);
    wait_for("one left", || effects(&session).len() == 1);
    session.close();
}

#[test]
fn a_text_layers_box_follows_its_text() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Text").unwrap();
    let session = projects
        .open_session(
            &p.id,
            Arc::new(FakeHost::default()),
            Arc::new(Heard::default()),
        )
        .unwrap();
    let spec = |text: &str| TextSpec {
        text: text.into(),
        font_family: None,
        size: 64.0,
        color: 0xFFFFFF,
        bold: false,
        align: 0,
        outline: None,
        background: None,
    };
    session.add_layer(NewLayer::Text { text: spec("Hi") });
    let text_box = |s: &Session| {
        let st = s.state();
        let canvas = st.canvas;
        st.scene()?
            .objects
            .iter()
            .find(|o| {
                o.source
                    .is_some_and(|id| st.workspace.sources[&id].kind == crate::templates::TEXT)
            })
            .map(|o| (o.id, o.layouts[&canvas]))
    };
    wait_for("the text layer", || text_box(&session).is_some());
    let (layer, before) = text_box(&session).unwrap();
    // at its drawn size: a short word is a short box
    assert!(before.width < before.height * 3.0, "{before:?}");
    session.set_text(layer, &spec("Hi there, everyone"));
    wait_for("the box to follow", || {
        text_box(&session).unwrap().1.width > before.width * 2.0
    });
    let after = text_box(&session).unwrap().1;
    assert!((after.height - before.height).abs() < 2.0, "{after:?}");
    assert_eq!(after.x, before.x, "held at the left, as it's aligned");
    assert_eq!(after.y, before.y);
    assert_eq!(session.state().undo.as_deref(), Some("Change text"));
}

#[test]
fn in_the_background_a_stream_repeats_its_cover() {
    let Some(_turn) = gpu_turn() else { return };
    use crate::outputs::{Destination, DestinationKind};
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Away").unwrap();
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    let studio = session.handle;
    session.go_live(
        vec![Destination {
            id: "test".into(),
            name: "Test stream".into(),
            kind: DestinationKind::Test { upload_kbps: None },
            video_kbps: 2000,
            audio_kbps: 128,
            height: Some(360),
            fps: None,
            visibility: None,
            network: None,
            delay_s: None,
            give_up_after_s: None,
            format: None,
        }],
        false,
    );
    wait_for("the stream's encoder", || !host.opened.lock().is_empty());
    let video = host.opened.lock()[0].0;
    // the host encodes: a keyframe's packet says so in its first byte
    let running = Arc::new(AtomicBool::new(true));
    let encoder = {
        let running = running.clone();
        std::thread::spawn(move || {
            while running.load(Ordering::Acquire) {
                let (r, buf, pts, key) = ffi::next_video_frame(studio, video, 50);
                if r == 1 {
                    crate::encode::release_buffer(buf);
                    ffi::video_packet(studio, video, &[key as u8, 0, 0, 0], true, pts, key);
                }
                if r == -1 {
                    break;
                }
            }
        })
    };
    wait_for("live", || {
        matches!(session.state().live, LiveState::Live { .. })
    });
    let e = session.hot().video_encoder(video).unwrap();
    session.lifecycle(false);
    let held = e.held.lock().packet.clone();
    assert_eq!(held, Some((vec![1, 0, 0, 0], true)), "the cover's keyframe");
    // no more frames are drawn, yet the stream goes on: the cover, twice a second
    let keyframes = e.sent.lock().keyframes;
    wait_for("the cover again", || {
        e.sent.lock().keyframes >= keyframes + 2
    });
    session.lifecycle(true);
    wait_for("back", || e.held.lock().packet.is_none());
    session.end_live();
    running.store(false, Ordering::Release);
    encoder.join().unwrap();
    session.close();
}

#[test]
fn each_setting_streams_from_its_own_encode_and_steps_alone() {
    let Some(_turn) = gpu_turn() else { return };
    use crate::outputs::{Destination, DestinationKind};
    let tmp = tempfile::tempdir().unwrap();
    // a class A phone: three encodes at once
    let device = DeviceInfo {
        platform: "ios".into(),
        model: "iPhone16,1".into(),
        ..Default::default()
    };
    let projects = Projects::open(tmp.path(), None, tmp.path(), device).unwrap();
    let p = projects.create("Two").unwrap();
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    let studio = session.handle;
    let dest = |id: &str, upload: Option<u32>, kbps: u32, height: u32| Destination {
        id: id.into(),
        name: id.into(),
        kind: DestinationKind::Test {
            upload_kbps: upload,
        },
        video_kbps: kbps,
        audio_kbps: 128,
        height: Some(height),
        fps: None,
        visibility: None,
        network: None,
        delay_s: None,
        give_up_after_s: None,
        format: None,
    };
    // the slow network's destination and a fast one, at different sizes; a third shares the
    // fast one's settings
    let dests = vec![
        dest("slow", Some(800), 3000, 540),
        dest("fast", None, 2000, 360),
        dest("same", None, 2000, 360),
    ];
    let plan = session.stream_plan(&dests, false);
    assert!(plan.allowed);
    assert_eq!(plan.encodes.len(), 2);
    assert_eq!(plan.encodes[1].destinations, ["fast", "same"]);
    session.go_live(dests, false);
    wait_for("two encoders", || host.opened.lock().len() == 2);
    let opened = host.opened.lock().clone();
    let (slow, fast) = (opened[0].0, opened[1].0);
    assert_eq!((opened[0].1.width, opened[1].1.width), (540, 360));
    let running = Arc::new(AtomicBool::new(true));
    let encoders: Vec<_> = [(slow, 3000u32), (fast, 2000)]
        .into_iter()
        .map(|(video, kbps)| {
            let running = running.clone();
            std::thread::spawn(move || {
                let packet = vec![0u8; kbps as usize * 1000 / 8 / 30];
                while running.load(Ordering::Acquire) {
                    let (r, buf, pts, key) = ffi::next_video_frame(studio, video, 50);
                    if r == 1 {
                        crate::encode::release_buffer(buf);
                        ffi::video_packet(studio, video, &packet, true, pts, key);
                    }
                    if r == -1 {
                        break;
                    }
                }
            })
        })
        .collect();
    wait_for("the slow one stepped down", || {
        host.bitrates
            .lock()
            .iter()
            .any(|(e, k)| *e == slow && *k < 3000)
    });
    assert!(
        !host
            .bitrates
            .lock()
            .iter()
            .any(|(e, k)| *e == fast && *k < 2000),
        "the fast one's encode isn't slowed: {:?}",
        host.bitrates.lock()
    );
    session.end_live();
    wait_for("off", || session.state().live == LiveState::Off);
    running.store(false, Ordering::Release);
    for e in encoders {
        e.join().unwrap();
    }
    session.close();
}

/// The iOS harness's four destinations (Stream Check, 22 November 2026): ordered start, the
/// first at 720p and the rest at 540p sharing a second encode. Every encoder gets frames and
/// every destination gets pictures, the first one's too.
#[test]
fn four_destinations_in_order_each_get_pictures_from_their_encode() {
    let Some(_turn) = gpu_turn() else { return };
    use crate::outputs::{Destination, DestinationKind};
    let tmp = tempfile::tempdir().unwrap();
    let device = DeviceInfo {
        platform: "ios".into(),
        model: "iPhone16,1".into(),
        ..Default::default()
    };
    let projects = Projects::open(tmp.path(), None, tmp.path(), device).unwrap();
    let p = projects.create("Four").unwrap();
    let host = Arc::new(FakeHost::default());
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, host.clone(), heard.clone())
        .unwrap();
    host.pull_loops.store(true, Ordering::Release);
    host.studio.store(session.handle, Ordering::Release);
    let dest = |n: u32| Destination {
        id: format!("d{n}"),
        name: format!("D{n}"),
        kind: DestinationKind::Test { upload_kbps: None },
        video_kbps: if n == 1 { 3500 } else { 1800 },
        audio_kbps: 128,
        height: Some(if n == 1 { 720 } else { 540 }),
        fps: None,
        visibility: None,
        network: None,
        delay_s: None,
        give_up_after_s: None,
        format: None,
    };
    session.go_live_ordered((1..=4).map(dest).collect(), false);
    let all_on = || session.hot().outputs.read().len() == 4;
    wait_for("all four on air", all_on);
    let opened = host.opened.lock().clone();
    assert_eq!(
        opened
            .iter()
            .map(|(_, c)| (c.width, c.height))
            .collect::<Vec<_>>(),
        [(720, 1280), (540, 960)]
    );
    // a few seconds with all four on: both encoders keep getting frames
    let before = host.frames.lock().clone();
    std::thread::sleep(Duration::from_secs(3));
    let after = host.frames.lock().clone();
    assert!(
        host.ended.lock().is_empty(),
        "a pull loop was told its encoder was closed: {:?}",
        host.ended.lock()
    );
    for (video, c) in &opened {
        let n = after.get(video).copied().unwrap_or(0) - before.get(video).copied().unwrap_or(0);
        assert!(
            n >= 30,
            "the {}x{} encoder got {n} frames in 3 s: {after:?}",
            c.width,
            c.height
        );
    }
    // and every destination its pictures (a health report from after the three seconds)
    let reports = heard.0.lock().len();
    wait_for("a new health report", || {
        heard.0.lock()[reports..]
            .iter()
            .any(|e| matches!(e, Event::Health { .. }))
    });
    let report = heard
        .0
        .lock()
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Health { report } => Some(report.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(report.outputs.len(), 4);
    for o in &report.outputs {
        assert!(
            o.sent_kbps > 0,
            "{} sent nothing: {:?}",
            o.id,
            report.outputs
        );
    }
    session.end_live();
    wait_for("off", || session.state().live == LiveState::Off);
    for t in std::mem::take(&mut *host.loops.lock()) {
        t.join().unwrap();
    }
    session.close();
}

#[test]
fn below_the_bitrate_floor_the_picture_steps_down_a_size() {
    let Some(_turn) = gpu_turn() else { return };
    use crate::outputs::{Destination, DestinationKind};
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Floor").unwrap();
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    let studio = session.handle;
    // 3 Mbps chosen, a network of 400 kbps: the bitrate floor (900) isn't enough
    session.go_live(
        vec![Destination {
            id: "slow".into(),
            name: "Slow".into(),
            kind: DestinationKind::Test {
                upload_kbps: Some(400),
            },
            video_kbps: 3000,
            audio_kbps: 128,
            height: None,
            fps: None,
            visibility: None,
            network: None,
            delay_s: None,
            give_up_after_s: None,
            format: None,
        }],
        false,
    );
    wait_for("the encoder", || !host.opened.lock().is_empty());
    let first = host.opened.lock()[0].clone();
    let running = Arc::new(AtomicBool::new(true));
    let encoder = {
        let (running, host) = (running.clone(), host.clone());
        std::thread::spawn(move || {
            while running.load(Ordering::Acquire) {
                // whichever encoder is the stream's now, at the bitrate it was last given
                let (video, kbps) = {
                    let opened = host.opened.lock();
                    let (id, c) = opened.last().unwrap().clone();
                    let set = host
                        .bitrates
                        .lock()
                        .iter()
                        .rev()
                        .find(|(e, _)| *e == id)
                        .map(|b| b.1);
                    (id, set.unwrap_or(c.bitrate_kbps))
                };
                let (r, buf, pts, key) = ffi::next_video_frame(studio, video, 20);
                if r == 1 {
                    crate::encode::release_buffer(buf);
                    let packet = vec![0u8; kbps as usize * 1000 / 8 / 30];
                    ffi::video_packet(studio, video, &packet, true, pts, key);
                }
            }
        })
    };
    wait_for("a smaller picture", || host.opened.lock().len() >= 2);
    let (second, config) = host.opened.lock()[1].clone();
    // the next rung below the chosen size (1080 → 720, 720 → 540)
    let chosen = first.1.width.min(first.1.height);
    let next = if chosen > 720 { 720 } else { 540 };
    assert_eq!(config.width.min(config.height), next, "{config:?}");
    // at what the network carried (400 kbps less the sound), the smaller picture's floor at
    // least: under the old floor (900), which the network couldn't carry
    assert!(
        (300..=400).contains(&config.bitrate_kbps),
        "{}",
        config.bitrate_kbps
    );
    wait_for("the health says so", || {
        let h = session.hot().outputs.read()[0].video_encoder();
        h == second
    });
    session.end_live();
    running.store(false, Ordering::Release);
    encoder.join().unwrap();
    session.close();
}

#[test]
fn a_background_is_replaced_removed_and_blurred_behind_the_person() {
    let Some(_turn) = gpu_turn() else { return };
    use crate::background::{Look, Pipelines, State, draw};
    let gpu = crate::gpu::Gpu::new().unwrap();
    // the camera: a red pixel buffer, sampled as it is (the iOS path)
    let pool =
        unsafe { crate::gpu::apple::PoolTarget::from_raw(crate::gpu::apple::test_pool(64, 32)) }
            .unwrap();
    let raw = buffer(&pool, [0, 0, 255, 255]);
    let kept = unsafe { crate::gpu::apple::Kept::retain(raw) }.unwrap();
    let _owned = unsafe { lightair_render::texture::PixelBuffer::from_raw(raw) }.unwrap();
    let camera = crate::gpu::apple::sampled(gpu.device(), &kept.0).unwrap();
    // the person on the left half
    let mask = crate::feeds::Mask {
        bytes: (0..32 * 16)
            .map(|i| if i % 32 < 16 { 255 } else { 0 })
            .collect(),
        size: (32, 16),
        rotation: 0,
        mirrored: false,
        serial: 1,
    };
    let p = Pipelines::new(gpu.device());
    let mut st = State::default();
    let look = |mode| Look {
        mode,
        blur: 60.0,
        feather: 0.0,
        shift: 0.0,
        colour: 0xFF00_FF00, // green, 0xAABBGGRR
        image: None,
        quality: 1,
    };
    let px = |t: &wgpu::Texture, x: usize, y: usize| {
        let rgba = lightair_render::compose::read_texture(t).unwrap();
        let i = (y * t.width() as usize + x) * 4;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    };
    let out = draw(&gpu, &p, &mut st, &camera, &mask, None, &look(3));
    assert_eq!(px(&out, 4, 16), [255, 0, 0, 255], "the person");
    assert_eq!(px(&out, 60, 16), [0, 255, 0, 255], "the colour behind");
    let out = draw(&gpu, &p, &mut st, &camera, &mask, None, &look(2));
    assert_eq!(px(&out, 4, 16)[3], 255);
    assert_eq!(px(&out, 60, 16)[3], 0, "removed: what's below shows");
    // a plain red camera blurred is still red behind
    let out = draw(&gpu, &p, &mut st, &camera, &mask, None, &look(1));
    let behind = px(&out, 60, 16);
    assert!(behind[0] > 240 && behind[1] < 10, "{behind:?}");
}

#[test]
fn a_cameras_background_effect_asks_the_host_for_masks() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Masks").unwrap();
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    let studio = session.handle;
    wait_for("the camera", || host.camera.lock().is_some());
    let cam = host.camera.lock().unwrap();
    let running = Arc::new(AtomicBool::new(true));
    let pusher = {
        let running = running.clone();
        std::thread::spawn(move || {
            let pool = unsafe {
                crate::gpu::apple::PoolTarget::from_raw(crate::gpu::apple::test_pool(64, 36))
            }
            .unwrap();
            while running.load(Ordering::Acquire) {
                let now = crate::clock::host_now_ns();
                unsafe {
                    ffi::push_pixel_buffer(studio, cam, buffer(&pool, [0, 0, 255, 255]), now)
                };
                // the host segments only while asked
                if ffi::mask_wanted(studio, cam) >= 0 {
                    let m = [255u8; 16 * 9];
                    unsafe { ffi::push_mask(studio, cam, m.as_ptr(), 16, 9, 16, 0, false) };
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };
    // no background effect: no masks
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(ffi::mask_wanted(studio, cam), -1);
    let camera_layer = {
        let st = session.state();
        let scene = st.scene().unwrap();
        scene
            .objects
            .iter()
            .find(|o| {
                o.source
                    .is_some_and(|s| st.workspace.sources[&s].kind == crate::templates::CAMERA)
            })
            .map(|o| o.id)
            .unwrap()
    };
    session.add_effect(
        camera_layer,
        "studio.bg_blur",
        Some(serde_json::json!({ "bg_quality": 2 })),
    );
    wait_for("masks asked for, accurate", || {
        ffi::mask_wanted(studio, cam) == 2
    });
    wait_for("a mask in", || {
        session
            .hot()
            .feed(cam)
            .is_some_and(|f| f.video().is_some_and(|v| v.mask.lock().is_some()))
    });
    // the effect off: masks stop being asked for within a second
    let effect = {
        let st = session.state();
        let src = st
            .scene()
            .unwrap()
            .object(camera_layer)
            .unwrap()
            .source
            .unwrap();
        st.workspace.sources[&src].effects[0].id
    };
    session.set_effect_enabled(camera_layer, effect, false);
    wait_for("masks no longer asked for", || {
        ffi::mask_wanted(studio, cam) == -1
    });
    running.store(false, Ordering::Release);
    pusher.join().unwrap();
    session.close();
}

#[test]
fn a_screen_layer_has_its_feeds_and_its_own_track() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Screen").unwrap();
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, Arc::new(FakeHost::default()), heard.clone())
        .unwrap();
    let studio = session.handle;
    assert!(session.screen_feeds().is_none());
    session.add_layer(NewLayer::Screen { with_sound: true });
    wait_for("the screen's feeds", || session.screen_feeds().is_some());
    let feeds = session.screen_feeds().unwrap();
    let audio = feeds.audio.expect("its sound");
    let st = session.state();
    assert_eq!(
        st.workspace.sources[&feeds.source].kind,
        crate::templates::SCREEN
    );
    assert!(
        st.workspace.tracks.iter().any(|t| t.source == feeds.source),
        "its own track"
    );
    // Android's frames and the screen's sound go in
    let frame = vec![200u8; 32 * 4 * 18];
    let now = crate::clock::host_now_ns();
    assert!(unsafe { ffi::push_rgba(studio, feeds.video, frame.as_ptr(), 32, 18, 32 * 4, now) });
    assert!(ffi::push_audio(studio, audio, &[0.1; 960], 2, 48_000, now));
    // a still screen sends nothing: its last frame stands, not drawn paused
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        crate::render::dim_paused_for_test(&session, crate::clock::host_now_ns()).is_none(),
        "a quiet screen isn't paused"
    );
    // a second screen layer shows the same screen
    session.add_layer(NewLayer::Screen { with_sound: true });
    wait_for("two layers", || {
        session
            .state()
            .scene()
            .unwrap()
            .objects
            .iter()
            .filter(|o| o.source == Some(feeds.source))
            .count()
            == 2
    });
    assert_eq!(session.screen_feeds(), Some(feeds), "the same feeds");
    // sharing stopped: said
    session.feed_state(feeds.video, FeedState::Stopped);
    wait_for("the issue", || {
        heard.0.lock().iter().any(|e| {
            matches!(e, Event::Issue { id, text, .. } if id == "screen" && text.contains("stopped"))
        })
    });
    session.close();
}

#[test]
fn an_imported_widget_is_a_web_layer_the_host_draws() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Web").unwrap();
    let session = projects
        .open_session(
            &p.id,
            Arc::new(FakeHost::default()),
            Arc::new(Heard::default()),
        )
        .unwrap();
    let studio = session.handle;
    // a native widget is drawn by the engine: no web view
    session.add_layer(NewLayer::Widget {
        kind: "chat-box".into(),
    });
    // an imported one (StreamElements-style files in a folder)
    let w = tmp.path().join("my-widget");
    std::fs::create_dir_all(&w).unwrap();
    std::fs::write(
        w.join("widget.html"),
        "<div id=\"hello\">Hello from the widget</div>",
    )
    .unwrap();
    std::fs::write(w.join("widget.css"), "#hello { color: red }").unwrap();
    session.add_layer(NewLayer::ImportedWidget {
        path: w.to_string_lossy().into(),
    });
    wait_for("the web layer", || !session.state().web.is_empty());
    let web = session.state().web.clone();
    assert_eq!(web.len(), 1, "only the imported one");
    let page = &web[0];
    assert_eq!((page.width, page.height), (800, 600));
    assert!(page.url.starts_with("http://127.0.0.1:"), "{}", page.url);
    // the widget server serves its page
    let body = lightair_live::net::get(&lightair_live::net::agent(), &page.url, &[]).unwrap();
    assert!(body.contains("Hello from the widget"), "{body}");
    // the host's web view's frames go to its feed
    let frame = vec![255u8; 16 * 4 * 12];
    assert!(unsafe {
        ffi::push_rgba(
            studio,
            page.feed,
            frame.as_ptr(),
            16,
            12,
            64,
            crate::clock::host_now_ns(),
        )
    });
    session.close();
}

#[test]
fn destinations_start_in_order_and_come_and_go_while_live() {
    let Some(_turn) = gpu_turn() else { return };
    use crate::outputs::{Destination, DestinationKind};
    let tmp = tempfile::tempdir().unwrap();
    let device = DeviceInfo {
        platform: "ios".into(),
        model: "iPhone16,1".into(),
        ..Default::default()
    };
    let projects = Projects::open(tmp.path(), None, tmp.path(), device).unwrap();
    let p = projects.create("Order").unwrap();
    let host = Arc::new(FakeHost::default());
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, host.clone(), heard.clone())
        .unwrap();
    let dest = |id: &str, kind: DestinationKind, height: u32| Destination {
        id: id.into(),
        name: id.to_uppercase(),
        kind,
        video_kbps: 2000,
        audio_kbps: 128,
        height: Some(height),
        fps: None,
        visibility: None,
        network: None,
        delay_s: None,
        give_up_after_s: None,
        format: None,
    };
    let test = || DestinationKind::Test { upload_kbps: None };
    let bad = DestinationKind::Rtmp {
        url: "http://not-rtmp".into(),
        key: "k".into(),
    };
    let live_ids = |s: &Session| -> Vec<String> {
        let mut v: Vec<String> = s
            .hot()
            .outputs
            .read()
            .iter()
            .map(|o| o.dest.id.clone())
            .collect();
        v.sort();
        v
    };
    session.go_live_ordered(
        vec![
            dest("a", test(), 360),
            dest("b", bad, 360),
            dest("c", test(), 360),
        ],
        false,
    );
    // a goes on air, then b can't start: c waits
    wait_for("b blocked", || {
        heard.0.lock().iter().any(|e| {
            matches!(e, Event::Issue { id, text, .. } if id == "start" && text.starts_with("B didn't start"))
        })
    });
    assert_eq!(live_ids(&session), ["a"]);
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(live_ids(&session), ["a"], "c waits for the decision");
    session.skip_destination("b");
    wait_for("c on", || live_ids(&session) == ["a", "c"]);
    assert_eq!(host.opened.lock().len(), 1, "a and c share an encode");
    // one stopped while the other goes on, then started again
    session.stop_destination("a");
    wait_for("a off", || live_ids(&session) == ["c"]);
    session.start_destination("a");
    wait_for("a back", || live_ids(&session) == ["a", "c"]);
    // one added with its own size: a second encode
    session.add_destination(dest("d", test(), 540));
    wait_for("d on", || live_ids(&session) == ["a", "c", "d"]);
    assert_eq!(host.opened.lock().len(), 2);
    // reordered: the health list follows
    let order = |s: &Session| -> Vec<String> {
        s.hot()
            .outputs
            .read()
            .iter()
            .map(|o| o.dest.id.clone())
            .collect()
    };
    session.reorder_destinations(vec!["d".into(), "c".into(), "a".into()]);
    wait_for("the new order", || order(&session) == ["d", "c", "a"]);
    // "Match settings": d takes a's size, so they share one encode; its place stays
    session.update_destination(dest("d", test(), 360));
    wait_for("d on the shared encode", || {
        let outputs = session.hot().outputs.read().clone();
        outputs.len() == 3
            && outputs
                .iter()
                .all(|o| o.video_encoder() == outputs[0].video_encoder())
    });
    assert_eq!(order(&session), ["d", "c", "a"]);
    // stopped: the rest go on
    session.stop_destination("d");
    wait_for("d off", || live_ids(&session) == ["a", "c"]);
    let report = || {
        heard.0.lock().iter().rev().find_map(|e| match e {
            Event::Health { report } => Some(report.clone()),
            _ => None,
        })
    };
    wait_for("the health says d is stopped", || {
        report().is_some_and(|r| r.stopped.contains(&"d".to_string()))
    });
    // dropped frames by cause: nothing dropped by the network here
    let r = report().unwrap();
    assert_eq!(r.dropped_network, 0);
    assert!(r.outputs.iter().all(|o| o.dropped_frames == 0));
    session.end_live();
    wait_for("off", || session.state().live == LiveState::Off);
    session.close();
}

#[test]
fn a_destination_goes_on_its_own_network() {
    let Some(_turn) = gpu_turn() else { return };
    use crate::outputs::{Destination, DestinationKind};
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Network").unwrap();
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, Arc::new(FakeHost::default()), heard.clone())
        .unwrap();
    // the "Wi-Fi" here is the loopback interface, where a pretend RTMP server listens
    session.device_state(DeviceState {
        networks: vec![NetworkPath {
            kind: NetworkKind::Wifi,
            interface: "lo0".into(),
            handle: 0,
        }],
        ..Default::default()
    });
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    server.set_nonblocking(true).unwrap();
    let dest = |id: &str, network| Destination {
        id: id.into(),
        name: id.into(),
        kind: DestinationKind::Rtmp {
            url: format!("rtmp://127.0.0.1:{port}/live"),
            key: "k".into(),
        },
        video_kbps: 1000,
        audio_kbps: 128,
        height: Some(360),
        fps: None,
        visibility: None,
        network: Some(network),
        delay_s: None,
        give_up_after_s: None,
        format: None,
    };
    session.go_live(
        vec![
            dest("wifi", NetworkKind::Wifi),
            dest("cell", NetworkKind::Cellular),
        ],
        false,
    );
    wait_for("a connection on the Wi-Fi", || server.accept().is_ok());
    wait_for("mobile data said missing", || {
        heard.0.lock().iter().any(|e| {
            matches!(e, Event::Issue { id, text, .. } if id == "output-cell" && text.contains("Mobile data isn't available"))
        })
    });
    session.end_live();
    session.close();
}

#[test]
fn a_clip_of_the_last_seconds_is_saved_from_the_stream() {
    let Some(_turn) = gpu_turn() else { return };
    use crate::outputs::{Destination, DestinationKind};
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Clips").unwrap();
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    let studio = session.handle;
    assert!(session.save_clip(5).is_err(), "not live");
    session.go_live(
        vec![Destination {
            id: "t".into(),
            name: "Test".into(),
            kind: DestinationKind::Test { upload_kbps: None },
            video_kbps: 1000,
            audio_kbps: 128,
            height: Some(360),
            fps: None,
            visibility: None,
            network: None,
            delay_s: None,
            give_up_after_s: None,
            format: None,
        }],
        false,
    );
    wait_for("the encoder", || !host.opened.lock().is_empty());
    let video = host.opened.lock()[0].0;
    wait_for("live", || !session.hot().outputs.read().is_empty());
    let audio = session.hot().outputs.read()[0].audio_encoder;
    ffi::parameter_sets(
        studio,
        video,
        vec![
            vec![0x67, 0x42, 0xe0, 0x1f, 0xda],
            vec![0x68, 0xce, 0x3c, 0x80],
        ],
    );
    // 3 s of the host's packets: pictures (a keyframe a second) and sound
    for i in 0..90i64 {
        let key = i % 30 == 0;
        let nal = if key { 0x65 } else { 0x41 };
        ffi::video_packet(
            studio,
            video,
            &[0, 0, 0, 3, nal, 1, 2],
            true,
            i * 33_333_333,
            key,
        );
        ffi::audio_packet(studio, audio, &[0x21, 0x10, 4], i * 33_333_333);
    }
    let path = session.save_clip(2).unwrap();
    assert!(path.starts_with(crate::clips::folder(tmp.path())));
    let bytes = std::fs::read(&path).unwrap();
    let mp4 =
        mp4::Mp4Reader::read_header(std::io::Cursor::new(&bytes), bytes.len() as u64).unwrap();
    let v = mp4
        .tracks()
        .values()
        .find(|t| t.track_type().unwrap() == mp4::TrackType::Video)
        .unwrap();
    // from the keyframe at 0 s or 1 s: 2–3 s of pictures
    assert!(
        (59..=90).contains(&v.sample_count()),
        "{}",
        v.sample_count()
    );
    session.end_live();
    session.close();
}

#[test]
fn with_the_screen_captured_in_the_background_the_program_goes_on() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Background").unwrap();
    let session = projects
        .open_session(
            &p.id,
            Arc::new(FakeHost::default()),
            Arc::new(Heard::default()),
        )
        .unwrap();
    session.add_layer(NewLayer::Screen { with_sound: false });
    wait_for("the screen", || session.screen_feeds().is_some());
    let frames = || session.hot().stats.frames.load(Ordering::Relaxed);
    wait_for("frames", || frames() > 2);
    session.set_background_capture(true);
    session.lifecycle(false);
    let before = frames();
    std::thread::sleep(Duration::from_millis(300));
    assert!(frames() > before, "still drawing in the background");
    // without it: drawing stops
    session.lifecycle(true);
    session.set_background_capture(false);
    session.lifecycle(false);
    std::thread::sleep(Duration::from_millis(100));
    let stopped = frames();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(frames(), stopped, "paused");
    session.lifecycle(true);
    session.close();
}

#[test]
fn a_primed_texture_is_cleared_and_sampled_through_wgpu() {
    let Some(_turn) = gpu_turn() else { return };
    let gpu = crate::gpu::Gpu::new().unwrap();
    let t = gpu.target((8, 4), wgpu::TextureFormat::Rgba8Unorm);
    gpu.prime(&t);
    let px = lightair_render::compose::read_texture(&t).unwrap();
    assert_eq!(&px[..4], &[0, 0, 0, 255], "cleared black");
}

#[test]
fn an_external_camera_layer_opens_that_camera() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("USB").unwrap();
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    wait_for("the built-in camera", || {
        host.calls
            .lock()
            .iter()
            .any(|c| c.starts_with("start_camera") && c.ends_with("None"))
    });
    // a scene of its own with the capture card
    let first = session.state().program;
    session.add_scene("Card", None);
    wait_for("the scene", || session.state().workspace.scenes.len() == 2);
    let card = session
        .state()
        .workspace
        .scenes
        .iter()
        .find(|s| s.id != first)
        .unwrap()
        .id;
    session.switch_scene(card);
    wait_for("on the card's scene", || session.state().program == card);
    session.add_layer(NewLayer::ExternalCamera {
        device: "usb-cam-1".into(),
        name: "Cam Link".into(),
    });
    wait_for("the card opened", || {
        host.calls
            .lock()
            .iter()
            .any(|c| c.starts_with("start_camera") && c.ends_with("Some(\"usb-cam-1\")"))
    });
    // back: the built-in camera again
    let starts = || {
        host.calls
            .lock()
            .iter()
            .filter(|c| c.starts_with("start_camera") && c.ends_with("None"))
            .count()
    };
    let before = starts();
    session.switch_scene(first);
    wait_for("the built-in again", || starts() > before);
    session.close();
}

#[test]
fn a_bonded_destination_sends_over_each_network_and_follows_them() {
    let Some(_turn) = gpu_turn() else { return };
    use crate::outputs::{Destination, DestinationKind};
    use lightair_feed::srtla::{ID_LEN, KEEPALIVE, REG1, REG2, REG3};
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Bonded").unwrap();
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, Arc::new(FakeHost::default()), heard.clone())
        .unwrap();
    // "Wi-Fi" and "Mobile data" are both the loopback interface here
    let lo = |kind| NetworkPath {
        kind,
        interface: "lo0".into(),
        handle: 0,
    };
    session.device_state(DeviceState {
        networks: vec![lo(NetworkKind::Wifi), lo(NetworkKind::Cellular)],
        ..Default::default()
    });
    // an SRTLA receiver that registers links and echoes keepalives (no SRT behind it)
    let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = rx.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        let mut group = None;
        while let Ok((n, from)) = rx.recv_from(&mut buf) {
            let p = &buf[..n];
            match u16::from_be_bytes([p[0], p[1]]) {
                REG1 if n == 2 + ID_LEN => {
                    let mut id = p[2..].to_vec();
                    id[ID_LEN / 2..].fill(1);
                    let mut r = REG2.to_be_bytes().to_vec();
                    r.extend_from_slice(&id);
                    group = Some(id);
                    let _ = rx.send_to(&r, from);
                }
                REG2 if group.as_deref() == Some(&p[2..]) => {
                    let _ = rx.send_to(&REG3.to_be_bytes(), from);
                }
                KEEPALIVE => {
                    let _ = rx.send_to(p, from);
                }
                _ => {}
            }
        }
    });
    session.go_live(
        vec![Destination {
            id: "bonded".into(),
            name: "Bonded".into(),
            kind: DestinationKind::Srtla {
                url: format!("srtla://127.0.0.1:{port}?streamid=live"),
            },
            video_kbps: 1000,
            audio_kbps: 128,
            height: Some(360),
            fps: None,
            visibility: None,
            network: None,
            delay_s: None,
            give_up_after_s: None,
            format: None,
        }],
        false,
    );
    let links = || {
        heard.0.lock().iter().rev().find_map(|e| match e {
            Event::Health { report } => report
                .outputs
                .iter()
                .find(|o| o.id == "bonded")
                .map(|o| o.links.clone()),
            _ => None,
        })
    };
    let up = |names: &[&str]| {
        links().is_some_and(|l| {
            l.len() == names.len()
                && l.iter().all(|l| l.up)
                && names.iter().all(|n| l.iter().any(|l| l.name == *n))
        })
    };
    wait_for("both networks up", || up(&["Wi-Fi", "Mobile data"]));
    // Wi-Fi goes away: its link stops, mobile data carries on
    session.device_state(DeviceState {
        networks: vec![lo(NetworkKind::Cellular)],
        ..Default::default()
    });
    wait_for("mobile data alone", || up(&["Mobile data"]));
    session.end_live();
    wait_for("off", || session.state().live == LiveState::Off);
    session.close();
}

#[test]
fn both_cameras_run_at_once_where_the_phone_can() {
    let Some(_turn) = gpu_turn() else { return };
    let run = |multi_camera: bool| {
        let tmp = tempfile::tempdir().unwrap();
        let device = DeviceInfo {
            multi_camera,
            ..Default::default()
        };
        let projects = Projects::open(tmp.path(), None, tmp.path(), device).unwrap();
        let p = projects.create("Two").unwrap();
        let host = Arc::new(FakeHost::default());
        let session = projects
            .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
            .unwrap();
        let calls = || host.calls.lock().clone();
        let starts = |facing: &str| {
            calls()
                .iter()
                .filter(|c| c.starts_with("start_camera") && c.contains(facing))
                .count()
        };
        wait_for("the back camera", || starts("Back") == 1);
        // a front camera layer in the same scene
        session.add_layer(NewLayer::Camera {
            facing: Facing::Front,
        });
        wait_for("the layer", || {
            let ws = session.state().workspace.clone();
            ws.sources
                .values()
                .filter(|s| s.kind == crate::templates::CAMERA)
                .count()
                == 2
        });
        std::thread::sleep(Duration::from_millis(300));
        let out = (starts("Front"), calls());
        session.close();
        out
    };
    // one at a time: the scene's first camera only, the back one left running
    let (front, calls) = run(false);
    assert_eq!(front, 0, "{calls:?}");
    assert!(
        !calls.iter().any(|c| c.starts_with("stop_camera")),
        "{calls:?}"
    );
    // two at once: the front one starts as well, at most 1080p30, and the back one carries on
    let (front, calls) = run(true);
    assert_eq!(front, 1, "{calls:?}");
    let started = calls.iter().find(|c| c.contains("Front")).unwrap();
    assert!(
        started.contains("p30 ") && !started.contains(" 1920p"),
        "{started}"
    );
    assert!(
        !calls.iter().any(|c| c.starts_with("stop_camera")),
        "{calls:?}"
    );
}

#[test]
fn a_destination_streams_another_format_from_the_same_frames() {
    let Some(_turn) = gpu_turn() else { return };
    use crate::outputs::{Destination, DestinationKind};
    let tmp = tempfile::tempdir().unwrap();
    // a class A phone: two formats at once
    let device = DeviceInfo {
        platform: "ios".into(),
        model: "iPhone16,1".into(),
        ..Default::default()
    };
    let projects = Projects::open(tmp.path(), None, tmp.path(), device).unwrap();
    let p = projects.create("Both").unwrap();
    let host = Arc::new(FakeHost::default());
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, host.clone(), heard.clone())
        .unwrap();
    let studio = session.handle;
    session.add_format(false);
    wait_for("the landscape format", || {
        session.state().workspace.canvases.len() == 2
    });
    let st = session.state();
    let wide = st
        .workspace
        .canvases
        .iter()
        .find(|c| c.width > c.height)
        .unwrap()
        .id;
    // the vertical one active (adding a format makes the new one active)
    let tall = st
        .workspace
        .canvases
        .iter()
        .find(|c| c.id != wide)
        .unwrap()
        .id;
    session.set_format(tall);
    wait_for("vertical active", || session.state().canvas == tall);
    let dest = |id: &str, format: Option<String>| Destination {
        id: id.into(),
        name: id.into(),
        kind: DestinationKind::Test { upload_kbps: None },
        video_kbps: 2000,
        audio_kbps: 128,
        height: Some(360),
        fps: None,
        visibility: None,
        network: None,
        delay_s: None,
        give_up_after_s: None,
        format,
    };
    let dests = vec![dest("tall", None), dest("wide", Some(wide.to_string()))];
    let plan = session.stream_plan(&dests, false);
    assert!(plan.allowed, "{plan:?}");
    assert_eq!(plan.encodes.len(), 2);
    assert_eq!(plan.encodes[1].format, wide.to_string());
    session.go_live(dests, false);
    let start = Instant::now();
    while host.opened.lock().len() < 2 {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "{:?} {:?}",
            host.opened.lock(),
            heard
                .0
                .lock()
                .iter()
                .filter(|e| matches!(e, Event::Issue { .. } | Event::Failed { .. }))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let opened = host.opened.lock().clone();
    let (tall_enc, wide_enc) = (&opened[0], &opened[1]);
    assert!(tall_enc.1.width < tall_enc.1.height, "{opened:?}");
    assert!(wide_enc.1.width > wide_enc.1.height, "{opened:?}");
    // both are drawn, each in its own format
    for (video, _) in [tall_enc, wide_enc] {
        let mut drawn = 0;
        let start = Instant::now();
        while drawn < 3 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "encoder {video} drew {drawn}"
            );
            let (r, buf, _, _) = ffi::next_video_frame(studio, *video, 50);
            if r == 1 {
                crate::encode::release_buffer(buf);
                drawn += 1;
            }
        }
    }
    session.end_live();
    wait_for("off", || session.state().live == LiveState::Off);
    session.close();
}

#[test]
fn a_cube_lut_is_checked_before_it_is_taken() {
    let tmp = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| {
        let p = tmp.path().join(name);
        std::fs::write(&p, text).unwrap();
        p
    };
    let mut good = String::from("TITLE \"x\"\nLUT_3D_SIZE 2\n");
    for i in 0..8 {
        good.push_str(&format!("{} {} {}\n", i & 1, (i >> 1) & 1, (i >> 2) & 1));
    }
    assert!(crate::session::check_cube(&write("good.cube", &good)).is_ok());
    let short = good.lines().take(6).collect::<Vec<_>>().join("\n");
    assert!(
        crate::session::check_cube(&write("short.cube", &short))
            .unwrap_err()
            .contains("incomplete")
    );
    assert!(crate::session::check_cube(&write("one.cube", "LUT_1D_SIZE 4\n")).is_err());
    assert!(crate::session::check_cube(&write("none.cube", "0 0 0\n")).is_err());
}

#[test]
fn a_subathon_timer_is_drawn_and_its_buttons_reach_the_widget_server() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Subathon").unwrap();
    let session = projects
        .open_session(
            &p.id,
            Arc::new(FakeHost::default()),
            Arc::new(Heard::default()),
        )
        .unwrap();
    session.add_layer(NewLayer::Widget {
        kind: "subathon".into(),
    });
    let found = || {
        let st = session.state();
        st.workspace.scenes[0].objects.iter().find_map(|o| {
            let s = st.workspace.sources.get(&o.source?)?;
            let i = crate::widgets::instance(&s.settings)?;
            (i.kind == "subathon").then_some((o.id, i.id))
        })
    };
    wait_for("the timer", || found().is_some());
    let (layer, id) = found().unwrap();
    // the layer is drawn, not a placeholder
    let st = session.state();
    let o = st.workspace.scenes[0]
        .objects
        .iter()
        .find(|o| o.id == layer)
        .unwrap();
    assert!(crate::session::unsupported(&st.workspace, o).is_none());
    session.widget_button(layer, "start");
    wait_for("running at the widget server", || {
        crate::web::started().is_some_and(|w| w.storage(&id)["running"] == true)
    });
    session.close();
}

#[test]
fn a_chroma_key_takes_the_green_out_and_keeps_the_rest() {
    let Some(_turn) = gpu_turn() else { return };
    let gpu = crate::gpu::Gpu::new().unwrap();
    // left half green, right half red
    let mut rgba = Vec::new();
    for _y in 0..4 {
        for x in 0..8 {
            rgba.extend(if x < 4 {
                [0, 255, 0, 255]
            } else {
                [255, 0, 0, 255]
            });
        }
    }
    let picture = gpu.texture(&rgba, (8, 4));
    let mut s = lightair_model::Source::new("Cam", "lightair_camera", serde_json::json!({}));
    s.effects.push(lightair_model::Effect::new(
        "Chroma key",
        "chroma_key_filter_v2",
        serde_json::json!({}),
    ));
    let steps = crate::keying::steps(&s);
    let pipeline = crate::keying::Pipeline::new(gpu.device());
    let mut targets = crate::keying::Targets::default();
    let out = crate::keying::draw(
        &gpu,
        &pipeline,
        &mut targets,
        s.id,
        &picture,
        false,
        &steps,
        None,
    );
    let px = lightair_render::compose::read_texture(&out).unwrap();
    let at = |x: usize, y: usize| &px[(y * 8 + x) * 4..(y * 8 + x) * 4 + 4];
    assert_eq!(at(1, 1)[3], 0, "green keyed out: {:?}", at(1, 1));
    assert_eq!(at(6, 1)[3], 255, "red kept: {:?}", at(6, 1));
    assert!(at(6, 1)[0] > 200);
}

#[test]
fn a_cameras_beauty_look_asks_for_faces_and_draws_with_them() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Beauty").unwrap();
    let host = Arc::new(FakeHost::default());
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, host.clone(), heard.clone())
        .unwrap();
    let studio = session.handle;
    wait_for("the camera", || host.camera.lock().is_some());
    let cam = host.camera.lock().unwrap();
    let running = Arc::new(AtomicBool::new(true));
    let faces = Arc::new(AtomicU64::new(0));
    let pusher = {
        let (running, faces) = (running.clone(), faces.clone());
        std::thread::spawn(move || {
            let pool = unsafe {
                crate::gpu::apple::PoolTarget::from_raw(crate::gpu::apple::test_pool(64, 36))
            }
            .unwrap();
            // a face in the middle: MediaPipe's canonical mesh squeezed into the picture
            let points: Vec<f32> = (0..crate::feeds::FACE_POINTS)
                .flat_map(|i| {
                    let a = i as f32 * 0.37;
                    [0.5 + 0.12 * a.cos(), 0.5 + 0.18 * a.sin()]
                })
                .collect();
            let seg = [200u8, 120, 0, 255].repeat(256 * 256);
            while running.load(Ordering::Acquire) {
                let now = crate::clock::host_now_ns();
                unsafe {
                    ffi::push_pixel_buffer(studio, cam, buffer(&pool, [120, 140, 200, 255]), now)
                };
                if ffi::face_wanted(studio, cam) {
                    unsafe {
                        ffi::push_face(studio, cam, now, true, 0.9, points.as_ptr(), 478, 640, 360);
                        ffi::push_segmentation(studio, cam, now, seg.as_ptr(), 256, 256, 1024);
                    }
                    faces.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !ffi::face_wanted(studio, cam),
        "no look: no faces asked for"
    );
    let camera_layer = {
        let st = session.state();
        let scene = st.scene().unwrap();
        scene
            .objects
            .iter()
            .find(|o| {
                o.source
                    .is_some_and(|s| st.workspace.sources[&s].kind == crate::templates::CAMERA)
            })
            .map(|o| o.id)
            .unwrap()
    };
    session.add_effect(
        camera_layer,
        "studio.beauty",
        Some(serde_json::json!({ "smooth": 80 })),
    );
    session.add_effect(
        camera_layer,
        "studio.light",
        Some(serde_json::json!({ "light_preset": "Soft Box" })),
    );
    wait_for("faces asked for and pushed", || {
        faces.load(Ordering::Relaxed) > 10
    });
    let drawn = session.hot().stats.frames.load(Ordering::Relaxed);
    wait_for("frames drawn with the look", || {
        session.hot().stats.frames.load(Ordering::Relaxed) > drawn + 10
    });
    assert!(
        !heard
            .0
            .lock()
            .iter()
            .any(|e| matches!(e, Event::Failed { .. })),
        "nothing failed"
    );
    running.store(false, Ordering::Release);
    pusher.join().unwrap();
    session.close();
}

#[test]
fn a_format_of_any_size_is_made_active_and_its_frames_read_back_whole_with_their_camera_times() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Capture").unwrap();
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    let studio = session.handle;
    wait_for("the camera", || host.camera.lock().is_some());
    let cam = host.camera.lock().unwrap();

    assert_eq!(
        session.add_sized_format(1921, 1080),
        None,
        "odd sizes can't be encoded"
    );
    assert_eq!(session.add_sized_format(0, 1080), None);
    let id = session
        .add_sized_format(1920, 1080)
        .expect("a 1920x1080 format");
    assert_eq!(
        session.add_sized_format(1920, 1080),
        Some(id),
        "a format of that size already there is the one used"
    );
    wait_for("the format active", || session.state().canvas == id);
    let c = session.state().workspace.canvas(id).cloned().unwrap();
    assert_eq!((c.width, c.height), (1920, 1080));

    // red camera frames with their own times, then no more: the last one stays on screen
    let pool =
        unsafe { crate::gpu::apple::PoolTarget::from_raw(crate::gpu::apple::test_pool(64, 36)) }
            .unwrap();
    let mut last = 0;
    for _ in 0..5 {
        last = crate::clock::host_now_ns();
        unsafe { ffi::push_pixel_buffer(studio, cam, buffer(&pool, [0, 0, 255, 255]), last) };
    }
    wait_for("the last camera frame drawn", || {
        session
            .program_frame()
            .is_some_and(|f| f.sources.iter().any(|s| s.feed == cam && s.host_ns == last))
    });
    let f = session.program_frame().unwrap();
    assert_eq!(
        (f.width, f.height),
        (1920, 1080),
        "the format's own size, not scaled"
    );
    assert_eq!(f.rgba.len(), 1920 * 1080 * 4);
    let mid = (540 * 1920 + 960) * 4;
    let px = &f.rgba[mid..mid + 4];
    assert!(
        px[0] > 200 && px[1] < 60 && px[2] < 60,
        "the camera's red fills the middle: {px:?}"
    );
    let png = image::load_from_memory(&f.png().unwrap())
        .unwrap()
        .to_rgba8();
    assert_eq!(png.as_raw(), &f.rgba, "the PNG is the frame, losslessly");
    session.close();
}

#[test]
fn a_feed_thats_gone_raises_no_issue_when_its_host_reports_late() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Late").unwrap();
    let host = Arc::new(FakeHost::default());
    let heard = Arc::new(Heard::default());
    let session = projects
        .open_session(&p.id, host.clone(), heard.clone())
        .unwrap();
    wait_for("the camera", || host.camera.lock().is_some());
    let cam = host.camera.lock().unwrap();
    // a camera switched away from (its feed dropped) says it failed as it stops
    session.feed_state(
        cam + 1000,
        FeedState::Failed {
            reason: "The camera stopped.".into(),
        },
    );
    session.feed_state(
        cam,
        FeedState::Interrupted {
            reason: "a call".into(),
        },
    );
    let issues = || -> Vec<String> {
        heard
            .0
            .lock()
            .iter()
            .filter_map(|e| match e {
                Event::Issue { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    };
    wait_for("the running camera's issue", || {
        issues().iter().any(|i| i == "camera")
    });
    assert_eq!(issues(), ["camera"], "nothing for the feed that's gone");
    session.close();
}

/// The Looks screen's Compare, held while live: the phone's preview shows the camera without its
/// looks, and what's streamed and recorded keeps them all along.
#[test]
fn the_looks_bypass_changes_the_preview_alone_never_the_stream() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Compare").unwrap();
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    let studio = session.handle;
    wait_for("the camera", || host.camera.lock().is_some());
    let cam = host.camera.lock().unwrap();
    // a preview on screen: the bypass draws only where there is one
    let layer: objc2::rc::Retained<objc2::runtime::AnyObject> =
        unsafe { objc2::msg_send![objc2::class!(CAMetalLayer), new] };
    let handle = objc2::rc::Retained::as_ptr(&layer) as u64;
    session.attach_preview(handle, 360, 640, 1.0);
    let running = Arc::new(AtomicBool::new(true));
    let pusher = {
        let running = running.clone();
        std::thread::spawn(move || {
            let pool = unsafe {
                crate::gpu::apple::PoolTarget::from_raw(crate::gpu::apple::test_pool(64, 36))
            }
            .unwrap();
            while running.load(Ordering::Acquire) {
                let now = crate::clock::host_now_ns();
                unsafe {
                    ffi::push_pixel_buffer(studio, cam, buffer(&pool, [120, 140, 200, 255]), now)
                };
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };
    let st = session.state();
    let camera_layer = st
        .scene()
        .unwrap()
        .objects
        .iter()
        .find(|o| {
            o.source
                .is_some_and(|s| st.workspace.sources[&s].kind == crate::templates::CAMERA)
        })
        .map(|o| o.id)
        .unwrap();
    // what's recorded (and streamed: the same encode path), frame by frame
    session.start_recording(false);
    wait_for("the recording", || host.recording.lock().is_some());
    let video = host.recording.lock().clone().unwrap().1.video_encoder;
    let recorded = |frames: usize| {
        let (mut got, mut last, start) = (0, [0u8; 4], Instant::now());
        while got < frames && start.elapsed() < Duration::from_secs(30) {
            let (r, buf, _, _) = ffi::next_video_frame(studio, video, 100);
            if r == 1 {
                last = centre(buf);
                got += 1;
            }
        }
        assert_eq!(got, frames, "frames recorded");
        [last[0], last[1], last[2]]
    };
    let plain = recorded(5);
    session.add_effect(
        camera_layer,
        "studio.grade",
        Some(serde_json::json!({ "filter_preset": "Portra", "filter_amt": 100 })),
    );
    recorded(8);
    let looked = recorded(2);
    let differs = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).any(|(x, y)| x.abs_diff(y) > 3);
    assert!(
        differs(plain, looked),
        "the look is recorded: {plain:?} → {looked:?}"
    );

    session.set_preview_bypass_looks(camera_layer, true);
    recorded(8);
    let during = recorded(2);
    assert!(
        !differs(during, looked),
        "the recording keeps the look while the preview compares: {looked:?} → {during:?}"
    );
    session.set_preview_bypass_looks(camera_layer, false);
    recorded(5);
    assert!(!differs(recorded(1), looked));

    running.store(false, Ordering::Release);
    pusher.join().unwrap();
    session.detach_preview();
    session.close();
}

/// The Looks screen's status chip: warming up until the host's face analysis comes, then the
/// face found with a box around it, no face when it leaves the picture, and stopped once no face
/// look draws the camera.
#[test]
fn the_face_status_follows_what_the_looks_see() {
    use crate::looks::FaceState;
    use std::sync::atomic::AtomicU8;
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Face").unwrap();
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    let studio = session.handle;
    wait_for("the camera", || host.camera.lock().is_some());
    let cam = host.camera.lock().unwrap();
    // 0: no analysis yet, 1: a face in the middle, 2: analysed, no face
    let mode = Arc::new(AtomicU8::new(0));
    let running = Arc::new(AtomicBool::new(true));
    let pusher = {
        let (running, mode) = (running.clone(), mode.clone());
        std::thread::spawn(move || {
            let pool = unsafe {
                crate::gpu::apple::PoolTarget::from_raw(crate::gpu::apple::test_pool(64, 36))
            }
            .unwrap();
            // a face around the middle: x 0.38-0.62, y 0.32-0.68
            let points: Vec<f32> = (0..crate::feeds::FACE_POINTS)
                .flat_map(|i| {
                    let a = i as f32 * 0.37;
                    [0.5 + 0.12 * a.cos(), 0.5 + 0.18 * a.sin()]
                })
                .collect();
            while running.load(Ordering::Acquire) {
                let now = crate::clock::host_now_ns();
                unsafe {
                    ffi::push_pixel_buffer(studio, cam, buffer(&pool, [120, 140, 200, 255]), now)
                };
                let m = mode.load(Ordering::Acquire);
                if m > 0 && ffi::face_wanted(studio, cam) {
                    unsafe {
                        ffi::push_face(
                            studio,
                            cam,
                            now,
                            m == 1,
                            0.9,
                            points.as_ptr(),
                            478,
                            640,
                            360,
                        )
                    };
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };
    let st = session.state();
    let camera_layer = st
        .scene()
        .unwrap()
        .objects
        .iter()
        .find(|o| {
            o.source
                .is_some_and(|s| st.workspace.sources[&s].kind == crate::templates::CAMERA)
        })
        .map(|o| o.id)
        .unwrap();
    assert_eq!(
        session.face_status(camera_layer).state,
        FaceState::Stopped,
        "no look yet"
    );

    session.add_effect(
        camera_layer,
        "studio.beauty",
        Some(serde_json::json!({ "smooth": 80 })),
    );
    wait_for("warming up", || {
        session.face_status(camera_layer).state == FaceState::Warming
    });

    mode.store(1, Ordering::Release);
    wait_for("the face found", || {
        session.face_status(camera_layer).state == FaceState::Found
    });
    let b = session.face_status(camera_layer).face_box.expect("its box");
    let close = |a: f32, b: f32| (a - b).abs() < 0.03;
    assert!(
        close(b[0], 0.38) && close(b[1], 0.32) && close(b[2], 0.24) && close(b[3], 0.36),
        "{b:?}"
    );

    mode.store(2, Ordering::Release);
    wait_for("no face", || {
        session.face_status(camera_layer).state == FaceState::None
    });
    assert_eq!(session.face_status(camera_layer).face_box, None);

    let st = session.state();
    let source = st
        .scene()
        .unwrap()
        .object(camera_layer)
        .unwrap()
        .source
        .unwrap();
    let beauty = st.workspace.sources[&source]
        .effects
        .iter()
        .find(|e| e.kind == "studio.beauty")
        .unwrap()
        .id;
    session.set_effect_enabled(camera_layer, beauty, false);
    wait_for("stopped", || {
        session.face_status(camera_layer).state == FaceState::Stopped
    });

    running.store(false, Ordering::Release);
    pusher.join().unwrap();
    session.close();
}

/// The Looks screen's tiles: a camera layer drawn alone as a square still, with its looks or
/// without them, while the program goes on as it was.
#[test]
fn a_layer_still_shows_it_with_or_without_its_looks_and_leaves_the_program_alone() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Tiles").unwrap();
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    let studio = session.handle;
    wait_for("the camera", || host.camera.lock().is_some());
    let cam = host.camera.lock().unwrap();
    let running = Arc::new(AtomicBool::new(true));
    let pusher = {
        let running = running.clone();
        std::thread::spawn(move || {
            let pool = unsafe {
                crate::gpu::apple::PoolTarget::from_raw(crate::gpu::apple::test_pool(64, 36))
            }
            .unwrap();
            while running.load(Ordering::Acquire) {
                let now = crate::clock::host_now_ns();
                unsafe {
                    ffi::push_pixel_buffer(studio, cam, buffer(&pool, [120, 140, 200, 255]), now)
                };
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };
    let st = session.state();
    let camera_layer = st
        .scene()
        .unwrap()
        .objects
        .iter()
        .find(|o| {
            o.source
                .is_some_and(|s| st.workspace.sources[&s].kind == crate::templates::CAMERA)
        })
        .map(|o| o.id)
        .unwrap();
    session.add_effect(
        camera_layer,
        "studio.grade",
        Some(serde_json::json!({ "filter_preset": "Portra", "filter_amt": 100 })),
    );
    let drawn = session.hot().stats.frames.load(Ordering::Relaxed);
    wait_for("frames with the look", || {
        session.hot().stats.frames.load(Ordering::Relaxed) > drawn + 10
    });
    let middle = |rgba: &[u8], w: u32, h: u32| {
        let at = ((h / 2 * w + w / 2) * 4) as usize;
        [rgba[at], rgba[at + 1], rgba[at + 2]]
    };
    let program = || {
        let f = session.program_frame().expect("the program's frame");
        middle(&f.rgba, f.width, f.height)
    };
    let before = program();

    let with = session
        .capture_layer_still(camera_layer, 64, true, None)
        .expect("a still");
    let without = session
        .capture_layer_still(camera_layer, 64, false, None)
        .expect("a still");
    assert_eq!(
        (with.width, with.height, with.rgba.len()),
        (64, 64, 64 * 64 * 4),
        "a square of the size asked"
    );
    let (a, b) = (middle(&with.rgba, 64, 64), middle(&without.rgba, 64, 64));
    assert!(
        a.iter().zip(b).any(|(x, y)| x.abs_diff(y) > 3),
        "with its look {a:?}, without {b:?}"
    );

    let after = program();
    assert!(
        before.iter().zip(after).all(|(x, y)| x.abs_diff(y) <= 3),
        "the program untouched: {before:?} → {after:?}"
    );
    assert!(
        session
            .capture_layer_still(lightair_model::Id::new(), 64, true, None)
            .is_none(),
        "no such layer"
    );

    running.store(false, Ordering::Release);
    pusher.join().unwrap();
    session.close();
}

/// A host call that never returns (a camera or an encoder stuck in the system) doesn't hang the
/// app when the project closes: its thread is left to end on its own.
#[test]
fn closing_a_project_doesnt_hang_on_a_host_call_that_never_returns() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Stuck").unwrap();
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    wait_for("the camera", || host.camera.lock().is_some());
    host.stuck.store(true, Ordering::Release);
    session.start_recording(false);
    wait_for("the stuck call", || !host.recordings.lock().is_empty());
    let (done, closed) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        session.close();
        let _ = done.send(());
    });
    // 3 s at most (1 s of silence from the stuck thread, then the rest of the 3 s for the
    // threads), with room for a debug build beside other tests
    let in_time = closed.recv_timeout(Duration::from_secs(5)).is_ok();
    host.stuck.store(false, Ordering::Release);
    assert!(
        in_time,
        "close took more than 5 s while a host call was stuck"
    );
}

/// A Compare held as the app goes to the background never sees its release: it's let go then,
/// so the preview comes back with the looks on.
#[test]
fn a_compare_held_as_the_app_leaves_is_let_go() {
    let Some(_turn) = gpu_turn() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let projects = Projects::open(tmp.path(), None, tmp.path(), DeviceInfo::default()).unwrap();
    let p = projects.create("Away").unwrap();
    let host = Arc::new(FakeHost::default());
    let session = projects
        .open_session(&p.id, host.clone(), Arc::new(Heard::default()))
        .unwrap();
    let layer = session.state().scene().unwrap().objects[0].id;
    session.set_preview_bypass_looks(layer, true);
    session.lifecycle(true);
    assert_eq!(
        *session.hot().preview_bypass.lock(),
        Some(layer),
        "held while in front"
    );
    session.lifecycle(false);
    session.lifecycle(true);
    assert_eq!(
        *session.hot().preview_bypass.lock(),
        None,
        "let go when the app left"
    );
    session.close();
}
