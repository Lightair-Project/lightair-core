//! Platforms without captures here yet: nothing to list, nothing starts.

fn unsupported() -> crate::Error {
    crate::Error::Unsupported("captures are macOS only for now".into())
}

pub mod screen {
    use crate::screen::{Config, Display, Shareable, Target};
    use crate::{Error, VideoSink};

    pub fn access() -> bool {
        false
    }

    pub fn displays() -> Vec<Display> {
        Vec::new()
    }

    pub fn shareable() -> Result<Shareable, Error> {
        Err(super::unsupported())
    }

    pub struct Stream;

    impl Stream {
        pub fn start(_: &Target, _: &Config, _: Box<dyn VideoSink>) -> Result<Stream, Error> {
            Err(super::unsupported())
        }

        pub fn reconfigure(&self, _: &Config) -> Result<(), Error> {
            Err(super::unsupported())
        }

        pub fn size(&self) -> (u32, u32) {
            (0, 0)
        }
    }
}

pub mod camera {
    use crate::camera::{Access, Camera, Config, DeviceEvent, Format};
    use crate::{Error, VideoSink};

    pub fn access() -> Access {
        Access::Restricted
    }

    pub fn cameras() -> Vec<Camera> {
        Vec::new()
    }

    pub fn formats(id: &str) -> Result<Vec<Format>, Error> {
        Err(Error::NotFound(id.into()))
    }

    pub struct Session {
        format: Format,
    }

    impl Session {
        pub fn start(_: &str, _: &Config, _: Box<dyn VideoSink>) -> Result<Session, Error> {
            Err(super::unsupported())
        }

        pub fn format(&self) -> &Format {
            &self.format
        }

        pub fn fps(&self) -> f64 {
            0.0
        }
    }

    pub struct Watch;

    impl Watch {
        pub fn new(_: Box<dyn Fn(DeviceEvent) + Send + Sync>) -> Watch {
            Watch
        }
    }
}

pub mod sound {
    use crate::sound::{SoundApp, Target};
    use crate::{Error, SoundSink};

    pub fn supported() -> bool {
        false
    }

    pub fn apps() -> Vec<SoundApp> {
        Vec::new()
    }

    pub struct Capture;

    impl Capture {
        pub fn start(_: &Target, _: Box<dyn SoundSink>) -> Result<Capture, Error> {
            Err(super::unsupported())
        }

        pub fn format(&self) -> Option<(u32, u32)> {
            None
        }
    }
}
