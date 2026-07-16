use super::{PlaybackGrant, PlaybackLoadRequest};
use crate::server_profile::{normalize_origin, same_origin};
use std::{error::Error, fmt};
use tauri::Url;

const MAX_MEDIA_URL_BYTES: usize = 16 * 1024;
const PLAYBACK_GRANT_BYTES: usize = 64;
const NATIVE_MEDIA_PATH_PREFIX: &str = "/api/playback/native/media/";
const MAX_START_POSITION_SECONDS: f64 = 366.0 * 24.0 * 60.0 * 60.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaybackValidationError {
    InvalidServerOrigin,
    MediaUrlEmpty,
    MediaUrlTooLong,
    InvalidMediaUrl,
    UnsupportedMediaUrlScheme,
    MediaUrlHasCredentials,
    MediaUrlHasFragment,
    MediaUrlOriginMismatch,
    MediaUrlPathNotAllowed,
    PlaybackGrantEmpty,
    PlaybackGrantInvalidLength,
    PlaybackGrantHasUnsafeCharacters,
    InvalidStartPosition,
}

impl fmt::Display for PlaybackValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidServerOrigin => "the selected Heya server origin is invalid",
            Self::MediaUrlEmpty => "the media URL is empty",
            Self::MediaUrlTooLong => "the media URL is too long",
            Self::InvalidMediaUrl => "the media URL is invalid",
            Self::UnsupportedMediaUrlScheme => "the media URL must use HTTP or HTTPS",
            Self::MediaUrlHasCredentials => "the media URL must not contain credentials",
            Self::MediaUrlHasFragment => "the media URL must not contain a fragment",
            Self::MediaUrlOriginMismatch => {
                "the media URL does not belong to the selected Heya server"
            }
            Self::MediaUrlPathNotAllowed => "the media URL is not a Heya native playback endpoint",
            Self::PlaybackGrantEmpty => "the playback grant is empty",
            Self::PlaybackGrantInvalidLength => "the playback grant has an invalid length",
            Self::PlaybackGrantHasUnsafeCharacters => {
                "the playback grant contains characters that are unsafe in a header value"
            }
            Self::InvalidStartPosition => "the playback start position is invalid",
        };
        formatter.write_str(message)
    }
}

impl Error for PlaybackValidationError {}

/// A load request that is safe to pass to a native renderer adapter.
///
/// Heya's allowlisted native media routes do not redirect. This prevents an
/// unrelated same-origin endpoint from receiving the fixed grant header; URL
/// redirects must remain forbidden by the corresponding server contract.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedPlaybackLoad {
    media_url: Url,
    playback_grant: PlaybackGrant,
    start_position_seconds: Option<f64>,
}

impl ValidatedPlaybackLoad {
    pub fn media_url(&self) -> &Url {
        &self.media_url
    }

    /// Returns the value for HeyaClient's fixed playback-authorization header.
    ///
    /// Renderer adapters must never log, persist, or expose this value, and
    /// must not attach it after a cross-origin redirect.
    pub fn playback_grant_header_value(&self) -> &str {
        self.playback_grant.as_str()
    }

    pub fn start_position_seconds(&self) -> Option<f64> {
        self.start_position_seconds
    }
}

pub(crate) fn validate_load(
    server_origin: &str,
    request: PlaybackLoadRequest,
) -> Result<ValidatedPlaybackLoad, PlaybackValidationError> {
    let selected_origin = normalize_origin(server_origin)
        .map_err(|_| PlaybackValidationError::InvalidServerOrigin)?;

    if request.media_url.is_empty() {
        return Err(PlaybackValidationError::MediaUrlEmpty);
    }
    if request.media_url.len() > MAX_MEDIA_URL_BYTES {
        return Err(PlaybackValidationError::MediaUrlTooLong);
    }

    let media_url =
        Url::parse(&request.media_url).map_err(|_| PlaybackValidationError::InvalidMediaUrl)?;
    if !matches!(media_url.scheme(), "http" | "https") {
        return Err(PlaybackValidationError::UnsupportedMediaUrlScheme);
    }
    if media_url.host_str().is_none() {
        return Err(PlaybackValidationError::InvalidMediaUrl);
    }
    if !media_url.username().is_empty() || media_url.password().is_some() {
        return Err(PlaybackValidationError::MediaUrlHasCredentials);
    }
    if media_url.fragment().is_some() {
        return Err(PlaybackValidationError::MediaUrlHasFragment);
    }
    if !same_origin(&selected_origin, &media_url) {
        return Err(PlaybackValidationError::MediaUrlOriginMismatch);
    }
    if !media_url.path().starts_with(NATIVE_MEDIA_PATH_PREFIX)
        || media_url.path().len() == NATIVE_MEDIA_PATH_PREFIX.len()
    {
        return Err(PlaybackValidationError::MediaUrlPathNotAllowed);
    }

    validate_grant(request.playback_grant.as_str())?;

    if request.start_position_seconds.is_some_and(|position| {
        !position.is_finite() || !(0.0..=MAX_START_POSITION_SECONDS).contains(&position)
    }) {
        return Err(PlaybackValidationError::InvalidStartPosition);
    }

    Ok(ValidatedPlaybackLoad {
        media_url,
        playback_grant: request.playback_grant,
        start_position_seconds: request.start_position_seconds,
    })
}

