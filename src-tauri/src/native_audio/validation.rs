use super::{AudioLoadRequest, AudioTrackLoadRequest};
use crate::native_audio::core::types::{AudioSource, PlaybackGrant, TrackMeta};
use crate::native_playback::{validate_load, BridgeError};

const MAX_ALBUM_KEY_BYTES: usize = 256;
const MAX_MIXRAMP_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct ValidatedAudioTrack {
    pub source: AudioSource,
    pub meta: TrackMeta,
    pub start_position_seconds: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct ValidatedAudioLoad {
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
    for ramp in [&request.start_ramp, &request.end_ramp]
        .into_iter()
        .flatten()
    {
        if ramp.len() > MAX_MIXRAMP_BYTES {
            return Err(BridgeError::invalid_request("MixRamp metadata is too long"));
        }
    }
    let duration_ms = (request.duration_seconds * 1000.0).round() as u64;
    let maximum_boundary_ms = duration_ms.saturating_add(60_000);
    if [
        request.intro_end_ms,
        request.outro_start_ms,
        request.fade_start_ms,
        request.silence_start_ms,
    ]
    .into_iter()
    .flatten()
    .any(|boundary| boundary > maximum_boundary_ms)
    {
        return Err(BridgeError::invalid_request(
            "crossfade boundary is outside the track",
        ));
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
            duration_ms,
            parent_key: request.album_key,
            gain_db: request.gain_db,
            skip_crossfade: request.skip_crossfade,
            start_ramp: request.start_ramp,
            end_ramp: request.end_ramp,
            intro_end_ms: request.intro_end_ms,
            outro_start_ms: request.outro_start_ms,
            fade_start_ms: request.fade_start_ms,
            silence_start_ms: request.silence_start_ms,
        },
        start_position_seconds: media.start_position_seconds(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_audio::AudioProcessingSettings;
    use crate::native_playback::{PlaybackGrant as BridgeGrant, PlaybackLoadRequest};

    fn request() -> AudioLoadRequest {
        AudioLoadRequest {
            processing: AudioProcessingSettings::default(),
            track: AudioTrackLoadRequest {
                track_id: 42,
                duration_seconds: 180.0,
                album_key: "album:7".into(),
                format_hint: Some("flac".into()),
                gain_db: Some(-4.0),
                skip_crossfade: false,
                start_ramp: Some("-30 0;-17 0.5;-3 1.5".into()),
                end_ramp: Some("-30 0;-17 1;-3 2".into()),
                intro_end_ms: Some(1_500),
                outro_start_ms: Some(170_000),
                fade_start_ms: Some(175_000),
                silence_start_ms: Some(179_500),
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
        assert!(validated.track.meta.start_ramp.is_some());
        assert!(validated.track.meta.end_ramp.is_some());
        assert_eq!(validated.track.meta.fade_start_ms, Some(175_000));
    }

    #[test]
    fn rejects_cross_origin_track() {
        let mut request = request();
        request.track.media.media_url =
            "https://evil.example/api/playback/native/media/file".into();
        assert!(validate_audio_load("https://heya.example", request).is_err());
    }
}
