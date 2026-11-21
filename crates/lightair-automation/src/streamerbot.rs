//! Streamer.bot import: an export string (`base64("SBAE" + gzip(JSON))`) → Lightair
//! automations, one per action, as close as Lightair has: chat messages, waits, variables,
//! sounds, speech, scenes, showing and hiding sources, running other actions, web requests,
//! If/Else, random groups. What has no counterpart (C# code, OBS filters, …) is listed, and C#
//! is kept in a JavaScript step as a comment to port by hand.
//!
//! Type codes from Streamer.bot's own enums (as the streamerz project extracted them), checked
//! against real exports; both the 1.0 shape (`data.actions`, nested `subActions`) and the older
//! one (top-level `actions`, flat `actions` with `actionGroups`).

use std::collections::HashMap;
use std::io::Read;

use base64::Engine;
use serde_json::{Value, json};

use crate::model::{Automation, Condition, Step, Trigger, Who};

/// What an import made, and what it couldn't bring over (one line each).
pub struct Imported {
    pub automations: Vec<Automation>,
    pub notes: Vec<String>,
}

/// The JSON inside an export string (found wherever it is in what was pasted: a README, a
/// message around it).
pub fn decode(text: &str) -> Result<Value, String> {
    let from = text.find("U0JBRR").unwrap_or(0);
    // the pieces after it that could be base64 (a wrapped string is several lines)
    let mut pieces: Vec<&str> = Vec::new();
    for word in text[from..].split_whitespace() {
        let end =
            word.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))).unwrap_or(word.len());
        if end > 0 {
            pieces.push(&word[..end]);
        }
        // the string ends where something else is stuck to it (a quote, a code fence)
        if end < word.len() {
            break;
        }
    }
    // the longest that unpacks (words after the string look like base64 too); what the whole
    // run said, if none does
    let mut first = None;
    for n in (1..=pieces.len()).rev() {
        match unpack(&pieces[..n].concat()) {
            Ok(v) => return Ok(v),
            Err(e) => {
                first.get_or_insert(e);
            }
        }
    }
    Err(first.unwrap_or_else(|| "that isn't a Streamer.bot export".into()))
}

fn unpack(compact: &str) -> Result<Value, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(compact)
        .map_err(|_| "that isn't a Streamer.bot export (not base64)".to_string())?;
    let gz = bytes.strip_prefix(b"SBAE").ok_or("that isn't a Streamer.bot export (no SBAE header)")?;
    let mut json = String::new();
    flate2::read::GzDecoder::new(gz).read_to_string(&mut json).map_err(|e| format!("couldn't unpack it: {e}"))?;
    serde_json::from_str(&json).map_err(|e| format!("couldn't read it: {e}"))
}

/// Streamer.bot's %variables% → Lightair's {{ templates }}.
pub fn template(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else { break };
        let name = &after[..end];
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            out.push_str(&rest[..start + 1]);
            rest = after;
            continue;
        }
        out.push_str(&rest[..start]);
        out.push_str(&format!("{{{{ {} }}}}", variable(name)));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// A Streamer.bot argument's name → a path in Lightair's context.
fn variable(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if let Some(n) = lower.strip_prefix("input").and_then(|n| n.parse::<usize>().ok()) {
        return format!("args.{n}");
    }
    match lower.as_str() {
        "user" | "username" | "display" | "displayname" => "name".into(),
        "userid" => "user.id".into(),
        "rawinput" | "rawinputescaped" => "arg".into(),
        "targetuser" | "targetusername" | "touser" => "target".into(),
        "message" | "rawmessage" => "message".into(),
        "bits" | "amount" | "viewers" | "viewercount" | "monthssubscribed" | "cumulative" => "amount".into(),
        "broadcastuser" | "broadcastusername" | "broadcaster" => "channel".into(),
        "game" => "game".into(),
        _ => format!("vars.{name}"),
    }
}

