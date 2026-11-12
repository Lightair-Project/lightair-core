//! The audio thread never allocates: processing, picking up a new graph, meters and loudness.
//! A counting allocator watches the renderer's thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use lightair_audio::dsp::{GainParams, Params};
use lightair_audio::{Channels, Dest, EffectSpec, Engine, GraphSpec, InputBlock, Mix, Send, Strip};

struct Counting;

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if WATCHING.with(|w| w.get()) {
            COUNT.with(|c| c.set(c.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if WATCHING.with(|w| w.get()) {
            COUNT.with(|c| c.set(c.get() + 1));
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn spec(gain_db: f32) -> GraphSpec {
    let mut mic = Strip::track(1, 100, Channels::Mono(0));
    mic.inserts = vec![EffectSpec { id: 7, enabled: true, params: Params::Gain(GainParams { db: gain_db, ..Default::default() }) }];
    mic.dest = Dest::Bus(10);
    mic.sends = vec![Send { mix: 50, level_db: -3.0, pre_fader: true }];
    let music = Strip::track(2, 101, Channels::Stereo(0, 1));
    GraphSpec { strips: vec![mic, music, Strip::bus(10)], mixes: vec![Mix::new(50)], ..Default::default() }
}

#[test]
fn the_audio_thread_never_allocates() {
    let (mut engine, mut r) = Engine::new(spec(0.0), 48_000);
    let (a, b) = (vec![0.3f32; 512], vec![-0.2f32; 512]);
    let mic: [&[f32]; 1] = [&a];
    let music: [&[f32]; 2] = [&a, &b];
    let inputs = [InputBlock { id: 100, channels: &mic }, InputBlock { id: 101, channels: &music }];
    // warm up (first blocks may grow nothing, but be fair)
    r.process(&inputs, 512);
    for round in 0..20 {
        engine.set(spec(round as f32)); // a new graph waiting (made on this, the "main", side)
        WATCHING.with(|w| w.set(true));
        for _ in 0..50 {
            r.process(&inputs, 512);
        }
        WATCHING.with(|w| w.set(false));
    }
    let n = COUNT.with(|c| c.get());
    assert_eq!(n, 0, "the renderer allocated or freed {n} times");
}
