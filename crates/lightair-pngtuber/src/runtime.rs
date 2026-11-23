//! A PNGtuber moving: each frame, the microphone's level (and push to talk, and an expression)
//! in, each layer's place and visibility out.
//!
//! - **Talking:** it starts above the mic's open level and stops once the voice has stayed below
//!   the lower close level for the release time (hysteresis, so it doesn't chatter); below the
//!   gate it's silence. Push to talk, when on, overrides.
//! - **Loudness:** smoothed with the attack and release times; it picks the mouth's frame (shut
//!   when quiet) and drives the bounce and squash.
//! - **Blinking:** on its own, at varying intervals, sometimes twice.
//! - **Motion:** while talking a bounce and a squash and stretch with the voice; while quiet a slow
//!   breath; a hop when it starts talking or the expression changes. The whole avatar moves about
//!   the middle of its canvas's bottom edge; each layer as much as its `motion` says.
//! - **Expressions:** show and hide layers over the roles' choice.

use crate::model::{Avatar, Role};

/// What drives it this frame.
#[derive(Clone, Copy, Debug)]
pub struct Inputs<'a> {
    /// The microphone's level, dBFS (`f32::NEG_INFINITY` for none).
    pub level_db: f32,
    /// Push to talk: `None` when it isn't used; else whether the key is down.
    pub push_to_talk: Option<bool>,
    /// The expression or costume on, by name.
    pub expression: Option<&'a str>,
    /// Seconds since the last frame.
    pub dt: f32,
}

/// A layer this frame: whether it shows, and how the avatar's motion moves it (scaled by
/// `sx, sy` about `about`, then moved down by `dy`; design canvas pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerPose {
    pub visible: bool,
    pub opacity: f32,
    pub sx: f32,
    pub sy: f32,
    pub dy: f32,
    pub about: [f32; 2],
}

impl LayerPose {
    /// Where a design canvas point of this layer is this frame.
    pub fn place(&self, [x, y]: [f32; 2]) -> [f32; 2] {
        [self.about[0] + (x - self.about[0]) * self.sx, self.about[1] + (y - self.about[1]) * self.sy + self.dy]
    }
}

/// The avatar this frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pose {
    /// In `Avatar::layers`' order.
    pub layers: Vec<LayerPose>,
    pub talking: bool,
    pub blinking: bool,
    /// The voice's loudness, smoothed (0 silent .. 1 loud).
    pub loudness: f32,
    /// The mouth frame shown.
    pub mouth: usize,
}

/// A PNGtuber being animated.
pub struct Puppet {
    avatar: Avatar,
    time: f32,
    talking: bool,
    quiet_for: f32,
    loudness: f32,
    next_blink: f32,
    shut_for: f32,
    second_blink: Option<f32>,
    hop: Option<f32>,
    expression: Option<String>,
    rng: u64,
    pose: Pose,
}

/// How long a hop lasts (seconds).
const HOP: f32 = 0.28;

