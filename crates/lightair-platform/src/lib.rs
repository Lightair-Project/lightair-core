//! What Lightair needs from the operating system (docs/plans/windows.md, "The lightair-platform
//! crate"): one module per concern, each with a macOS and a Windows implementation behind the
//! same functions. The rest of Lightair calls these rather than OS APIs.

pub mod apps;
pub mod devices;
pub mod files;
pub mod input;
pub mod net;
pub mod paths;
pub mod pick_color;
pub mod pointer_badge;
pub mod realtime;
pub mod system_pointer;
