//! A PNGtuber moving (docs/pngtuber-format.md, "Roles", "mic", "blink"): when it talks, which
//! mouth shows, when it blinks and what an expression shows. Time is only what each step says.

use lightair_pngtuber::{Avatar, Expression, Inputs, Layer, Pose, Puppet, Role};

fn layer(id: &str, role: Role, frame: u8) -> Layer {
    Layer {
        id: id.into(),
        name: id.into(),
        image: format!("layers/{id}.png"),
        x: 0.0,
        y: 0.0,
        width: 100.0,
        height: 100.0,
        pivot: [50.0, 100.0],
        role,
        frame,
        motion: 1.0,
        opacity: 1.0,
    }
}

/// An avatar with eyes, a four-frame mouth, a talking and a quiet layer, and no blinking (so it
/// doesn't get in the way of other tests). Mic: open −34, close −42, gate −58, attack 30 ms,
/// release 120 ms.
fn avatar() -> Avatar {
    let mut a = Avatar::new("Test", 200, 200);
    a.layers = vec![
        layer("body", Role::Part, 0),
        layer("eyes-open", Role::EyesOpen, 0),
        layer("eyes-shut", Role::EyesClosed, 0),
        layer("mouth-0", Role::Mouth, 0),
        layer("mouth-1", Role::Mouth, 1),
        layer("mouth-2", Role::Mouth, 2),
        layer("mouth-3", Role::Mouth, 3),
        layer("talk", Role::Talking, 0),
        layer("idle", Role::Quiet, 0),
    ];
    a.blink.every_s = 0.0;
    a
}

fn at(level_db: f32, dt: f32) -> Inputs<'static> {
    Inputs { level_db, push_to_talk: None, expression: None, dt }
}

/// Steps `seconds` of a steady level in 10 ms frames.
fn hold(p: &mut Puppet, level_db: f32, seconds: f32) -> Pose {
    let n = (seconds / 0.01).round() as usize;
    for _ in 0..n {
        p.step(&at(level_db, 0.01));
    }
    p.pose().clone()
}

/// The ids of the layers showing.
fn showing(p: &Puppet) -> Vec<String> {
    p.avatar().layers.iter().zip(&p.pose().layers).filter(|(_, s)| s.visible).map(|(l, _)| l.id.clone()).collect()
}

#[test]
fn talking_starts_at_the_open_level_not_between_the_two() {
    let mut p = Puppet::new(avatar());
    assert!(!hold(&mut p, -36.0, 2.0).talking, "between close and open doesn't start it");
    assert!(hold(&mut p, -34.0, 0.01).talking, "the open level starts it at once");
}

#[test]
fn talking_holds_between_the_levels_and_stops_after_the_release_below_close() {
    let mut p = Puppet::new(avatar());
    hold(&mut p, -30.0, 0.1);
    assert!(hold(&mut p, -40.0, 3.0).talking, "between close and open it keeps talking");
    assert!(hold(&mut p, -45.0, 0.10).talking, "below close, but not yet for the release (120 ms)");
    assert!(!hold(&mut p, -45.0, 0.03).talking, "below close for the release: it stops");
}

#[test]
fn a_dip_shorter_than_the_release_doesnt_stop_it() {
    let mut p = Puppet::new(avatar());
    hold(&mut p, -30.0, 0.1);
    // dips of 100 ms below close, each back above close for a frame: never 120 ms in a row
    for _ in 0..10 {
        hold(&mut p, -60.0, 0.10);
        hold(&mut p, -40.0, 0.01);
    }
    assert!(p.pose().talking);
}

#[test]
fn push_to_talk_released_keeps_it_quiet() {
    let mut p = Puppet::new(avatar());
    for _ in 0..50 {
        p.step(&Inputs { level_db: -10.0, push_to_talk: Some(false), expression: None, dt: 0.01 });
    }
    assert!(!p.pose().talking && p.pose().mouth == 0, "{:?}", p.pose());
    p.step(&Inputs { level_db: -10.0, push_to_talk: Some(true), expression: None, dt: 0.01 });
    assert!(p.pose().talking, "the key down lets the voice through");
}

