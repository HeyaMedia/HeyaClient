//! Shared types for the audio engine.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Engine playback state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineState {
    Stopped,
    Buffering,
    Playing,
    Paused,
}

impl EngineState {
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Buffering => 1,
            Self::Playing => 2,
            Self::Paused => 3,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Stopped,
            1 => Self::Buffering,
            2 => Self::Playing,
            3 => Self::Paused,
            _ => Self::Stopped,
        }
    }
}

/// Identifies which physical deck (A or B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckId {
    A,
    B,
}

/// Origin-validated media descriptor accepted by the native audio core.
///
/// The grant deliberately has a redacted Debug representation and is never
/// serialized back to the WebView or written to logs.
#[derive(Clone)]
pub struct AudioSource {
    pub media_url: String,
    pub playback_grant: PlaybackGrant,
    pub format_hint: Option<String>,
}

#[derive(Clone)]
pub struct PlaybackGrant(String);

impl PlaybackGrant {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PlaybackGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PlaybackGrant([redacted])")
    }
}

impl fmt::Debug for AudioSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AudioSource")
            .field("media_url", &"[redacted]")
            .field("playback_grant", &self.playback_grant)
            .field("format_hint", &self.format_hint)
            .finish()
    }
}

impl DeckId {
    pub fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

/// Metadata for a track loaded into a deck.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackMeta {
    pub rating_key: i64,
    pub duration_ms: u64,
    pub parent_key: String,
    pub gain_db: Option<f32>,
    pub skip_crossfade: bool,
    pub start_ramp: Option<String>,
    pub end_ramp: Option<String>,
    pub intro_end_ms: Option<u64>,
    pub outro_start_ms: Option<u64>,
    pub fade_start_ms: Option<u64>,
    pub silence_start_ms: Option<u64>,
}