fn s(v: &Value, k: &str) -> String {
    match &v[k] {
        Value::String(t) => t.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Children in Streamer.bot's order.
fn ordered(list: &Value) -> Vec<&Value> {
    let mut v: Vec<&Value> =
        list.as_array().into_iter().flatten().filter(|x| x["enabled"].as_bool() != Some(false)).collect();
    v.sort_by_key(|x| x["index"].as_i64().unwrap_or(0));
    v
}

struct Ctx<'a> {
    /// action id → name (Run Action)
    names: &'a HashMap<String, String>,
    notes: Vec<String>,
    action: String,
}

impl Ctx<'_> {
    fn note(&mut self, what: String) {
        self.notes.push(format!("{}: {what}", self.action));
    }
}

fn steps(list: &Value, cx: &mut Ctx) -> Vec<Step> {
    ordered(list).into_iter().flat_map(|sa| step(sa, cx)).collect()
}

fn step(sa: &Value, cx: &mut Ctx) -> Vec<Step> {
    let kind = sa["type"].as_i64().unwrap_or(-1);
    let one = |s: Step| vec![s];
    match kind {
        // send a chat message: Twitch, reply, announcement, YouTube, Kick, Kick reply
        10 | 560 | 23 | 4001 | 35001 | 35002 => {
            let platform = match kind {
                4001 => "youtube",
                35001 | 35002 => "kick",
                _ => "twitch",
            };
            one(Step::Chat { text: template(&s(sa, "text")), platform: platform.into() })
        }
        // delay (milliseconds, maybe text)
        1002 => {
            let ms = s(sa, "value").trim().parse::<f64>().unwrap_or_else(|_| {
                cx.note(format!("a delay of “{}” became 1 second", s(sa, "value")));
                1000.0
            });
            one(Step::Wait { seconds: ms / 1000.0 })
        }
        // set a global (or the user's) variable: 0 value, 1 add, 2 subtract, 3 remove
        122 => {
            let name = s(sa, "variableName");
            let user = sa["destination"].as_i64() == Some(1);
            let value = template(&s(sa, "value"));
            let (op, value) = match sa["source"].as_i64() {
                Some(1) => ("add", if value.is_empty() { "1".into() } else { value }),
                Some(2) => ("add", format!("-{}", if value.is_empty() { "1" } else { value.as_str() })),
                Some(3) => ("clear", String::new()),
                _ => ("set", value),
            };
            one(Step::Var { name, op: op.into(), value, user })
        }
        // set an argument for the rest of the run: a variable here (%name% reads vars.name)
        123 => one(Step::Var {
            name: s(sa, "variableName"),
            op: "set".into(),
            value: template(&s(sa, "value")),
            user: false,
        }),
        // a global copied into an argument
        121 => {
            let (from, to) = (s(sa, "variableName"), s(sa, "destinationVariable"));
            let to = if to.is_empty() { from.clone() } else { to };
            if to == from {
                vec![]
            } else {
                one(Step::Var { name: to, op: "set".into(), value: format!("{{{{ vars.{from} }}}}"), user: false })
            }
        }
        // OBS: a text source's text, a browser source's address
        39 => one(Step::Call {
            method: "source.update".into(),
            params: json!({"source": s(sa, "sourceName"), "settings": {"text": template(&s(sa, "text"))}}),
        }),
        38 => one(Step::Call {
            method: "source.update".into(),
            params: json!({"source": s(sa, "sourceName"), "settings": {"url": template(&s(sa, "url"))}}),
        }),
        // play a sound (volume 1.0 = 100%)
        1 => one(Step::Sound {
            file: s(sa, "soundFile"),
            volume: sa["volume"].as_f64().map_or(100.0, |v| (v * 100.0).clamp(0.0, 100.0)),
        }),
        // Speaker.bot speech
        601 | 602 => one(Step::Speak { text: template(&s(sa, "message")) }),
        // OBS: set scene
        34 => one(Step::Call { method: "scene.switch".into(), params: json!({"scene": s(sa, "sceneName")}) }),
        // OBS: source visibility (0 visible, 1 hidden, 2 toggle)
        30 => {
            let visible = match sa["state"].as_i64() {
                Some(1) => false,
                Some(2) => {
                    cx.note(format!("“toggle {}” became “show”", s(sa, "sourceName")));
                    true
                }
                _ => true,
            };
            one(Step::Call {
                method: "object.update".into(),
                params: json!({"object": s(sa, "sourceName"), "scene": s(sa, "sceneName"), "visible": visible}),
            })
        }
        // run another action
        4 => {
            let id = s(sa, "actionId");
            one(Step::Run { automation: cx.names.get(&id).cloned().unwrap_or(id) })
        }
        // fetch a URL
        1007 => one(Step::Http { url: template(&s(sa, "url")), method: "GET".into(), body: String::new() }),
        // C# code: kept to port by hand
        99999 | 99998 => {
            let source = sa["byteCode"]
                .as_str()
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            cx.note("C# code can't run in Lightair: kept as a comment in a JavaScript step to port".into());
            one(Step::Script {
                code: format!(
                    "// From Streamer.bot (C#), to port by hand:\n/*\n{}\n*/\nreturn {{}};",
                    source.replace("*/", "* /")
                ),
            })
        }
        // if / else
        120 => {
            let children = ordered(&sa["subActions"]);
            let then = children
                .iter()
                .find(|c| c["type"].as_i64() == Some(99901))
                .map(|c| steps(&c["subActions"], cx))
                .unwrap_or_default();
            let otherwise = children
                .iter()
                .find(|c| c["type"].as_i64() == Some(99902))
                .map(|c| steps(&c["subActions"], cx))
                .unwrap_or_default();
            let left = format!("{{{{ {} }}}}", variable(s(sa, "input").trim_matches('%')));
            let right = template(&s(sa, "value"));
            let (op, right) = match sa["operation"].as_i64() {
                Some(1) | Some(8) => ("is not", right),
                Some(2) => ("contains", right),
                Some(4) => ("<", right),
                Some(5) => (">", right),
                Some(6) | Some(9) => ("is", String::new()),
                Some(3) => {
                    cx.note("a regex condition became “contains”".into());
                    ("contains", right)
                }
                _ => ("is", right),
            };
            one(Step::If { when: vec![Condition { left, op: op.into(), right }], then, otherwise })
        }
        // a group: random picks one child, otherwise its children in order
        99900 => {
            if sa["random"].as_bool() == Some(true) {
                let choices: Vec<Vec<Step>> =
                    ordered(&sa["subActions"]).into_iter().map(|c| step(c, cx)).filter(|c| !c.is_empty()).collect();
                if choices.is_empty() { vec![] } else { one(Step::Random { choices }) }
            } else {
                steps(&sa["subActions"], cx)
            }
        }
        // break
        124 => one(Step::Stop),
        // comments and log lines
        1009 | 1020 => vec![],
        other => {
            cx.note(format!("a step of kind {other} has no counterpart in Lightair (skipped)"));
            vec![]
        }
    }
}

