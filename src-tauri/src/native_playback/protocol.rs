use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use std::fmt;
use uuid::Uuid;

pub const NATIVE_PLAYBACK_PROTOCOL_VERSION: u16 = 1;
const MAX_IDENTIFIER_BYTES: usize = 256;

/// An opaque, server-issued credential scoped to media playback.
///
/// Deliberately does not implement `Display` or `Serialize`: it may enter the
/// native process, but must not be reflected back to the WebView or logs.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
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

/// Arbitrary HTTP headers, MPV options, filesystem paths, and executable
/// arguments are intentionally absent from this contract.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaybackLoadRequest {
    pub media_url: String,
    pub playback_grant: PlaybackGrant,
    #[serde(default)]
    pub start_position_seconds: Option<f64>,
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Serialize, PartialEq, Eq, Hash)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                validate_identifier(stringify!($name), &value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(D::Error::custom)
            }
        }
    };
}

string_id!(CommandId);
string_id!(NormalizedTrackId);
string_id!(ServerVariantId);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct RendererSessionId(String);

impl RendererSessionId {
    pub(crate) fn generate() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct PageInstanceId(String);

impl PageInstanceId {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        Uuid::parse_str(&value).map_err(|_| "pageInstanceId must be a UUID".to_string())?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PageInstanceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

fn validate_identifier(kind: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{kind} must not be empty"));
    }
    if value.len() > MAX_IDENTIFIER_BYTES {
        return Err(format!("{kind} is too long"));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(format!("{kind} contains unsupported characters"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RendererLifecycle {
    Loading,
    Active,
    Stopping,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    Ended,
    Stopped,
    WindowClosed,
    Disposed,
    Failed,
    NativeCrashed,
    LoggedOut,
    ServerSwitched,
    AppQuit,
}

impl TerminationReason {
    /// Only a natural end-of-file is allowed to advance Heya's queue.
    pub fn advances_queue(self) -> bool {
        self == Self::Ended
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum PlaybackCommand {
    Play,
    Pause,
    Seek {
        #[serde(rename = "positionSeconds")]
        position_seconds: f64,
    },
    SetVolume {
        volume: f64,
    },
    SetMuted {
        muted: bool,
    },
    SetFullscreen {
        fullscreen: bool,
    },
    SelectAudioTrack {
        #[serde(rename = "trackId")]
        track_id: NormalizedTrackId,
    },
    SelectSubtitleTrack {
        #[serde(rename = "trackId")]
        track_id: Option<NormalizedTrackId>,
    },
    SelectVariant {
        #[serde(rename = "variantId")]
        variant_id: ServerVariantId,
    },
    Stop,
}

impl PlaybackCommand {
    pub fn validate(&self) -> Result<(), BridgeError> {
        match self {
            Self::Seek { position_seconds }
                if !position_seconds.is_finite() || *position_seconds < 0.0 =>
            {
                Err(BridgeError::invalid_request(
                    "seek position must be a finite non-negative number",
                ))
            }
            Self::SetVolume { volume } if !volume.is_finite() || !(0.0..=1.0).contains(volume) => {
                Err(BridgeError::invalid_request(
                    "volume must be a finite number between 0 and 1",
                ))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackCommandRequest {
    pub renderer_session_id: RendererSessionId,
    pub command_id: CommandId,
    #[serde(flatten)]
    pub command: PlaybackCommand,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DisposePlaybackRequest {
    pub renderer_session_id: RendererSessionId,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackCapabilities {
    pub protocol_version: u16,
    pub backend: &'static str,
    pub available: bool,
    pub video_surface: NativeVideoSurface,
    pub diagnostics: bool,
    pub audio_track_selection: bool,
    pub subtitle_track_selection: bool,
    pub quality_selection: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<BridgeErrorCode>,
}

impl PlaybackCapabilities {
    pub fn mpv(
        available: bool,
        video_surface: NativeVideoSurface,
        unavailable_reason: Option<BridgeErrorCode>,
    ) -> Self {
        Self {
            protocol_version: NATIVE_PLAYBACK_PROTOCOL_VERSION,
            backend: "mpv",
            available,
            video_surface,
            diagnostics: available,
            audio_track_selection: available,
            subtitle_track_selection: available,
            quality_selection: available,
            unavailable_reason,
        }
    }
}

/// The native renderer presentation selected for one playback session.
///
/// This is descriptive UI metadata. It never changes origin validation,
/// grant validation, authentication, or authorization.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NativeVideoSurface {
    NativeWindow,
    NativeSurface,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackLoadResult {
    pub renderer_session_id: RendererSessionId,
    pub video_surface: NativeVideoSurface,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommandResult {
    pub renderer_session_id: RendererSessionId,
    pub command_id: CommandId,
    pub command_sequence: u64,
    pub accepted: bool,
    pub duplicate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PlaybackError>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackError {
    pub code: BridgeErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NativePlaybackState {
    pub playing: bool,
    pub paused: bool,
    pub ended: bool,
    pub loading: bool,
    pub buffering: bool,
    /// Whether the selected native presentation surface has displayed video.
    /// For embedded rendering this changes only after the first real frame is
    /// swapped, so the WebView can remain opaque during decoder startup, and
    /// returns to false when the session terminates and the surface goes away.
    pub video_surface_ready: bool,
    pub current_time: f64,
    pub duration: f64,
    pub buffered: f64,
    pub volume: f64,
    pub muted: bool,
    pub fullscreen: bool,
    pub seek_revision: u64,
    pub audio_tracks: Vec<NativeTrack>,
    pub subtitle_tracks: Vec<NativeTrack>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_audio_track_id: Option<NormalizedTrackId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_subtitle_track_id: Option<NormalizedTrackId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PlaybackError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<TerminationReason>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeTrack {
    pub id: NormalizedTrackId,
    pub kind: NativeTrackKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub selected: bool,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeTrackKind {
    Audio,
    Subtitle,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NativeStateEvent {
    pub protocol_version: u16,
    pub renderer_session_id: RendererSessionId,
    pub state_revision: u64,
    pub payload: NativePlaybackState,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NativeDiagnosticsEvent {
    pub protocol_version: u16,
    pub renderer_session_id: RendererSessionId,
    pub diagnostics_revision: u64,
    pub payload: Option<PlaybackDiagnostics>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackDiagnostics {
    pub backend: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampled_at_milliseconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthDiagnostics>,
}

impl Default for PlaybackDiagnostics {
    fn default() -> Self {
        Self {
            backend: "mpv",
            sampled_at_milliseconds: None,
            transport: None,
            video: None,
            audio: None,
            health: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TransportDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffered_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffered_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_bytes_per_second: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VideoDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<VideoSourceDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoded: Option<VideoDecodedDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<VideoOutputDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<VideoColorDiagnostics>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VideoSourceDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nominal_frames_per_second: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_bits_per_second: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VideoDecodedDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_frames_per_second: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardware_decoder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardware_interop: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VideoOutputDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_format: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VideoColorDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primaries: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matrix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dolby_vision_profile: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_content_light: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_frame_average_light: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AudioDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<AudioSourceDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<AudioOutputDiagnostics>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AudioSourceDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_bits_per_second: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AudioOutputDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_format: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HealthDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoded_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoder_dropped_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mistimed_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub av_sync_milliseconds: Option<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeErrorCode {
    InvalidRequest,
    ProtocolMismatch,
    OriginNotAllowed,
    PlaybackGrantRequired,
    BackendUnavailable,
    UnknownSession,
    RendererStopping,
    CommandFailed,
    InternalError,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BridgeError {
    pub code: BridgeErrorCode,
    pub message: String,
}

impl BridgeError {
    pub fn new(code: BridgeErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(BridgeErrorCode::InvalidRequest, message)
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(untagged)]
pub enum BridgeResponse<T> {
    Success { ok: bool, value: T },
    Failure { ok: bool, error: BridgeError },
}

impl<T> BridgeResponse<T> {
    pub fn success(value: T) -> Self {
        Self::Success { ok: true, value }
    }

    pub fn failure(error: BridgeError) -> Self {
        Self::Failure { ok: false, error }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BridgeErrorCode, CommandId, PlaybackCommand, PlaybackCommandRequest, PlaybackGrant,
        PlaybackLoadRequest, TerminationReason,
    };

    #[test]
    fn playback_grants_are_redacted_from_debug_output() {
        let grant = PlaybackGrant::new("super-secret-playback-grant");
        let output = format!("{grant:?}");

        assert_eq!(output, "PlaybackGrant([redacted])");
        assert!(!output.contains("super-secret"));
    }

    #[test]
    fn load_request_rejects_unrecognized_fields() {
        let request = serde_json::from_str::<PlaybackLoadRequest>(
            r#"{
                "mediaUrl": "https://heya.example.com/api/stream/1",
                "playbackGrant": "grant",
                "mpvArguments": ["--script=/tmp/not-allowed.lua"]
            }"#,
        );

        assert!(request.is_err());
    }

    #[test]
    fn desired_state_commands_are_flat_and_strict() {
        let request = serde_json::from_str::<PlaybackCommandRequest>(
            r#"{
                "rendererSessionId":"2af4fd7b-fbde-4538-a5d8-313f89c24a61",
                "commandId":"seek-1",
                "type":"seek",
                "position_seconds":42
            }"#,
        );
        assert!(request.is_err());

        let request = serde_json::from_str::<PlaybackCommandRequest>(
            r#"{
                "rendererSessionId":"2af4fd7b-fbde-4538-a5d8-313f89c24a61",
                "commandId":"seek-1",
                "type":"seek",
                "positionSeconds":42
            }"#,
        )
        .unwrap();
        assert_eq!(
            request.command,
            PlaybackCommand::Seek {
                position_seconds: 42.0
            }
        );
    }

    #[test]
    fn invalid_values_and_identifiers_are_rejected() {
        assert_eq!(
            PlaybackCommand::SetVolume { volume: 1.1 }
                .validate()
                .unwrap_err()
                .code,
            BridgeErrorCode::InvalidRequest
        );
        assert!(PlaybackCommand::Seek {
            position_seconds: -1.0
        }
        .validate()
        .is_err());
        assert!(CommandId::parse("spaces are not valid").is_err());
    }

    #[test]
    fn only_natural_end_advances_the_queue() {
        assert!(TerminationReason::Ended.advances_queue());
        for reason in [
            TerminationReason::Stopped,
            TerminationReason::WindowClosed,
            TerminationReason::Disposed,
            TerminationReason::Failed,
            TerminationReason::NativeCrashed,
            TerminationReason::LoggedOut,
            TerminationReason::ServerSwitched,
            TerminationReason::AppQuit,
        ] {
            assert!(!reason.advances_queue());
        }
    }
}