#[test]
fn loudness_rises_with_the_attack_time_and_falls_with_the_release_time() {
    let e = (-1.0f32).exp();
    let mut p = Puppet::new(avatar());
    // −22 dB is 12 dB above open: full loudness. One attack time (30 ms) in: 1 − 1/e of it.
    let up = hold(&mut p, -22.0, 0.03).loudness;
    assert!((up - (1.0 - e)).abs() < 0.01, "after the attack time: {up}");
    let full = hold(&mut p, -22.0, 1.0).loudness;
    assert!(full > 0.99, "{full}");
    // silence: one release time (120 ms) later, 1/e of it left
    let down = hold(&mut p, f32::NEG_INFINITY, 0.12).loudness;
    assert!((down - full * e).abs() < 0.01, "after the release time: {down}");
}

#[test]
fn loudness_depends_on_time_not_on_the_frame_rate() {
    let after = |frames: usize| {
        let mut p = Puppet::new(avatar());
        for _ in 0..frames {
            p.step(&at(-26.0, 0.06 / frames as f32));
        }
        p.pose().loudness
    };
    let (slow, fast) = (after(2), after(60));
    assert!((slow - fast).abs() < 1e-3, "60 ms in 2 frames: {slow}, in 60 frames: {fast}");
}

#[test]
fn the_mouth_opens_wider_the_louder_the_voice() {
    let mut p = Puppet::new(avatar());
    assert_eq!(p.pose().mouth, 0);
    assert_eq!(showing(&p).iter().filter(|id| id.starts_with("mouth")).collect::<Vec<_>>(), ["mouth-0"]);
    let mut mouths = Vec::new();
    for db in [-33.0, -40.0, -32.0, -22.0] {
        hold(&mut p, db, 1.0);
        let shown: Vec<String> = showing(&p).into_iter().filter(|id| id.starts_with("mouth")).collect();
        assert_eq!(shown, [format!("mouth-{}", p.pose().mouth)], "only the chosen frame shows at {db} dB");
        mouths.push((db, p.pose().mouth));
    }
    // −33 starts it; −40 is quiet but still talking (above close); −22 is full loudness
    assert_eq!(mouths, [(-33.0, 2), (-40.0, 1), (-32.0, 2), (-22.0, 3)]);
    hold(&mut p, -70.0, 1.0);
    assert_eq!(p.pose().mouth, 0, "shut once quiet");
}

#[test]
fn a_single_mouth_frame_always_shows() {
    let mut a = avatar();
    a.layers.retain(|l| l.role != Role::Mouth || l.frame == 0);
    let mut p = Puppet::new(a);
    hold(&mut p, -22.0, 1.0);
    assert!(p.pose().talking && showing(&p).contains(&"mouth-0".to_string()), "{:?}", showing(&p));
}

#[test]
fn talking_and_quiet_layers_follow_the_voice() {
    let mut p = Puppet::new(avatar());
    let has = |p: &Puppet, id: &str| showing(p).iter().any(|s| s == id);
    assert!(has(&p, "idle") && !has(&p, "talk"));
    hold(&mut p, -25.0, 0.5);
    assert!(has(&p, "talk") && !has(&p, "idle"));
}

#[test]
fn an_avatar_without_a_mouth_still_talks() {
    let mut a = avatar();
    a.layers.retain(|l| l.role != Role::Mouth);
    let mut p = Puppet::new(a);
    hold(&mut p, -22.0, 0.5);
    assert!(p.pose().talking && showing(&p).contains(&"talk".to_string()), "{:?}", showing(&p));
}

/// Steps `seconds` in 10 ms frames, returning when each blink started and how long it lasted.
fn blinks(p: &mut Puppet, seconds: f32) -> Vec<(f32, f32)> {
    let mut out: Vec<(f32, f32)> = Vec::new();
    let mut was = false;
    for i in 1..=(seconds / 0.01).round() as usize {
        let now = p.step(&at(f32::NEG_INFINITY, 0.01)).blinking;
        if now && !was {
            out.push((i as f32 * 0.01, 0.0));
        }
        if now {
            out.last_mut().unwrap().1 += 0.01;
        }
        was = now;
    }
    out
}

#[test]
fn it_blinks_every_so_often_for_the_blinks_length() {
    let mut a = avatar();
    a.blink.every_s = 4.0;
    a.blink.length_ms = 140.0;
    a.blink.double = 0.0;
    let mut p = Puppet::new(a);
    let b = blinks(&mut p, 120.0);
    // each wait is 60–140% of every_s, so 120 s holds 20 to 50 blinks
    assert!((20..=50).contains(&b.len()), "{} blinks", b.len());
    assert!(b[0].0 >= 2.4 - 0.01, "first at {}", b[0].0);
    for w in b.windows(2) {
        let wait = w[1].0 - (w[0].0 + w[0].1);
        assert!((2.4 - 0.02..=5.6 + 0.02).contains(&wait), "eyes open for {wait} s between blinks");
    }
    for (start, length) in &b {
        assert!((length - 0.14).abs() < 0.015, "the blink at {start} lasted {length}");
    }
}

