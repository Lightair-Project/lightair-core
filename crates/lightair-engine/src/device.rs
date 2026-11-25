//! Device classes: what a phone is offered by default.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    A,
    B,
    C,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub platform: String,
    pub model: String,
    pub soc: Option<String>,
    pub memory_mb: u64,
    pub display_refresh_hz: u32,
    pub hevc_encode: bool,
    pub max_concurrent_video_encodes: Option<u32>,
    /// Two cameras can run at once (iOS `AVCaptureMultiCamSession.isMultiCamSupported`,
    /// Android a concurrent pair in `CameraManager.getConcurrentCameraIds`).
    pub multi_camera: bool,
    /// The device's time zone now, in minutes east of UTC (file names, clock timers).
    pub utc_offset_min: i32,
}

impl DeviceInfo {
    /// From the chip and memory (the 10-second benchmark refines it later).
    pub fn class(&self) -> Class {
        let model = self.model.as_str();
        if self.platform == "ios" {
            // "iPhone16,1" is the 15 Pro; "iPhone14,x" the 13 and 14; "iPhone15,x" the 14 Pro and 15
            let major =
                model.strip_prefix("iPhone").and_then(|m| m.split(',').next()).and_then(|m| m.parse::<u32>().ok());
            return match major {
                Some(m) if m >= 16 => Class::A,
                Some(m) if m >= 14 => Class::B,
                Some(_) => Class::C,
                // iPads and the simulator: by memory
                None if self.memory_mb >= 8_000 => Class::A,
                None if self.memory_mb >= 4_000 => Class::B,
                None => Class::C,
            };
        }
        let soc = self.soc.as_deref().unwrap_or("").to_ascii_lowercase();
        if ["sm8650", "sm8750", "tensor g4", "tensor g5"].iter().any(|s| soc.contains(s)) || self.memory_mb >= 12_000 {
            Class::A
        } else if ["sm8450", "sm8475", "sm8550", "mt6985", "mt6989"].iter().any(|s| soc.contains(s))
            || self.memory_mb >= 8_000
        {
            Class::B
        } else {
            Class::C
        }
    }

    /// The default format's (short side, frame rate).
    pub fn default_format(&self) -> (u32, u32) {
        match self.class() {
            Class::A => (1080, 60),
            Class::B => (1080, 30),
            Class::C => (720, 30),
        }
    }

    /// Video encodes the phone can run at once.
    /// Formats drawn at once (two on classes A and B).
    pub fn formats_at_once(&self) -> u32 {
        match self.class() {
            Class::A | Class::B => 2,
            Class::C => 1,
        }
    }

    pub fn video_encodes(&self) -> u32 {
        let by_class = match self.class() {
            Class::A => 3,
            Class::B => 2,
            Class::C => 1,
        };
        self.max_concurrent_video_encodes.map_or(by_class, |m| m.min(by_class))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes() {
        let ios =
            |m: &str| DeviceInfo { platform: "ios".into(), model: m.into(), memory_mb: 6000, ..Default::default() };
        assert_eq!(ios("iPhone16,1").class(), Class::A);
        assert_eq!(ios("iPhone15,4").class(), Class::B);
        assert_eq!(ios("iPhone13,2").class(), Class::C);
        assert_eq!(ios("arm64").default_format(), (1080, 30));
    }
}
