use crate::native_playback::{
    BridgeError, BridgeErrorCode, CommandId, PlaybackError, PlaybackLoadRequest, RendererSessionId,
    TerminationReason,
};
use serde::{Deserialize, Serialize};

pub const NATIVE_AUDIO_PROTOCOL_VERSION: u16 = 2;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CrossfadeMode {
    Gapless,
    Crossfade,
    Smart,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CrossfeedPreset {
    Subtle,
    Natural,
    Strong,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DspBlockId {
    Equalizer,
    Crossfeed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AudioProcessingSettings {
    pub replay_gain_enabled: bool,
    pub eq_enabled: bool,
    pub eq_bands_db: [f32; 10],
    pub preamp_db: f32,
    pub postgain_db: f32,
    pub limiter_enabled: bool,
    pub crossfeed_enabled: bool,
    pub crossfeed_preset: CrossfeedPreset,
    pub dsp_order: [DspBlockId; 2],
    pub crossfade_mode: CrossfadeMode,
    pub crossfade_seconds: f32,
    pub visualizer_enabled: bool,
}

impl Default for AudioProcessingSettings {
    fn default() -> Self {
        Self {
            replay_gain_enabled: true,
            eq_enabled: false,
            eq_bands_db: [0.0; 10],
            preamp_db: 0.0,
            postgain_db: 0.0,
            limiter_enabled: true,
            crossfeed_enabled: false,
            crossfeed_preset: CrossfeedPreset::Natural,
            dsp_order: [DspBlockId::Equalizer, DspBlockId::Crossfeed],
            crossfade_mode: CrossfadeMode::Gapless,
            crossfade_seconds: 3.0,
            visualizer_enabled: false,
        }
    }
}

impl AudioProcessingSettings {
    pub fn validate(&self) -> Result<(), BridgeError> {
        if self
            .eq_bands_db
            .iter()
            .any(|value| !value.is_finite() || !(-12.0..=12.0).contains(value))
        {
            return Err(BridgeError::invalid_request(
                "equalizer gains must be finite values between -12 and 12 dB",
            ));
        }
        for (name, value) in [("preamp", self.preamp_db), ("postgain", self.postgain_db)] {
            if !value.is_finite() || !(-12.0..=12.0).contains(&value) {
                return Err(BridgeError::invalid_request(format!(
                    "{name} must be a finite value between -12 and 12 dB"
                )));
            }
        }
        if !self.crossfade_seconds.is_finite() || !(0.0..=20.0).contains(&self.crossfade_seconds) {
            return Err(BridgeError::invalid_request(
                "crossfade duration must be between 0 and 20 seconds",
            ));
        }
        if self.dsp_order[0] == self.dsp_order[1] {
            return Err(BridgeError::invalid_request(
                "DSP order must contain equalizer and crossfeed exactly once",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AudioTrackLoadRequest {
    pub track_id: i64,
    pub duration_seconds: f64,
    #[serde(default)]
    pub album_key: String,
    #[serde(default)]
    pub format_hint: Option<String>,
    #[serde(default)]
    pub gain_db: Option<f32>,
    #[serde(default)]
    pub skip_crossfade: bool,
    #[serde(default)]
    pub start_ramp: Option<String>,
    #[serde(default)]
    pub end_ramp: Option<String>,
    #[serde(default)]
    pub intro_end_ms: Option<u64>,
    #[serde(default)]
    pub outro_start_ms: Option<u64>,
    #[serde(default)]
    pub fade_start_ms: Option<u64>,
    #[serde(default)]
    pub silence_start_ms: Option<u64>,
    pub media: PlaybackLoadRequest,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AudioLoadRequest {
    #[serde(default)]
    pub processing: AudioProcessingSettings,
    pub track: AudioTrackLoadRequest,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AudioPreloadRequest {
    pub renderer_session_id: RendererSessionId,
    pub command_id: CommandId,
    pub track: AudioTrackLoadRequest,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum AudioCommand {
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
    UpdateProcessing {
        settings: AudioProcessingSettings,
    },
    UpdateTrackAnalysis {
        #[serde(rename = "trackId")]
        track_id: i64,
        #[serde(default, rename = "gainDb")]
        gain_db: Option<f32>,
        #[serde(default, rename = "introEndMs")]
        intro_end_ms: Option<u64>,
        #[serde(default, rename = "outroStartMs")]
        outro_start_ms: Option<u64>,
        #[serde(default, rename = "fadeStartMs")]
        fade_start_ms: Option<u64>,
        #[serde(default, rename = "silenceStartMs")]
        silence_start_ms: Option<u64>,
    },
    Stop,
}

impl AudioCommand {
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
            Self::UpdateProcessing { settings } => settings.validate(),
            Self::UpdateTrackAnalysis { track_id, .. } if *track_id <= 0 => {
                Err(BridgeError::invalid_request("trackId must be positive"))
            }
            Self::UpdateTrackAnalysis {
                gain_db: Some(gain),
                ..
            } if !gain.is_finite() || !(-60.0..=24.0).contains(gain) => Err(
                BridgeError::invalid_request("normalization gain is invalid"),
            ),
            Self::UpdateTrackAnalysis {
                intro_end_ms,
                outro_start_ms,
                fade_start_ms,
                silence_start_ms,
                ..
            } if [
                *intro_end_ms,
                *outro_start_ms,
                *fade_start_ms,
                *silence_start_ms,
            ]
            .into_iter()
            .flatten()
            .any(|boundary| boundary > 7 * 24 * 60 * 60 * 1_000) =>
            {
                Err(BridgeError::invalid_request(
                    "crossfade boundary is invalid",
                ))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AudioCommandRequest {
    pub renderer_session_id: RendererSessionId,
    pub command_id: CommandId,
    #[serde(flatten)]
    pub command: AudioCommand,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioCapabilities {
    pub protocol_version: u16,
    pub backend: &'static str,
    pub available: bool,
    pub gapless: bool,
    pub crossfade: bool,
    pub replay_gain: bool,
    pub equalizer: bool,
    pub visualizer: bool,
    pub output_device_selection: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<BridgeErrorCode>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeAudioOutputDevice {
    pub device_id: String,
    pub label: String,
    pub is_default: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeAudioOutputDevices {
    pub devices: Vec<NativeAudioOutputDevice>,
    pub active_device_id: Option<String>,
    pub follows_system_default: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioLoadResult {
    pub renderer_session_id: RendererSessionId,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NativeAudioState {
    pub playing: bool,
    pub paused: bool,
    pub loading: bool,
    pub buffering: bool,
    pub ended: bool,
    pub position_seconds: f64,
    pub duration_seconds: f64,
    pub volume: f64,
    pub muted: bool,
    pub current_track_id: Option<i64>,
    pub started_track_id: Option<i64>,
    pub ended_track_id: Option<i64>,
    pub source_sample_rate_hz: Option<u32>,
    pub source_channels: Option<u16>,
    pub output_sample_rate_hz: Option<u32>,
    pub output_channels: Option<u16>,
    pub output_device_id: Option<String>,
    pub output_device_name: Option<String>,
    pub resampler_active: bool,
    pub dsp_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PlaybackError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<TerminationReason>,
}

impl Default for NativeAudioState {
    fn default() -> Self {
        Self {
            playing: false,
            paused: true,
            loading: false,
            buffering: false,
            ended: false,
            position_seconds: 0.0,
            duration_seconds: 0.0,
            volume: 1.0,
            muted: false,
            current_track_id: None,
            started_track_id: None,
            ended_track_id: None,
            source_sample_rate_hz: None,
            source_channels: None,
            output_sample_rate_hz: None,
            output_channels: None,
            output_device_id: None,
            output_device_name: None,
            resampler_active: false,
            dsp_active: false,
            error: None,
            termination_reason: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NativeAudioStateEvent {
    pub protocol_version: u16,
    pub renderer_session_id: RendererSessionId,
    pub state_revision: u64,
    pub payload: NativeAudioState,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NativeAudioVisualizerEvent {
    pub protocol_version: u16,
    pub renderer_session_id: RendererSessionId,
    pub visualizer_revision: u64,
    pub samples: Vec<f32>,
    pub frequency_bins: Vec<f32>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioCommandResult {
    pub renderer_session_id: RendererSessionId,
    pub command_id: CommandId,
    pub command_sequence: u64,
    pub accepted: bool,
    pub duplicate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<PlaybackError>,
}

#[cfg(test)]
mod tests {
    use super::{
        AudioCommand, AudioCommandRequest, NativeAudioOutputDevice, NativeAudioOutputDevices,
    };
    use serde_json::json;

    fn envelope(command: serde_json::Value) -> serde_json::Value {
        let mut value = json!({
            "rendererSessionId": "renderer-session",
            "commandId": "command-id"
        });
        value.as_object_mut().unwrap().extend(
            command
                .as_object()
                .expect("the test command is an object")
                .clone(),
        );
        value
    }

    #[test]
    fn decodes_the_flat_javascript_audio_command_envelope() {
        let request = serde_json::from_value::<AudioCommandRequest>(envelope(json!({
            "type": "setVolume",
            "volume": 0.8
        })))
        .expect("the browser's flat command payload must decode");

        assert_eq!(request.command, AudioCommand::SetVolume { volume: 0.8 });

        let seek = serde_json::from_value::<AudioCommandRequest>(envelope(json!({
            "type": "seek",
            "positionSeconds": 148.05
        })))
        .expect("the browser's camel-case seek payload must decode");

        assert_eq!(
            seek.command,
            AudioCommand::Seek {
                position_seconds: 148.05
            }
        );

        let gain = serde_json::from_value::<AudioCommandRequest>(envelope(json!({
            "type": "updateTrackAnalysis",
            "trackId": 42,
            "gainDb": -5.25,
            "fadeStartMs": 195000,
            "silenceStartMs": 199000
        })))
        .expect("a live ReplayGain update must decode");
        assert_eq!(
            gain.command,
            AudioCommand::UpdateTrackAnalysis {
                track_id: 42,
                gain_db: Some(-5.25),
                intro_end_ms: None,
                outro_start_ms: None,
                fade_start_ms: Some(195_000),
                silence_start_ms: Some(199_000),
            }
        );
        assert!(gain.command.validate().is_ok());
    }

    #[test]
    fn flat_audio_command_still_rejects_unknown_fields() {
        let result = serde_json::from_value::<AudioCommandRequest>(envelope(json!({
            "type": "setMuted",
            "muted": false,
            "arbitrary": "not allowed"
        })));

        assert!(result.is_err());
    }

    #[test]
    fn serializes_normalized_output_devices_for_javascript() {
        let value = serde_json::to_value(NativeAudioOutputDevices {
            devices: vec![NativeAudioOutputDevice {
                device_id: "stable-device-id".into(),
                label: "Studio speakers".into(),
                is_default: true,
            }],
            active_device_id: Some("stable-device-id".into()),
            follows_system_default: false,
        })
        .unwrap();

        assert_eq!(
            value,
            json!({
                "devices": [{
                    "deviceId": "stable-device-id",
                    "label": "Studio speakers",
                    "isDefault": true
                }],
                "activeDeviceId": "stable-device-id",
                "followsSystemDefault": false
            })
        );
    }
}