/// An older export: the action's flat `actions` with `actionGroups` (random groups become one
/// Random step where the group's first step was).
fn legacy_steps(action: &Value, cx: &mut Ctx) -> Vec<Step> {
    let random: HashMap<String, bool> = action["actionGroups"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|g| (s(g, "name"), g["random"].as_bool() == Some(true)))
        .collect();
    let mut out = Vec::new();
    let mut done_groups = Vec::new();
    let all = ordered(&action["actions"]);
    for sa in &all {
        let group = s(sa, "group");
        if !group.is_empty() && random.get(&group) == Some(&true) {
            if done_groups.contains(&group) {
                continue;
            }
            done_groups.push(group.clone());
            let choices: Vec<Vec<Step>> =
                all.iter().filter(|x| s(x, "group") == group).map(|x| step(x, cx)).filter(|c| !c.is_empty()).collect();
            if !choices.is_empty() {
                out.push(Step::Random { choices });
            }
        } else {
            if sa["type"].as_i64() == Some(120) {
                cx.note("an older If/Else (it ran other actions) needs redoing by hand".into());
                continue;
            }
            out.extend(step(sa, cx));
        }
    }
    out
}

fn trigger(
    t: &Value,
    commands: &HashMap<String, Value>,
    timers: &HashMap<String, Value>,
    cx: &mut Ctx,
) -> Option<(Trigger, f64, f64)> {
    let event = |kinds: &[&str]| Trigger::Event {
        kinds: kinds.iter().map(|k| k.to_string()).collect(),
        platforms: vec![],
        min_amount: 0.0,
        tests: true,
    };
    Some(match t["type"].as_i64()? {
        401 => {
            let c = commands.get(&s(t, "commandId"))?;
            let mut names = s(c, "command")
                .split(['\r', '\n'])
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
                .into_iter();
            let name = names.next()?;
            if c["mode"].as_i64() == Some(1) {
                cx.note(format!("the regex command “{name}” became a plain one"));
            }
            let who = if c["permittedGroups"]
                .as_array()
                .is_some_and(|g| g.iter().any(|x| x.as_str().is_some_and(|x| x.to_lowercase().contains("mod"))))
            {
                Who::Moderators
            } else {
                Who::Everyone
            };
            let cooldown = c["globalCooldown"].as_f64().unwrap_or(0.0);
            let per_user = c["userCooldown"].as_f64().unwrap_or(0.0);
            (Trigger::Command { name, aliases: names.collect(), who }, cooldown, per_user)
        }
        101 | 35011 => (event(&["follow"]), 0.0, 0.0),
        102 => (event(&["cheer"]), 0.0, 0.0),
        103..=106 | 4018 | 35014 => (event(&["sub"]), 0.0, 0.0),
        107 => (event(&["raid"]), 0.0, 0.0),
        4006 => (event(&["paid"]), 0.0, 0.0),
        133 | 4003 | 35010 => (Trigger::Chat { contains: String::new(), first_time: false }, 0.0, 0.0),
        120 => (Trigger::Chat { contains: String::new(), first_time: true }, 0.0, 0.0),
        701 => {
            let timer = timers.get(&s(t, "timerId"));
            let seconds = timer.and_then(|t| t["interval"].as_f64()).unwrap_or(600.0);
            let lines = timer.and_then(|t| t["lines"].as_u64()).unwrap_or(0);
            (Trigger::Timer { minutes: (seconds / 60.0).max(1.0), min_messages: lines }, 0.0, 0.0)
        }
        706 => (Trigger::Start, 0.0, 0.0),
        14005 | 154 | 4001 | 35012 => (Trigger::Output { what: "stream".into(), state: "start".into() }, 0.0, 0.0),
        14006 | 155 | 4002 | 35013 => (Trigger::Output { what: "stream".into(), state: "stop".into() }, 0.0, 0.0),
        15001 => (Trigger::Midi { message: "note".into(), number: -1, channel: 0 }, 0.0, 0.0),
        16001 => {
            cx.note("its hotkey wasn't brought over: set one (A hotkey) in Lightair".into());
            (Trigger::Manual, 0.0, 0.0)
        }
        501..=504 => {
            cx.note("its file watcher wasn't brought over: choose the file (A file changes)".into());
            (Trigger::Manual, 0.0, 0.0)
        }
        other => {
            cx.note(format!("a trigger of kind {other} has no counterpart (runs by hand)"));
            (Trigger::Manual, 0.0, 0.0)
        }
    })
}

