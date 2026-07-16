use super::{
    BridgeErrorCode, NativePlaybackState, NativeVideoSurface, PlaybackCapabilities,
    PlaybackCommand, PlaybackDiagnostics, TerminationReason, ValidatedPlaybackLoad,
};
#[cfg(debug_assertions)]
use std::path::PathBuf;
use std::{error::Error, fmt, time::Duration};

/// The renderer input is selected by native code. `DevelopmentFile` and
/// `Synthetic` are never represented in the WebView bridge request schema.
pub enum EngineMedia {
    Production(ValidatedPlaybackLoad),
    #[cfg(debug_assertions)]
    DevelopmentFile(PathBuf),
    #[cfg(debug_assertions)]
    Synthetic,
}

impl fmt::Debug for EngineMedia {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Production(_) => formatter.write_str("EngineMedia::Production([redacted])"),
            #[cfg(debug_assertions)]
            Self::DevelopmentFile(_) => {
                formatter.write_str("EngineMedia::DevelopmentFile([redacted])")
            }
            #[cfg(debug_assertions)]
            Self::Synthetic => formatter.write_str("EngineMedia::Synthetic"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateUpdateKind {
    Structural,
    Position,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    State {
        state: NativePlaybackState,
        kind: StateUpdateKind,
    },
    Diagnostics {
        diagnostics: Box<Option<PlaybackDiagnostics>>,
        structural: bool,
    },
    Terminated(TerminationReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineError {
    pub code: BridgeErrorCode,
    pub message: String,
}

impl EngineError {
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            code: BridgeErrorCode::BackendUnavailable,
            message: message.into(),
        }
    }

    pub fn command(message: impl Into<String>) -> Self {
        Self {
            code: BridgeErrorCode::CommandFailed,
            message: message.into(),
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for EngineError {}

/// A playback engine is owned by exactly one serialized renderer worker.
/// Neither raw MPV commands nor raw properties cross this interface.
pub trait PlaybackEngine: Send + 'static {
    fn video_surface(&self) -> NativeVideoSurface;
    fn command(&mut self, command: &PlaybackCommand) -> Result<(), EngineError>;
    fn poll_event(&mut self, timeout: Duration) -> Result<Option<EngineEvent>, EngineError>;

    /// Must return only after this engine no longer produces audio or video.
    fn stop(&mut self, reason: TerminationReason) -> Result<(), EngineError>;
}

pub trait PlaybackEngineFactory: Send + Sync + 'static {
    fn capabilities(&self) -> PlaybackCapabilities;
    fn create(&self, media: EngineMedia) -> Result<Box<dyn PlaybackEngine>, EngineError>;
}

#[derive(Default)]
pub struct UnavailableEngineFactory;

impl PlaybackEngineFactory for UnavailableEngineFactory {
    fn capabilities(&self) -> PlaybackCapabilities {
        PlaybackCapabilities::mpv(
            false,
            NativeVideoSurface::NativeWindow,
            Some(BridgeErrorCode::BackendUnavailable),
        )
    }

    fn create(&self, _media: EngineMedia) -> Result<Box<dyn PlaybackEngine>, EngineError> {
        Err(EngineError::unavailable(
            "native MPV support is not included in this build",
        ))
    }
}

/// Only allow short, non-path diagnostic labels through the normalization
/// boundary. Identifiers such as codecs, formats, languages and device labels
/// do not need URL, header or filesystem syntax.
#[allow(dead_code)]
pub(crate) fn sanitize_diagnostic_label(value: impl AsRef<str>) -> Option<String> {
    let value = value.as_ref().trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return None;
    }

    let lower = value.to_ascii_lowercase();
    let looks_sensitive = value.contains("://")
        || value.contains('\\')
        || value.starts_with('/')
        || value.starts_with('~')
        || lower.contains("authorization")
        || lower.contains("cookie")
        || lower.contains("playback-grant")
        || lower.contains("playback_grant");
    (!looks_sensitive).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::sanitize_diagnostic_label;

    #[test]
    fn diagnostic_labels_strip_paths_urls_headers_and_credentials() {
        for value in [
            "/Users/alice/Avatar.mkv",
            "https://heya.example/movie.m3u8",
            r"C:\\Movies\\Avatar.mkv",
            "Authorization: Bearer secret",
            "Cookie: session=secret",
            "playback_grant=secret",
        ] {
            assert_eq!(sanitize_diagnostic_label(value), None, "{value}");
        }

        assert_eq!(
            sanitize_diagnostic_label("videotoolbox"),
            Some("videotoolbox".to_string())
        );
        assert_eq!(
            sanitize_diagnostic_label("Built-in Audio Output"),
            Some("Built-in Audio Output".to_string())
        );
    }
}