#[test]
fn while_blinking_the_shut_eyes_replace_the_open_ones() {
    let mut a = avatar();
    a.blink.every_s = 1.0;
    let mut p = Puppet::new(a);
    let mut seen = (false, false);
    for _ in 0..500 {
        let blinking = p.step(&at(f32::NEG_INFINITY, 0.01)).blinking;
        let s = showing(&p);
        let (open, shut) = (s.contains(&"eyes-open".to_string()), s.contains(&"eyes-shut".to_string()));
        assert_eq!((open, shut), (!blinking, blinking), "{s:?}");
        if blinking { seen.1 = true } else { seen.0 = true }
    }
    assert_eq!(seen, (true, true));
}

#[test]
fn it_never_blinks_when_told_not_to_or_without_shut_eyes() {
    let mut never = avatar();
    never.blink.every_s = 0.0;
    let mut no_shut_eyes = avatar();
    no_shut_eyes.blink.every_s = 1.0;
    no_shut_eyes.layers.retain(|l| l.role != Role::EyesClosed);
    for a in [never, no_shut_eyes] {
        let mut p = Puppet::new(a);
        assert!(blinks(&mut p, 30.0).is_empty());
        assert!(showing(&p).contains(&"eyes-open".to_string()));
    }
}

#[test]
fn a_double_blink_comes_as_a_pair() {
    let mut a = avatar();
    a.blink.every_s = 4.0;
    a.blink.double = 1.0;
    let mut p = Puppet::new(a);
    let b = blinks(&mut p, 60.0);
    assert!(b.len() >= 4 && b.len().is_multiple_of(2), "{b:?}");
    for pair in b.chunks(2) {
        let gap = pair[1].0 - pair[0].0;
        assert!(gap < 0.6, "the second of a pair {gap} s after the first: {b:?}");
    }
    for w in b.windows(2).skip(1).step_by(2) {
        assert!(w[1].0 - w[0].0 > 2.0, "pairs apart: {b:?}");
    }
}

#[test]
fn an_expression_shows_its_layers_and_hides_others() {
    let mut a = avatar();
    a.layers.push(layer("blush", Role::Extra, 0));
    a.layers.push(layer("hat", Role::Extra, 0));
    a.layers.push(layer("hair", Role::Part, 0));
    a.expressions = vec![
        Expression { name: "Shy".into(), show: vec!["blush".into()], ..Default::default() },
        Expression { name: "Hat".into(), show: vec!["hat".into()], hide: vec!["hair".into()], costume: true },
    ];
    let mut p = Puppet::new(a);
    let mut with = |e: Option<&str>| {
        p.step(&Inputs { level_db: f32::NEG_INFINITY, push_to_talk: None, expression: e, dt: 0.01 });
        let s = showing(&p);
        ["blush", "hat", "hair"].map(|id| s.iter().any(|x| x == id))
    };
    assert_eq!(with(None), [false, false, true], "extras hide without an expression");
    assert_eq!(with(Some("Shy")), [true, false, true]);
    assert_eq!(with(Some("Hat")), [false, true, false], "a costume shows the hat and hides the hair");
    assert_eq!(with(Some("Nobody")), [false, false, true], "an unknown name is no expression");
    assert_eq!(with(None), [false, false, true]);
}

#[test]
fn an_expression_shows_a_layer_only_when_its_role_would() {
    let mut a = avatar();
    a.expressions = vec![Expression { name: "Chatty".into(), show: vec!["talk".into()], ..Default::default() }];
    let mut p = Puppet::new(a);
    let talk_shows = |p: &mut Puppet, db: f32| {
        for _ in 0..50 {
            p.step(&Inputs { level_db: db, push_to_talk: None, expression: Some("Chatty"), dt: 0.01 });
        }
        showing(p).contains(&"talk".to_string())
    };
    assert!(!talk_shows(&mut p, -70.0), "a talking layer stays hidden while quiet");
    assert!(talk_shows(&mut p, -25.0));
}
