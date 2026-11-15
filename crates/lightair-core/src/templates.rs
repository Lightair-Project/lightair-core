//! Mixer templates (docs/plans/audio-engine.md, Use cases): a mixer set up for streaming, a
//! podcast, music or gaming, as one undoable change that replaces the tracks, buses, mixes and
//! master effects (scenes and their sources stay).

use serde_json::{Value, json};

use crate::catalog;
use crate::command::Command;
use crate::model::{
    AudioMix, Bus, Effect, Id, Source, Master, Workspace, Route, Send, StemFormat, StemPoint, Track, TrackChannels,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    Streaming,
    Podcast,
    Music,
    Gaming,
}

impl Template {
    pub const ALL: [Template; 4] = [Template::Streaming, Template::Podcast, Template::Music, Template::Gaming];

    pub fn name(self) -> &'static str {
        match self {
            Template::Streaming => "Streaming",
            Template::Podcast => "Podcast",
            Template::Music => "Music",
            Template::Gaming => "Gaming",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Template::Streaming => "Mic, desktop audio, music that ducks when you talk, a VOD mix without it",
            Template::Podcast => "A track per mic, a Guests bus, a remote guest and their mix-minus",
            Template::Music => "Vocal and instrument you hear live, a reverb bus, a low-latency buffer",
            Template::Gaming => "Mic, game and chat apps as tracks, and a mix without the chat",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Template::Streaming => "radio",
            Template::Podcast => "mic",
            Template::Music => "music",
            Template::Gaming => "gamepad-2",
        }
    }

    /// The engine block size it wants (frames), when it cares.
    pub fn block(self) -> Option<usize> {
        match self {
            Template::Music => Some(128),
            _ => None,
        }
    }
}

/// Where microphones come from.
#[derive(Debug, Clone)]
pub struct Mics {
    /// The source kind of an audio interface (the engine's device source).
    pub kind: String,
    /// The interface ("" for the system default source).
    pub device: String,
    /// Its channels (an 8-channel interface gives 8 mics).
    pub channels: u16,
}

/// Source kinds that exist only for the mixer (no picture): replaced with the mixer.
const AUDIO_KINDS: &[&str] =
    &["lightair_audio_device", crate::kinds::MIC, crate::kinds::DESKTOP_AUDIO, crate::kinds::APP_AUDIO];

fn effect(kind: &str, set: &[(&str, Value)]) -> Effect {
    let mut e = catalog::def(kind).map(|d| d.create()).unwrap_or_else(|| Effect::new(kind, kind, json!({})));
    for (k, v) in set {
        e.settings[*k] = v.clone();
    }
    e
}

fn voice() -> Effect {
    effect("audio.voice", &[])
}

fn limiter(ceiling: f64) -> Effect {
    effect("audio.limiter", &[("ceiling_db", json!(ceiling))])
}

/// What the template adds, gathered.
struct Build {
    sources: Vec<Source>,
    tracks: Vec<Track>,
    buses: Vec<Bus>,
    mixes: Vec<AudioMix>,
    master: Vec<Effect>,
}

impl Build {
    fn new() -> Self {
        Self { sources: vec![], tracks: vec![], buses: vec![], mixes: vec![], master: vec![] }
    }

    fn source(&mut self, name: &str, kind: &str, settings: Value) -> Id {
        let i = Source::new(name, kind, settings);
        let id = i.id;
        self.sources.push(i);
        id
    }

    /// The computer's sound: the whole desktop, or one app (chosen in the mixer).
    fn app_audio(&mut self, name: &str, desktop: bool) -> Id {
        let kind = if desktop { crate::kinds::DESKTOP_AUDIO } else { crate::kinds::APP_AUDIO };
        self.source(name, kind, json!({}))
    }

    fn track(&mut self, name: &str, source: Id, effects: Vec<Effect>) -> &mut Track {
        let mut t = Track::new(name, source);
        t.effects = effects;
        self.tracks.push(t);
        self.tracks.last_mut().unwrap()
    }
}

/// The microphones: an interface's channels as mono tracks (up to `most`), or one track on the
/// default source. Their ids.
fn mics(b: &mut Build, m: &Mics, names: &[&str], effects: impl Fn() -> Vec<Effect>) -> Vec<Id> {
    let n = if m.channels >= 2 { names.len().min(m.channels as usize) } else { 1 };
    let name = if m.device.is_empty() { "Microphone".to_string() } else { m.device.clone() };
    let source = b.source(&name, &m.kind, json!({ "device": m.device }));
    (0..n)
        .map(|c| {
            let t = b.track(names[c], source, effects());
            if n > 1 {
                t.channels = TrackChannels::Mono(c as u16);
            }
            t.id
        })
        .collect()
}

