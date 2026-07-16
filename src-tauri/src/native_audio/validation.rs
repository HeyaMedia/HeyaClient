use super::{AudioLoadRequest, AudioTrackLoadRequest};
use crate::native_audio::core::types::{AudioSource, PlaybackGrant, TrackMeta};
use crate::native_playback::{validate_load, BridgeError};

const MAX_ALBUM_KEY_BYTES: usize = 256;

#[derive(Clone, Debug)]
pub struct ValidatedAudioTrack {
    pub source: AudioSource,
    pub meta: TrackMeta,
    pub start_position_seconds: Option<f64>,
    pub codec: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub bit_depth: Option<u16>,
    pub channels: Option<u16>,
    pub lossless: bool,
}

#[derive(Clone, Debug)]
pub struct ValidatedAudioLoad {
    pub mode: super::AudioOutputMode,
    pub processing: super::AudioProcessingSettings,
    pub track: ValidatedAudioTrack,
}

pub fn validate_audio_load(
    server_origin: &str,
    request: AudioLoadRequest,
) -> Result<ValidatedAudioLoad, BridgeError> {
    request.processing.validate()?;
    let track = validate_audio_track(server_origin, request.track)?;
    Ok(ValidatedAudioLoad {
        mode: request.mode,
        processing: request.processing,
        track,
    })
}

pub fn validate_audio_track(
    server_origin: &str,
    request: AudioTrackLoadRequest,
) -> Result<ValidatedAudioTrack, BridgeError> {
    if request.track_id <= 0 {
        return Err(BridgeError::invalid_request("trackId must be positive"));
    }
    if !request.duration_seconds.is_finite()
        || !(0.0..=7.0 * 24.0 * 60.0 * 60.0).contains(&request.duration_seconds)
    {
        return Err(BridgeError::invalid_request(
            "track duration must be finite and no longer than seven days",
        ));
    }
    if request.album_key.len() > MAX_ALBUM_KEY_BYTES {
        return Err(BridgeError::invalid_request("album key is too long"));
    }
    if let Some(gain) = request.gain_db {
        if !gain.is_finite() || !(-60.0..=24.0).contains(&gain) {
            return Err(BridgeError::invalid_request(
                "normalization gain is invalid",
            ));
        }
    }
    if request
        .format_hint
        .as_deref()
        .is_some_and(|value| value.len() > 16 || !value.bytes().all(|b| b.is_ascii_alphanumeric()))
    {
        return Err(BridgeError::invalid_request("audio format hint is invalid"));
    }
    let media = validate_load(server_origin, request.media)
        .map_err(|error| BridgeError::invalid_request(error.to_string()))?;
    let source = AudioSource {
        media_url: media.media_url().to_string(),
        playback_grant: PlaybackGrant::new(media.playback_grant_header_value()),
        format_hint: request.format_hint.clone(),
    };
    Ok(ValidatedAudioTrack {
        source,
        meta: TrackMeta {
            rating_key: request.track_id,
            duration_ms: (request.duration_seconds * 1000.0).round() as u64,
            parent_key: request.album_key,
            gain_db: request.gain_db,
            skip_crossfade: false,
            start_ramp: None,
            end_ramp: None,
        },
        start_position_seconds: media.start_position_seconds(),
        codec: request.codec,
        sample_rate_hz: request.sample_rate_hz,
        bit_depth: request.bit_depth,
        channels: request.channels,
        lossless: request.lossless,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_audio::{AudioOutputMode, AudioProcessingSettings};
    use crate::native_playback::{PlaybackGrant as BridgeGrant, PlaybackLoadRequest};

    fn request() -> AudioLoadRequest {
        AudioLoadRequest {
            mode: AudioOutputMode::Processed,
            processing: AudioProcessingSettings::default(),
            track: AudioTrackLoadRequest {
                track_id: 42,
                duration_seconds: 180.0,
                album_key: "album:7".into(),
                format_hint: Some("flac".into()),
                codec: Some("flac".into()),
                sample_rate_hz: Some(96_000),
                bit_depth: Some(24),
                channels: Some(2),
                lossless: true,
                gain_db: Some(-4.0),
                media: PlaybackLoadRequest {
                    media_url: "https://heya.example/api/playback/native/media/file".into(),
                    playback_grant: BridgeGrant::new("a".repeat(64)),
                    start_position_seconds: None,
                },
            },
        }
    }

    #[test]
    fn accepts_scoped_same_origin_track() {
        let validated = validate_audio_load("https://heya.example", request()).unwrap();
        assert_eq!(validated.track.meta.rating_key, 42);
        assert_eq!(validated.track.source.format_hint.as_deref(), Some("flac"));
    }

    #[test]
    fn rejects_cross_origin_track() {
        let mut request = request();
        request.track.media.media_url =
            "https://evil.example/api/playback/native/media/file".into();
        assert!(validate_audio_load("https://heya.example", request).is_err());
    }
}