fn validate_grant(grant: &str) -> Result<(), PlaybackValidationError> {
    if grant.is_empty() {
        return Err(PlaybackValidationError::PlaybackGrantEmpty);
    }
    if !grant.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(PlaybackValidationError::PlaybackGrantHasUnsafeCharacters);
    }
    if grant.len() != PLAYBACK_GRANT_BYTES {
        return Err(PlaybackValidationError::PlaybackGrantInvalidLength);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_load, PlaybackValidationError};
    use crate::native_playback::{PlaybackGrant, PlaybackLoadRequest};

    fn request(media_url: &str) -> PlaybackLoadRequest {
        PlaybackLoadRequest {
            media_url: media_url.to_string(),
            playback_grant: PlaybackGrant::new("a".repeat(64)),
            start_position_seconds: Some(42.5),
        }
    }

    #[test]
    fn accepts_same_origin_media_urls_and_preserves_query_parameters() {
        let load = validate_load(
            "https://heya.example.com/",
            request("https://heya.example.com/api/playback/native/media/42/hls/master.m3u8?quality=original"),
        )
        .unwrap();

        assert_eq!(
            load.media_url().as_str(),
            "https://heya.example.com/api/playback/native/media/42/hls/master.m3u8?quality=original"
        );
        assert_eq!(load.start_position_seconds(), Some(42.5));
        assert_eq!(
            load.playback_grant_header_value(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn rejects_cross_origin_media_urls() {
        let error = validate_load(
            "https://heya.example.com/",
            request("https://cdn.heya.example.com/api/playback/native/media/42"),
        )
        .unwrap_err();

        assert_eq!(error, PlaybackValidationError::MediaUrlOriginMismatch);
    }

    #[test]
    fn rejects_local_paths_credentials_and_fragments() {
        assert_eq!(
            validate_load(
                "https://heya.example.com/",
                request("file:///api/playback/native/media/movie.mkv")
            )
            .unwrap_err(),
            PlaybackValidationError::UnsupportedMediaUrlScheme
        );
        assert_eq!(
            validate_load(
                "https://heya.example.com/",
                request("https://user:secret@heya.example.com/api/playback/native/media/video")
            )
            .unwrap_err(),
            PlaybackValidationError::MediaUrlHasCredentials
        );
        assert_eq!(
            validate_load(
                "https://heya.example.com/",
                request("https://heya.example.com/api/playback/native/media/video#fragment")
            )
            .unwrap_err(),
            PlaybackValidationError::MediaUrlHasFragment
        );
    }

    #[test]
    fn rejects_non_native_same_origin_paths() {
        assert_eq!(
            validate_load(
                "https://heya.example.com/",
                request("https://heya.example.com/api/stream/42")
            )
            .unwrap_err(),
            PlaybackValidationError::MediaUrlPathNotAllowed
        );
    }

    #[test]
    fn rejects_header_injection_in_playback_grants() {
        let mut request = request("https://heya.example.com/api/playback/native/media/video");
        request.playback_grant = PlaybackGrant::new("valid\r\nX-Evil: injected");

        assert_eq!(
            validate_load("https://heya.example.com/", request).unwrap_err(),
            PlaybackValidationError::PlaybackGrantHasUnsafeCharacters
        );
    }

    #[test]
    fn rejects_playback_grants_outside_the_v1_format() {
        let mut request = request("https://heya.example.com/api/playback/native/media/video");
        request.playback_grant = PlaybackGrant::new("abcd");

        assert_eq!(
            validate_load("https://heya.example.com/", request).unwrap_err(),
            PlaybackValidationError::PlaybackGrantInvalidLength
        );
    }

    #[test]
    fn rejects_non_finite_or_negative_start_positions() {
        for position in [f64::NAN, f64::INFINITY, -0.1] {
            let mut request = request("https://heya.example.com/api/playback/native/media/video");
            request.start_position_seconds = Some(position);

            assert_eq!(
                validate_load("https://heya.example.com/", request).unwrap_err(),
                PlaybackValidationError::InvalidStartPosition
            );
        }
    }
}