/// The commands that set the mixer up as `template`, replacing what's there.
pub fn apply(workspace: &Workspace, template: Template, m: &Mics) -> Command {
    let mut b = Build::new();
    let mut recording = workspace.outputs.recording.clone();
    recording.file_mixes.clear();
    match template {
        Template::Streaming => {
            let mic = mics(&mut b, m, &["Mic"], || vec![voice()])[0];
            let desktop = b.app_audio("Desktop audio", true);
            let game = b.track("Desktop audio", desktop, vec![]).id;
            let music_in = b.app_audio("Music", false);
            let ducker = effect("audio.ducker", &[("source", json!(mic.to_string()))]);
            b.track("Music", music_in, vec![ducker]).volume_db = -12.0;
            // a VOD mix: everything but the music (Twitch mutes copyrighted music in VODs)
            let vod = AudioMix::new("VOD (no music)");
            for t in &mut b.tracks {
                if t.id == mic || t.id == game {
                    t.sends.push(Send { mix: vod.id, level_db: 0.0, pre_fader: false });
                }
            }
            b.mixes.push(vod);
            b.master.push(limiter(-1.0));
        }
        Template::Podcast => {
            let ids = mics(&mut b, m, &["Host", "Guest 1", "Guest 2", "Guest 3"], || vec![voice()]);
            let remote_in = b.app_audio("Remote guest", false);
            let remote = b.track("Remote guest", remote_in, vec![voice()]).id;
            // every voice to its own file, for editing
            for t in &mut b.tracks {
                t.record = true;
            }
            // guests share a bus (one fader for the room)
            if ids.len() > 1 {
                let guests = Bus::new("Guests");
                for t in &mut b.tracks {
                    if ids[1..].contains(&t.id) {
                        t.route = Route::Bus(guests.id);
                    }
                }
                b.buses.push(guests);
            }
            // a mix-minus: everyone but the remote guest, to send back to them
            let minus = AudioMix::new("Mix-minus");
            for t in &mut b.tracks {
                if t.id != remote {
                    t.sends.push(Send { mix: minus.id, level_db: 0.0, pre_fader: false });
                }
            }
            b.mixes.push(minus);
            b.master.push(effect(
                "audio.compressor",
                &[("threshold_db", json!(-20.0)), ("ratio", json!(2.0)), ("attack_ms", json!(20.0))],
            ));
            b.master.push(limiter(-1.0));
            recording.multitrack = true;
            recording.stem_point = StemPoint::Raw;
        }
        Template::Music => {
            // no voice processing: it colours singing and instruments
            let vocal_fx = || {
                vec![
                    effect("audio.eq", &[("b1_on", json!(true))]),
                    effect("audio.compressor", &[("ratio", json!(2.5)), ("threshold_db", json!(-20.0))]),
                ]
            };
            let ids = mics(&mut b, m, &["Vocal", "Instrument"], vocal_fx);
            let reverb = Bus::new("Reverb");
            let mut bus = reverb;
            bus.effects.push(effect("audio.reverb", &[("mix", json!(1.0)), ("size", json!(0.6))]));
            for (k, t) in b.tracks.iter_mut().enumerate() {
                if ids.contains(&t.id) {
                    t.cue = true;
                    t.record = true;
                    // the instrument plays clean
                    if k == 1 {
                        t.effects.clear();
                    }
                    t.sends.push(Send { mix: bus.id, level_db: if k == 0 { -12.0 } else { -18.0 }, pre_fader: false });
                }
            }
            b.buses.push(bus);
            b.master.push(limiter(-1.0));
            recording.multitrack = true;
            recording.stem_format = StemFormat::Pcm24;
            recording.stem_point = StemPoint::Raw;
        }
        Template::Gaming => {
            let mic = mics(&mut b, m, &["Mic"], || vec![voice()])[0];
            let game_in = b.app_audio("Game", false);
            let game = b.track("Game", game_in, vec![]).id;
            let chat_in = b.app_audio("Chat", false);
            // heard in headphones too
            b.track("Chat", chat_in, vec![]).cue = true;
            let clean = AudioMix::new("No chat");
            for t in &mut b.tracks {
                if t.id == mic || t.id == game {
                    t.sends.push(Send { mix: clean.id, level_db: 0.0, pre_fader: false });
                }
            }
            b.mixes.push(clean);
            b.master.push(limiter(-1.0));
        }
    }

    // out with the old mixer: tracks, then their audio-only sources nothing else shows
    let mut commands = Vec::new();
    for t in &workspace.tracks {
        commands.push(Command::RemoveTrack { track: t.id });
    }
    let shown = |id: Id| workspace.scenes.iter().any(|s| s.layers.iter().any(|l| l.source == Some(id)));
    let mut seen = std::collections::HashSet::new();
    for t in &workspace.tracks {
        if seen.insert(t.source)
            && let Some(i) = workspace.sources.get(&t.source)
            && AUDIO_KINDS.contains(&i.kind.as_str())
            && !shown(i.id)
        {
            commands.push(Command::RemoveSource { source: i.id });
        }
    }
    for m in &workspace.audio.mixes {
        commands.push(Command::RemoveMix { mix: m.id });
    }
    for bus in &workspace.audio.buses {
        commands.push(Command::RemoveBus { bus: bus.id });
    }
    // in with the new
    commands.extend(b.sources.into_iter().map(|source| Command::AddSource { source }));
    commands.extend(b.buses.into_iter().map(|bus| Command::AddBus { bus, index: None }));
    commands.extend(b.mixes.into_iter().map(|mix| Command::AddMix { mix }));
    commands.extend(b.tracks.into_iter().map(|track| Command::AddTrack { track, index: None }));
    commands.push(Command::UpdateMaster { master: Master { effects: b.master, ..workspace.audio.master.clone() } });
    commands.push(Command::SetRecordingSettings { settings: recording });
    Command::Batch { label: format!("{} mixer", template.name()), commands }
}
