//! Native playback service and its narrow, versioned WebView boundary.
//!
//! Raw renderer operations remain inside Rust. The selected Heya origin gets a
//! semantic playback object backed by two origin-scoped Tauri commands. The
//! page receives no generic application API, and every operation is validated
//! again against the current saved server origin before dispatch.

mod bridge;
mod engine;
mod manager;
#[cfg(all(
    feature = "native-mpv",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
mod mpv;
mod protocol;
mod runtime_install;
#[cfg(all(feature = "native-mpv", target_os = "macos"))]
mod surface_macos;
#[cfg(all(feature = "native-mpv", target_os = "windows"))]
mod surface_windows;
#[allow(dead_code)]
mod validation;

pub use bridge::*;
pub use engine::*;
pub use manager::*;
pub use protocol::*;
pub use runtime_install::*;
pub(crate) use validation::validate_load;
pub use validation::{PlaybackValidationError, ValidatedPlaybackLoad};

#[cfg(all(
    debug_assertions,
    feature = "native-mpv",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
pub use mpv::start_development_harness;
#[cfg(all(
    feature = "native-mpv",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
pub use mpv::{
    configure_bundled_vulkan_loader, MpvEngineFactory, NATIVE_MPV_FULLSCREEN_OFF_MENU_ID,
    NATIVE_MPV_FULLSCREEN_ON_MENU_ID, NATIVE_MPV_SPIKE_MENU_ID,
};
