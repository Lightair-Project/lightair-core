//! The mixer's model: buses, mixes, routing rules, removal, older projects.

use lightair_core::{
    AudioMix, Bus, Command, CommandError, Document, Effect, Input, Project, Route, Send, Track, TrackChannels,
};
use serde_json::json;

fn project() -> (Document, lightair_core::Id) {
    let mut doc = Document::new(Project::new("Pod"));
    let mic = Input::new("Interface", "coreaudio_input_capture", json!({}));
    let id = mic.id;
    doc.apply(&Command::AddInput { input: mic }).unwrap();
    let t = Track::new("Host", id);
    let tid = t.id;
    doc.apply(&Command::AddTrack { track: t, index: None }).unwrap();
    (doc, tid)
}

fn track(doc: &Document, id: lightair_core::Id) -> Track {
    (**doc.project().track(id).unwrap()).clone()
}

#[test]
fn routing_to_buses_and_loops() {
    let (mut doc, tid) = project();
    let (a, b) = (Bus::new("Guests"), Bus::new("Voices"));
    doc.apply(&Command::AddBus { bus: a.clone(), index: None }).unwrap();
    doc.apply(&Command::AddBus { bus: b.clone(), index: None }).unwrap();
    let mut t = track(&doc, tid);
    t.route = Route::Bus(a.id);
    t.channels = TrackChannels::Mono(3);
    doc.apply(&Command::UpdateTrack { track: t.clone() }).unwrap();
    assert_eq!(track(&doc, tid).channels, TrackChannels::Mono(3));
    // Guests → Voices is fine; Voices → Guests then loops
    doc.apply(&Command::UpdateBus { bus: Bus { route: Route::Bus(b.id), ..a.clone() } }).unwrap();
    let looped = doc.apply(&Command::UpdateBus { bus: Bus { route: Route::Bus(a.id), ..b.clone() } });
    assert!(matches!(looped, Err(CommandError::AudioLoop)), "{looped:?}");
    let missing = doc.apply(&Command::UpdateTrack { track: Track { route: Route::Bus(lightair_core::Id::new()), ..t } });
    assert!(matches!(missing, Err(CommandError::NoBus(_))));
    // removing Guests sends the track back to Program
    doc.apply(&Command::RemoveBus { bus: a.id }).unwrap();
    assert_eq!(track(&doc, tid).route, Route::Program);
}

#[test]
fn mixes_and_their_sends() {
    let (mut doc, tid) = project();
    let phones = AudioMix::new("Headphones");
    let bad = doc.apply(&Command::UpdateTrack {
        track: Track { sends: vec![Send { mix: phones.id, level_db: 0.0, pre_fader: true }], ..track(&doc, tid) },
    });
    assert!(matches!(bad, Err(CommandError::NoMix(_))), "a send needs its mix");
    doc.apply(&Command::AddMix { mix: phones.clone() }).unwrap();
    doc.apply(&Command::UpdateTrack {
        track: Track { sends: vec![Send { mix: phones.id, level_db: -3.0, pre_fader: true }], ..track(&doc, tid) },
    })
    .unwrap();
    assert_eq!(track(&doc, tid).sends.len(), 1);
    doc.apply(&Command::RemoveMix { mix: phones.id }).unwrap();
    assert!(track(&doc, tid).sends.is_empty(), "its sends go with it");
    assert!(doc.undo().is_some());
    assert_eq!(track(&doc, tid).sends.len(), 1, "undo brings both back");
}

#[test]
fn older_projects_load_and_their_filters_become_engine_effects() {
    // a track saved before routing existed
    let old = json!({"id": lightair_core::Id::new(), "name": "Mic", "input": lightair_core::Id::new(),
        "volume_db": -3.0, "muted": false, "cue": false,
        "effects": [{"id": lightair_core::Id::new(), "name": "Gate", "kind": "noise_gate_filter", "enabled": true,
            "settings": {"open_threshold": -30.0, "close_threshold": -36.0}}]});
    let t: Track = serde_json::from_value(old).unwrap();
    assert_eq!((t.route, t.channels, t.pan, t.record), (Route::Program, TrackChannels::All, 0.0, false));
    let gate = lightair_core::catalog::migrate_audio_effect(&t.effects[0]).unwrap();
    assert_eq!(gate.kind, "audio.gate");
    assert_eq!(gate.settings["threshold_db"], -30.0);
    assert_eq!(gate.settings["hysteresis_db"], 6.0);
    assert_eq!(gate.id, t.effects[0].id, "the same effect, converted");
    assert!(lightair_core::catalog::migrate_audio_effect(&Effect::new("V", "audio.voice", json!({}))).is_none());
}

#[test]
fn effects_on_buses_mixes_and_the_master() {
    use lightair_core::{MASTER_ID, MixerStrip};
    let (mut doc, tid) = project();
    let bus = Bus::new("Music");
    let mix = AudioMix::new("VOD");
    doc.apply(&Command::AddBus { bus: bus.clone(), index: None }).unwrap();
    doc.apply(&Command::AddMix { mix: mix.clone() }).unwrap();
    for strip in [MixerStrip::Bus(bus.id), MixerStrip::Mix(mix.id), MixerStrip::Master, MixerStrip::Track(tid)] {
        let e = Effect::new("Limiter", "audio.limiter", json!({}));
        let eid = e.id;
        doc.apply(&Command::AddStripEffect { strip, effect: e, index: None }).unwrap();
        doc.apply(&Command::SetStripEffectSettings { strip, effect: eid, settings: json!({"ceiling_db": -2.0}) }).unwrap();
        doc.apply(&Command::SetStripEffectEnabled { strip, effect: eid, enabled: false }).unwrap();
    }
    let p = doc.project();
    assert_eq!(p.audio.bus(bus.id).unwrap().effects[0].settings["ceiling_db"], -2.0);
    assert!(!p.audio.mix(mix.id).unwrap().effects[0].enabled);
    assert_eq!(p.audio.master.effects.len(), 1);
    assert_eq!(p.track(tid).unwrap().effects.len(), 1);
    assert_eq!(MixerStrip::find(p, MASTER_ID), Some(MixerStrip::Master));
    assert_eq!(MixerStrip::find(p, bus.id), Some(MixerStrip::Bus(bus.id)));
    let first = p.audio.master.effects[0].id;
    doc.apply(&Command::RemoveStripEffect { strip: MixerStrip::Master, effect: first }).unwrap();
    assert!(doc.project().audio.master.effects.is_empty());
}
