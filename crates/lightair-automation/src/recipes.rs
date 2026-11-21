//! Ready-made automations (docs/plans/live-integrations.md, 2.6: the recipes people build in
//! Streamer.bot first), to start from and change.

use serde_json::json;

use crate::model::{Automation, Condition, Step, Trigger, Who};

pub struct Recipe {
    pub key: &'static str,
    pub name: &'static str,
    pub about: &'static str,
}

pub const RECIPES: &[Recipe] = &[
    Recipe {
        key: "shoutout",
        name: "!so shoutout",
        about: "Moderators type !so @name to point chat at another streamer",
    },
    Recipe {
        key: "death-counter",
        name: "Death counter",
        about: "!death adds one (moderators), !deaths says how many",
    },
    Recipe {
        key: "timed-message",
        name: "Timed message",
        about: "Posts a message every 15 minutes while chat is active",
    },
    Recipe {
        key: "tts-donations",
        name: "Read donations aloud",
        about: "Says the message of every donation, cheer or Super Chat on stream",
    },
    Recipe { key: "raid-welcome", name: "Raid welcome", about: "Thanks raiders in chat and tells them who raided" },
    Recipe {
        key: "first-chatter",
        name: "First-time chatter greeting",
        about: "Welcomes people the first time they chat",
    },
    Recipe { key: "points", name: "!points", about: "Tells people how many points they have" },
    Recipe { key: "giveaway", name: "Giveaway", about: "!join enters, moderators !draw a winner" },
    Recipe { key: "lurk", name: "!lurk", about: "Thanks people for lurking" },
    Recipe {
        key: "follow-scene",
        name: "Follow → show an object",
        about: "Shows a scene object for a few seconds on every follow",
    },
    Recipe { key: "blank", name: "Blank", about: "Start from nothing" },
];

fn chat(text: &str) -> Step {
    Step::Chat { text: text.into(), platform: String::new() }
}

/// A new automation from a recipe (a fresh id).
pub fn make(key: &str) -> Option<Automation> {
    let r = RECIPES.iter().find(|r| r.key == key)?;
    let command = |name: &str, who: Who| Trigger::Command { name: name.into(), aliases: vec![], who };
    let (trigger, steps, conditions, cooldown, user_cooldown) = match key {
        "shoutout" => (
            command("!so", Who::Moderators),
            vec![chat("Go show some love to {{ target }}! https://twitch.tv/{{ target | lower }}")],
            vec![Condition { left: "{{ target }}".into(), op: "is not".into(), right: String::new() }],
            5.0,
            0.0,
        ),
        "death-counter" => (
            command("!death", Who::Moderators),
            vec![
                Step::Var { name: "deaths".into(), op: "add".into(), value: "1".into(), user: false },
                chat("Deaths: {{ vars.deaths }}"),
            ],
            vec![],
            3.0,
            0.0,
        ),
        "timed-message" => (
            Trigger::Timer { minutes: 15.0, min_messages: 5 },
            vec![chat("Enjoying the stream? Follow so you don't miss the next one!")],
            vec![],
            0.0,
            0.0,
        ),
        "tts-donations" => (
            Trigger::Event {
                kinds: vec!["paid".into(), "cheer".into()],
                platforms: vec![],
                min_amount: 0.0,
                tests: true,
            },
            vec![Step::Wait { seconds: 2.0 }, Step::Speak { text: "{{ name }} says: {{ message }}".into() }],
            vec![Condition { left: "{{ message }}".into(), op: "is not".into(), right: String::new() }],
            0.0,
            0.0,
        ),
        "raid-welcome" => (
            Trigger::Event { kinds: vec!["raid".into()], platforms: vec![], min_amount: 0.0, tests: true },
            vec![chat("Welcome raiders! Thank you {{ name }} for bringing {{ viewers }} friends 💜")],
            vec![],
            0.0,
            0.0,
        ),
        "first-chatter" => (
            Trigger::Chat { contains: String::new(), first_time: true },
            vec![chat("Welcome to the stream, {{ name }}! 👋")],
            vec![],
            0.0,
            0.0,
        ),
        "points" => (
            command("!points", Who::Everyone),
            vec![chat("{{ name }}, you have {{ viewer.points }} {{ points_name }}.")],
            vec![],
            0.0,
            10.0,
        ),
        "giveaway" => (
            command("!join", Who::Everyone),
            vec![Step::If {
                when: vec![Condition { left: "{{ user_vars.entered }}".into(), op: "is".into(), right: "yes".into() }],
                then: vec![],
                otherwise: vec![
                    Step::Var { name: "entered".into(), op: "set".into(), value: "yes".into(), user: true },
                    Step::Var { name: "giveaway".into(), op: "push".into(), value: "{{ name }}".into(), user: false },
                    chat("{{ name }} is in the giveaway!"),
                ],
            }],
            vec![],
            0.0,
            0.0,
        ),
        "lurk" => {
            (command("!lurk", Who::Everyone), vec![chat("Thanks for lurking, {{ name }}! Enjoy 🍿")], vec![], 0.0, 60.0)
        }
        "follow-scene" => (
            Trigger::Event { kinds: vec!["follow".into()], platforms: vec![], min_amount: 0.0, tests: true },
            vec![
                Step::Call {
                    method: "object.update".into(),
                    params: json!({"object": "Follow overlay", "visible": true}),
                },
                Step::Wait { seconds: 5.0 },
                Step::Call {
                    method: "object.update".into(),
                    params: json!({"object": "Follow overlay", "visible": false}),
                },
            ],
            vec![],
            0.0,
            0.0,
        ),
        _ => (Trigger::Manual, vec![], vec![], 0.0, 0.0),
    };
    Some(Automation {
        id: crate::new_id(),
        name: r.name.into(),
        enabled: true,
        trigger,
        conditions,
        steps,
        queue: if key == "follow-scene" || key == "tts-donations" { "alerts".into() } else { String::new() },
        cooldown,
        user_cooldown,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_recipe_makes_an_automation() {
        for r in super::RECIPES {
            assert!(super::make(r.key).is_some(), "{}", r.key);
        }
    }
}