/// Imports an export string.
pub fn import(text: &str) -> Result<Imported, String> {
    let v = decode(text)?;
    let data = if v["data"].is_object() { &v["data"] } else { &v };
    let by_id = |k: &str| -> HashMap<String, Value> {
        data[k].as_array().into_iter().flatten().map(|x| (s(x, "id"), x.clone())).collect()
    };
    let (commands, timers) = (by_id("commands"), by_id("timers"));
    let actions: Vec<&Value> = data["actions"].as_array().into_iter().flatten().collect();
    if actions.is_empty() {
        return Err("the export has no actions".into());
    }
    let names: HashMap<String, String> = actions.iter().map(|a| (s(a, "id"), s(a, "name"))).collect();
    let mut cx = Ctx { names: &names, notes: Vec::new(), action: String::new() };
    let mut automations = Vec::new();
    for a in actions {
        cx.action = s(a, "name");
        let triggers: Vec<&Value> =
            a["triggers"].as_array().into_iter().flatten().filter(|t| t["enabled"].as_bool() != Some(false)).collect();
        if triggers.len() > 1 {
            cx.note(format!("it had {} triggers; the first was kept", triggers.len()));
        }
        let (trigger, cooldown, user_cooldown) = triggers
            .first()
            .and_then(|t| trigger(t, &commands, &timers, &mut cx))
            .unwrap_or((Trigger::Manual, 0.0, 0.0));
        let steps =
            if a["subActions"].is_array() { steps(&a["subActions"], &mut cx) } else { legacy_steps(a, &mut cx) };
        automations.push(Automation {
            id: crate::new_id(),
            name: cx.action.clone(),
            enabled: a["enabled"].as_bool().unwrap_or(true),
            trigger,
            conditions: vec![],
            steps,
            queue: String::new(),
            cooldown,
            user_cooldown,
        });
    }
    Ok(Imported { automations, notes: cx.notes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn export(json: &Value) -> String {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(json.to_string().as_bytes()).unwrap();
        let mut bytes = b"SBAE".to_vec();
        bytes.extend(gz.finish().unwrap());
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn variables_become_templates() {
        assert_eq!(
            template("Hi %user%, you said %rawInput% (%input0%) 100%"),
            "Hi {{ name }}, you said {{ arg }} ({{ args.0 }}) 100%"
        );
        assert_eq!(template("%deaths% deaths"), "{{ vars.deaths }} deaths");
    }

    #[test]
    fn a_one_point_oh_export() {
        let json = json!({
            "meta": {"name": "pack"}, "version": 23,
            "data": {
                "commands": [{"id": "c1", "command": "!date\r\n!day", "mode": 0, "globalCooldown": 5, "userCooldown": 30}],
                "timers": [{"id": "t1", "interval": 900, "lines": 5}],
                "actions": [
                    {"id": "a1", "name": "Date", "enabled": true, "triggers": [{"type": 401, "commandId": "c1", "enabled": true}],
                     "subActions": [
                        {"type": 10, "text": "Hi %user%!", "index": 0, "enabled": true},
                        {"type": 1002, "value": "1500", "index": 1, "enabled": true},
                        {"type": 120, "input": "rawInput", "operation": 2, "value": "hello", "index": 2, "enabled": true, "subActions": [
                            {"type": 99901, "index": 0, "subActions": [{"type": 34, "sceneName": "Main", "index": 0}]},
                            {"type": 99902, "index": 1, "subActions": [{"type": 4, "actionId": "a2", "index": 0}]}
                        ]},
                        {"type": 99900, "random": true, "index": 3, "enabled": true, "subActions": [
                            {"type": 602, "message": "one", "index": 0}, {"type": 602, "message": "two", "index": 1}
                        ]},
                        {"type": 99999, "byteCode": base64::engine::general_purpose::STANDARD.encode("CPH.SendMessage(\"x\");"), "index": 4, "enabled": true},
                        {"type": 47, "index": 5, "enabled": true}
                     ]},
                    {"id": "a2", "name": "Ad", "enabled": false, "triggers": [{"type": 701, "timerId": "t1"}], "subActions": [{"type": 122, "variableName": "ads", "source": 1, "value": "", "index": 0}]}
                ]
            }
        });
        let r = import(&export(&json)).unwrap();
        assert_eq!(r.automations.len(), 2);
        let a = &r.automations[0];
        assert_eq!(
            a.trigger,
            Trigger::Command { name: "!date".into(), aliases: vec!["!day".into()], who: Who::Everyone }
        );
        assert_eq!((a.cooldown, a.user_cooldown), (5.0, 30.0));
        assert_eq!(a.steps[0], Step::Chat { text: "Hi {{ name }}!".into(), platform: "twitch".into() });
        assert_eq!(a.steps[1], Step::Wait { seconds: 1.5 });
        let Step::If { when, then, otherwise } = &a.steps[2] else { panic!("{:?}", a.steps[2]) };
        assert_eq!(
            (when[0].left.as_str(), when[0].op.as_str(), when[0].right.as_str()),
            ("{{ arg }}", "contains", "hello")
        );
        assert_eq!(then[0], Step::Call { method: "scene.switch".into(), params: json!({"scene": "Main"}) });
        assert_eq!(otherwise[0], Step::Run { automation: "Ad".into() });
        assert!(matches!(&a.steps[3], Step::Random { choices } if choices.len() == 2));
        assert!(matches!(&a.steps[4], Step::Script { code } if code.contains("CPH.SendMessage")));
        assert_eq!(a.steps.len(), 5);
        let b = &r.automations[1];
        assert!(!b.enabled);
        assert_eq!(b.trigger, Trigger::Timer { minutes: 15.0, min_messages: 5 });
        assert_eq!(b.steps[0], Step::Var { name: "ads".into(), op: "add".into(), value: "1".into(), user: false });
        assert!(r.notes.iter().any(|n| n.contains("kind 47")), "{:?}", r.notes);
        assert!(r.notes.iter().any(|n| n.contains("C#")));
    }

    #[test]
    fn an_older_export_with_a_random_group() {
        let json = json!({"version": 3, "actions": [{"id": "x", "name": "VFX", "enabled": true, "triggers": [],
            "actionGroups": [{"name": "vfx", "random": true}],
            "actions": [{"type": 10, "text": "a", "group": "vfx", "index": 0}, {"type": 10, "text": "b", "group": "vfx", "index": 1}, {"type": 1002, "value": "500", "index": 2}]}]});
        let r = import(&export(&json)).unwrap();
        let a = &r.automations[0];
        assert_eq!(a.trigger, Trigger::Manual);
        assert!(matches!(&a.steps[0], Step::Random { choices } if choices.len() == 2));
        assert_eq!(a.steps[1], Step::Wait { seconds: 0.5 });
    }

    #[test]
    fn not_an_export() {
        assert!(import("hello").is_err());
        // found inside other text
        let wrapped = format!(
            "# Import String\n\n{}\n\nThanks!",
            export(&json!({"actions": [{"id": "a", "name": "A", "subActions": []}]}))
        );
        assert_eq!(import(&wrapped).unwrap().automations.len(), 1);
        assert!(import(&base64::engine::general_purpose::STANDARD.encode("SBAEnot gzip")).is_err());
    }
}

#[cfg(test)]
mod real_exports {
    /// With LIGHTAIR_SB_DIRS (folders of export strings, one per .txt/.sb, separated by `:`):
    /// every one imports; what didn't come over, counted.
    #[test]
    fn imports_real_exports() {
        let Ok(dirs) = std::env::var("LIGHTAIR_SB_DIRS") else { return };
        let (mut files, mut ok, mut actions, mut steps) = (0, 0, 0, 0);
        let mut notes: std::collections::BTreeMap<String, usize> = Default::default();
        let mut failed = Vec::new();
        for dir in dirs.split(':') {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if !path.extension().is_some_and(|e| e == "txt" || e == "sb") {
                    continue;
                }
                files += 1;
                match super::import(&std::fs::read_to_string(&path).unwrap_or_default()) {
                    Ok(r) => {
                        ok += 1;
                        actions += r.automations.len();
                        steps += r.automations.iter().map(|a| a.steps.len()).sum::<usize>();
                        for n in r.notes {
                            let kind = n.split_once(": ").map_or(n.clone(), |(_, k)| k.to_string());
                            *notes.entry(kind).or_default() += 1;
                        }
                    }
                    Err(e) => failed.push(format!("{}: {e}", path.display())),
                }
            }
        }
        eprintln!("{ok}/{files} imported: {actions} automations, {steps} top-level steps");
        let mut top: Vec<_> = notes.into_iter().collect();
        top.sort_by_key(|t| std::cmp::Reverse(t.1));
        for (n, c) in top.iter().take(15) {
            eprintln!("  {c:4} × {n}");
        }
        for f in failed.iter().take(5) {
            eprintln!("FAILED {f}");
        }
    }
}
