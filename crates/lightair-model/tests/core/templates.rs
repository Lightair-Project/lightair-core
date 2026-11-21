//! Mixer templates: each replaces the mixer in one undoable step, with valid routing.

use lightair_model::templates::{Mics, Template, apply};
use lightair_model::{AudioMix, Bus, Command, Document, Route, Source, Track, TrackChannels, Workspace};
use serde_json::json;

fn interface(channels: u16) -> Mics {
    let device: String = if channels > 2 { "Interface".into() } else { String::new() };
    Mics { kind: "lightair_audio_device".into(), name: device.clone(), device, channels }
}

fn names(doc: &Document) -> Vec<String> {
    doc.workspace().tracks.iter().map(|t| t.name.clone()).collect()
}

#[test]
fn every_template_replaces_the_mixer_and_undoes() {
    let mut doc = Document::new(Workspace::new("Show"));
    let mic = Source::new("Old mic", "lightair_audio_device", json!({}));
    let mic_id = mic.id;
    doc.apply(&Command::AddSource { source: mic }).unwrap();
    doc.apply(&Command::AddTrack { track: Track::new("Old", mic_id), index: None }).unwrap();
    doc.apply(&Command::AddBus { bus: Bus::new("Old bus"), index: None }).unwrap();
    doc.apply(&Command::AddMix { mix: AudioMix::new("Old mix") }).unwrap();
    let before: Workspace = (**doc.workspace()).clone();

    for t in Template::ALL {
        let cmd = apply(doc.workspace(), t, &interface(8));
        doc.apply(&cmd).unwrap_or_else(|e| panic!("{}: {e:?}", t.name()));
        let p = doc.workspace();
        assert!(!p.tracks.is_empty(), "{}", t.name());
        assert!(!p.tracks.iter().any(|t| t.name == "Old"), "replaced");
        assert!(!p.audio.buses.iter().any(|b| b.name == "Old bus"));
        assert!(!p.audio.mixes.iter().any(|m| m.name == "Old mix"));
        assert!(!p.sources.contains_key(&mic_id), "the old mic input went with its track");
        assert!(p.audio.master.effects.iter().any(|e| e.kind == "audio.limiter"), "{}: a limiter on Program", t.name());
        // only the template's own sources remain for the mixer: no orphans pile up
        let used: std::collections::HashSet<_> = p.tracks.iter().map(|t| t.source).collect();
        assert!(p.sources.keys().all(|i| used.contains(i)), "{}: orphan inputs", t.name());
    }

    // the last one applied was Gaming; check each in turn on a fresh copy
    let fresh = |t: Template, m: Mics| {
        let mut d = Document::new(before.clone());
        d.apply(&apply(d.workspace(), t, &m)).unwrap();
        d
    };
    let s = fresh(Template::Streaming, interface(1));
    assert_eq!(names(&s), ["Mic", "Desktop audio", "Music"]);
    let music = s.workspace().tracks.iter().find(|t| t.name == "Music").unwrap();
    let mic = s.workspace().tracks.iter().find(|t| t.name == "Mic").unwrap();
    assert_eq!(music.effects[0].kind, "audio.ducker");
    assert_eq!(music.effects[0].settings["source"], json!(mic.id.to_string()), "ducks under the mic");
    let vod = &s.workspace().audio.mixes[0];
    assert!(music.sends.is_empty() && mic.sends.iter().any(|x| x.mix == vod.id), "VOD has no music");

    let p = fresh(Template::Podcast, interface(4));
    assert_eq!(names(&p), ["Host", "Guest 1", "Guest 2", "Guest 3", "Remote guest"]);
    let proj = p.workspace();
    assert!(proj.tracks.iter().all(|t| t.record), "every voice to its own file");
    assert!(proj.outputs.recording.multitrack);
    assert_eq!(proj.tracks[2].channels, TrackChannels::Mono(2), "interface channels");
    let guests = &proj.audio.buses[0];
    assert!(proj.tracks[1..4].iter().all(|t| t.route == Route::Bus(guests.id)));
    let minus = &proj.audio.mixes[0];
    let remote = proj.tracks.iter().find(|t| t.name == "Remote guest").unwrap();
    assert!(!remote.sends.iter().any(|x| x.mix == minus.id), "the mix-minus leaves the remote guest out");

    let m = fresh(Template::Music, interface(2));
    let proj = m.workspace();
    assert_eq!(names(&m), ["Vocal", "Instrument"]);
    assert!(proj.tracks.iter().all(|t| t.cue && t.record));
    assert!(proj.tracks[1].effects.is_empty(), "the instrument plays clean");
    assert_eq!(proj.audio.buses[0].effects[0].kind, "audio.reverb");

    // one step back restores the old mixer
    let mut d = Document::new(before.clone());
    let cmd = apply(d.workspace(), Template::Podcast, &interface(1));
    d.apply(&cmd).unwrap();
    assert!(d.undo().is_some());
    assert_eq!(names(&d), ["Old"]);
    assert_eq!(d.workspace().audio.buses.len(), 1);
}
