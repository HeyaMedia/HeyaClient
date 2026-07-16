use serde::{Deserialize, Serialize};

pub const SYSTEM_MEDIA_PROTOCOL_VERSION: u16 = 1;
pub const MAX_SYSTEM_MEDIA_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_ARTWORK_BYTES: usize = 512 * 1024;
pub const MAX_ITEM_KEY_BYTES: usize = 128;
pub const MAX_TITLE_BYTES: usize = 512;
pub const MAX_SECONDARY_TEXT_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SystemMediaPlatform {
    Macos,
    Windows,
    Linux,
    Unsupported,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SystemMediaCapabilities {
    pub protocol_version: u16,
    pub available: bool,
    pub platform: SystemMediaPlatform,
    pub now_playing: bool,
    pub media_commands: bool,
    pub track_notifications: bool,
    pub track_notifications_enabled: bool,
    pub artwork: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SystemMediaArtwork {
    pub cache_key: String,
    pub mime_type: SystemMediaArtworkMime,
    pub base64_data: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub enum SystemMediaArtworkMime {
    #[serde(rename = "image/jpeg")]
    Jpeg,
    #[serde(rename = "image/png")]
    Png,
}

impl SystemMediaArtworkMime {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SystemMediaPlaybackState {
    Playing,
    Paused,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SystemMediaSnapshot {
    pub revision: u64,
    pub item_key: String,
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    pub duration_seconds: f64,
    pub position_seconds: f64,
    pub playback_state: SystemMediaPlaybackState,
    pub can_go_previous: bool,
    pub can_go_next: bool,
    pub can_seek: bool,
    #[serde(default)]
    pub artwork: Option<SystemMediaArtwork>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClearSystemMediaRequest {
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrackChangedNotificationRequest {
    pub revision: u64,
    pub item_key: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrackChangedNotificationResult {
    pub shown: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SystemMediaCommandEvent {
    pub command_sequence: u64,
    #[serde(flatten)]
    pub command: SystemMediaCommand,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SystemMediaCommand {
    Play,
    Pause,
    TogglePlayPause,
    Previous,
    Next,
    Stop,
    SeekTo { position_seconds: f64 },
    SeekBy { offset_seconds: f64 },
}
