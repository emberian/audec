//! Public desktop-application facade.
//!
//! This facade is intentionally smaller than Audec's implementation module
//! graph. Headless and worker binaries should get their own similarly narrow
//! facades instead of depending on GPUI-facing types.

pub use crate::ui_platform::{run, LaunchOptions};