impl Puppet {
    pub fn new(avatar: Avatar) -> Puppet {
        let seed =
            avatar.name.bytes().fold(0x9E37_79B9_7F4A_7C15u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01B3)) | 1;
        let mut p = Puppet {
            avatar,
            time: 0.0,
            talking: false,
            quiet_for: 0.0,
            loudness: 0.0,
            next_blink: 0.0,
            shut_for: 0.0,
            second_blink: None,
            hop: None,
            expression: None,
            rng: seed,
            pose: Pose::default(),
        };
        p.next_blink = p.blink_interval();
        p.step(&Inputs { level_db: f32::NEG_INFINITY, push_to_talk: None, expression: None, dt: 0.0 });
        p
    }

    pub fn avatar(&self) -> &Avatar {
        &self.avatar
    }

    /// Changes the avatar's settings or layers in place (the editor), keeping the motion going.
    pub fn set_avatar(&mut self, avatar: Avatar) {
        self.avatar = avatar;
    }

    pub fn pose(&self) -> &Pose {
        &self.pose
    }

    fn random(&mut self) -> f32 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32
    }

    fn blink_interval(&mut self) -> f32 {
        let every = self.avatar.blink.every_s;
        if every <= 0.0 { f32::INFINITY } else { every * (0.6 + 0.8 * self.random()) }
    }

    /// Moves it on by one frame.
    pub fn step(&mut self, i: &Inputs) -> &Pose {
        let dt = if i.dt.is_finite() { i.dt.clamp(0.0, 0.25) } else { 0.0 };
        self.time += dt;
        let mic = self.avatar.mic;
        let db = if i.level_db.is_finite() { i.level_db } else { -200.0 };
        let heard = db > mic.gate_db;

        // talking: hysteresis, a release, push to talk over it
        let was = self.talking;
        match i.push_to_talk {
            Some(false) => {
                self.talking = false;
                self.quiet_for = 0.0;
            }
            _ => {
                if !self.talking && heard && db >= mic.open_db {
                    self.talking = true;
                    self.quiet_for = 0.0;
                } else if self.talking && (!heard || db < mic.close_db) {
                    self.quiet_for += dt;
                    if self.quiet_for * 1000.0 >= mic.release_ms.max(0.0) {
                        self.talking = false;
                    }
                } else {
                    self.quiet_for = 0.0;
                }
            }
        }

        // loudness: from the close level to well above the open one, smoothed
        let top = mic.open_db.max(mic.close_db + 1.0) + 12.0;
        let target =
            if heard && self.talking { ((db - mic.close_db) / (top - mic.close_db)).clamp(0.0, 1.0) } else { 0.0 };
        let tau = if target > self.loudness { mic.attack_ms } else { mic.release_ms } / 1000.0;
        let k = if tau <= 0.0 { 1.0 } else { 1.0 - (-dt / tau).exp() };
        self.loudness += (target - self.loudness) * k;

        // a hop when it starts talking or the expression changes
        let expression = i.expression.filter(|e| !e.is_empty()).map(str::to_string);
        if (self.talking && !was) || expression != self.expression {
            self.hop = Some(0.0);
        }
        self.expression = expression;
        if let Some(t) = self.hop.as_mut() {
            *t += dt;
            if *t >= HOP {
                self.hop = None;
            }
        }

        // blinking
        if self.shut_for > 0.0 {
            self.shut_for -= dt;
        } else if let Some(t) = self.second_blink.as_mut() {
            *t -= dt;
            if *t <= 0.0 {
                self.second_blink = None;
                self.shut_for = self.avatar.blink.length_ms / 1000.0;
            }
        } else {
            self.next_blink -= dt;
            if self.next_blink <= 0.0 {
                self.shut_for = self.avatar.blink.length_ms / 1000.0;
                if self.random() < self.avatar.blink.double {
                    self.second_blink = Some(0.12 + self.shut_for);
                }
                self.next_blink = self.blink_interval();
            }
        }
        let blinking = self.shut_for > 0.0 && self.avatar.layers.iter().any(|l| l.role == Role::EyesClosed);

        // the mouth's frame
        let frames = self.avatar.mouth_frames();
        let mouth = if !self.talking || frames < 2 {
            0
        } else {
            (1 + (self.loudness * (frames - 1) as f32) as usize).min(frames - 1)
        };

        // motion, about the middle of the bottom edge
        let (w, h) = (self.avatar.width as f32, self.avatar.height as f32);
        let m = self.avatar.motion;
        let n = self.loudness;
        let breath = if self.talking { 0.0 } else { (self.time * std::f32::consts::TAU / 3.6).sin() };
        let hop = self.hop.map_or(0.0, |t| {
            let u = (t / HOP).clamp(0.0, 1.0);
            4.0 * u * (1.0 - u)
        });
        let sy = 1.0 + m.squash * 0.045 * n + m.breathing * 0.008 * breath;
        let sx = 1.0 - m.squash * 0.03 * n;
        let dy = -(m.bounce * 0.035 * h * n) - m.breathing * 0.004 * h * (breath + 1.0) * 0.5 - m.hop * 0.05 * h * hop;
        let about = [w * 0.5, h];

        // which layers show
        let on = self.expression.as_deref().and_then(|e| self.avatar.expressions.iter().find(|x| x.name == e));
        let talking = self.talking;
        self.pose.layers = self
            .avatar
            .layers
            .iter()
            .map(|l| {
                let by_role = match l.role {
                    Role::Part => true,
                    Role::EyesOpen => !blinking,
                    Role::EyesClosed => blinking,
                    Role::Mouth => l.frame as usize == mouth,
                    Role::Talking => talking,
                    Role::Quiet => !talking,
                    Role::Extra => false,
                };
                let visible = match on {
                    Some(x) if x.hide.contains(&l.id) => false,
                    Some(x) if x.show.contains(&l.id) => by_role || l.role == Role::Extra,
                    _ => by_role,
                };
                let k = l.motion.clamp(0.0, 1.0);
                LayerPose {
                    visible,
                    opacity: l.opacity.clamp(0.0, 1.0),
                    sx: 1.0 + (sx - 1.0) * k,
                    sy: 1.0 + (sy - 1.0) * k,
                    dy: dy * k,
                    about,
                }
            })
            .collect();
        self.pose.talking = talking;
        self.pose.blinking = blinking;
        self.pose.loudness = n;
        self.pose.mouth = mouth;
        &self.pose
    }

    /// The expressions and costumes, by name, in order.
    pub fn expressions(&self) -> Vec<String> {
        self.avatar.expressions.iter().map(|e| e.name.clone()).collect()
    }

    /// Where an anchor is this frame: its point and how much it's grown (design canvas pixels).
    pub fn anchor(&self, i: usize) -> Option<([f32; 2], f32)> {
        let a = self.avatar.anchors.get(i)?;
        let pose = match &a.layer {
            Some(id) => {
                self.avatar.layers.iter().position(|l| l.id == *id).and_then(|j| self.pose.layers.get(j)).copied()
            }
            None => {
                let full = self.pose.layers.iter().find(|p| p.sx != 1.0 || p.sy != 1.0 || p.dy != 0.0).copied();
                full.or(self.pose.layers.first().copied())
            }
        }?;
        Some((pose.place([a.x, a.y]), pose.sy))
    }

    /// A layer's box this frame: its four corners (top left, top right, bottom right, bottom left).
    pub fn layer_corners(&self, i: usize) -> Option<[[f32; 2]; 4]> {
        let l = self.avatar.layers.get(i)?;
        let p = self.pose.layers.get(i)?;
        Some([
            p.place([l.x, l.y]),
            p.place([l.x + l.width, l.y]),
            p.place([l.x + l.width, l.y + l.height]),
            p.place([l.x, l.y + l.height]),
        ])
    }
}
