use super::{
    SystemMediaArtwork, SystemMediaArtworkMime, SystemMediaCapabilities, SystemMediaCommand,
    SystemMediaCommandEvent, SystemMediaPlatform, SystemMediaPlaybackState, SystemMediaSnapshot,
    TrackChangedNotificationRequest, TrackChangedNotificationResult, MAX_ARTWORK_BYTES,
    MAX_ITEM_KEY_BYTES, MAX_SECONDARY_TEXT_BYTES, MAX_TITLE_BYTES, SYSTEM_MEDIA_PROTOCOL_VERSION,
};
use crate::{
    native_playback::{BridgeError, BridgeErrorCode, WebPlaybackOwner},
    navigation,
    server_profile::{normalize_origin, same_origin, AppState},
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::json;
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{AppHandle, Manager, WebviewWindow};
use tauri_plugin_notification::NotificationExt;

pub const SYSTEM_MEDIA_COMMAND_EVENT: &str = "heya:system-media:command-v1";
pub const PLAY_PAUSE_MENU_ID: &str = "system-media-play-pause";
pub const PREVIOUS_MENU_ID: &str = "system-media-previous";
pub const NEXT_MENU_ID: &str = "system-media-next";

pub fn menu_command(menu_id: &str) -> Option<SystemMediaCommand> {
    match menu_id {
        PLAY_PAUSE_MENU_ID => Some(SystemMediaCommand::TogglePlayPause),
        PREVIOUS_MENU_ID => Some(SystemMediaCommand::Previous),
        NEXT_MENU_ID => Some(SystemMediaCommand::Next),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedArtwork {
    cache_key: String,
    path: PathBuf,
    url: String,
}

struct SystemMediaState {
    controls: Option<MediaControls>,
    commands_attached: bool,
    owner: Option<WebPlaybackOwner>,
    last_revision: u64,
    last_snapshot: Option<SystemMediaSnapshot>,
    last_notified_item_key: Option<String>,
    artwork: Option<CachedArtwork>,
    next_command_sequence: u64,
}

#[derive(Clone)]
pub struct SystemMediaManager {
    app: AppHandle,
    cache_dir: Arc<PathBuf>,
    state: Arc<Mutex<SystemMediaState>>,
}

impl SystemMediaManager {
    pub fn new(app: AppHandle, window: &WebviewWindow, cache_dir: PathBuf) -> Self {
        let state = Arc::new(Mutex::new(SystemMediaState {
            controls: None,
            commands_attached: false,
            owner: None,
            last_revision: 0,
            last_snapshot: None,
            last_notified_item_key: None,
            artwork: None,
            next_command_sequence: 1,
        }));

        let mut controls = match MediaControls::new(platform_config(window)) {
            Ok(controls) => Some(controls),
            Err(error) => {
                log::warn!("could not initialize OS media controls: {error}");
                None
            }
        };
        let commands_attached = controls.as_mut().is_some_and(|controls| {
            let event_app = app.clone();
            let event_state = state.clone();
            controls
                .attach(move |event| dispatch_platform_event(&event_app, &event_state, event))
                .map(|_| true)
                .unwrap_or_else(|error| {
                    log::warn!("could not attach OS media command handlers: {error}");
                    false
                })
        });
        if let Ok(mut shared) = state.lock() {
            shared.controls = controls;
            shared.commands_attached = commands_attached;
        }

        Self {
            app,
            cache_dir: Arc::new(cache_dir.join("system-media-artwork")),
            state,
        }
    }

    pub fn capabilities(&self, notifications_enabled: bool) -> SystemMediaCapabilities {
        let state = self.state.lock().ok();
        let now_playing = state.as_ref().is_some_and(|state| state.controls.is_some());
        let media_commands = state.as_ref().is_some_and(|state| state.commands_attached);
        SystemMediaCapabilities {
            protocol_version: SYSTEM_MEDIA_PROTOCOL_VERSION,
            available: now_playing,
            platform: current_platform(),
            now_playing,
            media_commands,
            track_notifications: true,
            track_notifications_enabled: notifications_enabled,
            artwork: now_playing,
            unavailable_reason: (!now_playing)
                .then(|| "OS media controls could not be initialized".to_string()),
        }
    }

    pub fn update(
        &self,
        owner: WebPlaybackOwner,
        mut snapshot: SystemMediaSnapshot,
    ) -> Result<(), BridgeError> {
        validate_snapshot(&snapshot)?;
        let mut state = self.lock_state()?;
        if state.owner.as_ref() != Some(&owner) {
            reset_owner_state(&mut state, owner);
        }
        if snapshot.revision <= state.last_revision {
            return Ok(());
        }

        let same_item = state
            .last_snapshot
            .as_ref()
            .is_some_and(|previous| previous.item_key == snapshot.item_key);
        if !same_item {
            state.last_notified_item_key = None;
        }
        let previous_artwork = state.artwork.clone();
        let next_artwork = match snapshot.artwork.take() {
            Some(ref artwork)
                if same_item
                    && previous_artwork
                        .as_ref()
                        .is_some_and(|cached| cached.cache_key == artwork.cache_key) =>
            {
                previous_artwork.clone()
            }
            Some(ref artwork) => Some(cache_artwork(
                self.cache_dir.as_ref(),
                artwork,
                snapshot.revision,
            )?),
            None if same_item => previous_artwork.clone(),
            None => None,
        };

        let metadata_changed = state
            .last_snapshot
            .as_ref()
            .is_none_or(|previous| metadata_changed(previous, &snapshot))
            || previous_artwork.as_ref().map(|art| &art.cache_key)
                != next_artwork.as_ref().map(|art| &art.cache_key);

        if let Some(controls) = state.controls.as_mut() {
            if metadata_changed {
                controls
                    .set_metadata(MediaMetadata {
                        title: Some(&snapshot.title),
                        artist: snapshot.artist.as_deref(),
                        album: snapshot.album.as_deref(),
                        cover_url: next_artwork.as_ref().map(|art| art.url.as_str()),
                        duration: (snapshot.duration_seconds > 0.0)
                            .then(|| Duration::from_secs_f64(snapshot.duration_seconds)),
                    })
                    .map_err(|error| platform_error("update OS now-playing metadata", error))?;
            }
            controls
                .set_playback(playback_for_snapshot(&snapshot))
                .map_err(|error| platform_error("update OS playback state", error))?;
        }

        state.last_revision = snapshot.revision;
        state.last_snapshot = Some(snapshot);
        state.artwork = next_artwork.clone();
        drop(state);

        if let Some(previous) = previous_artwork {
            if next_artwork.as_ref().map(|art| &art.path) != Some(&previous.path) {
                remove_cached_artwork(&previous.path);
            }
        }
        Ok(())
    }

    pub fn clear(&self, owner: &WebPlaybackOwner, revision: u64) -> Result<(), BridgeError> {
        if revision == 0 {
            return Err(BridgeError::invalid_request(
                "system media revision must be positive",
            ));
        }
        let mut state = self.lock_state()?;
        ensure_owner(&state, owner)?;
        if revision <= state.last_revision {
            return Ok(());
        }
        let previous_artwork = state.artwork.take();
        if let Some(controls) = state.controls.as_mut() {
            controls
                .set_playback(MediaPlayback::Stopped)
                .and_then(|_| controls.set_metadata(MediaMetadata::default()))
                .map_err(|error| platform_error("clear OS now-playing state", error))?;
        }
        state.last_revision = revision;
        state.last_snapshot = None;
        state.last_notified_item_key = None;
        drop(state);
        if let Some(artwork) = previous_artwork {
            remove_cached_artwork(&artwork.path);
        }
        Ok(())
    }

    pub fn clear_owned(&self, owner: &WebPlaybackOwner) -> Result<(), BridgeError> {
        let mut state = self.lock_state()?;
        if state.owner.as_ref() != Some(owner) {
            return Ok(());
        }
        let previous_artwork = state.artwork.take();
        if let Some(controls) = state.controls.as_mut() {
            if let Err(error) = controls
                .set_playback(MediaPlayback::Stopped)
                .and_then(|_| controls.set_metadata(MediaMetadata::default()))
            {
                log::warn!("could not clear OS media state for a departing page: {error}");
            }
        }
        state.owner = None;
        state.last_revision = 0;
        state.last_snapshot = None;
        state.last_notified_item_key = None;
        drop(state);
        if let Some(artwork) = previous_artwork {
            remove_cached_artwork(&artwork.path);
        }
        Ok(())
    }

    pub fn clear_all(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let previous_artwork = state.artwork.take();
        if let Some(controls) = state.controls.as_mut() {
            if let Err(error) = controls
                .set_playback(MediaPlayback::Stopped)
                .and_then(|_| controls.set_metadata(MediaMetadata::default()))
            {
                log::warn!("could not clear OS media state: {error}");
            }
        }
        state.owner = None;
        state.last_revision = 0;
        state.last_snapshot = None;
        state.last_notified_item_key = None;
        drop(state);
        if let Some(artwork) = previous_artwork {
            remove_cached_artwork(&artwork.path);
        }
    }

    pub fn notify_track_changed(
        &self,
        owner: &WebPlaybackOwner,
        request: TrackChangedNotificationRequest,
        enabled: bool,
    ) -> Result<TrackChangedNotificationResult, BridgeError> {
        validate_item_key(&request.item_key)?;
        let mut state = self.lock_state()?;
        ensure_owner(&state, owner)?;
        let snapshot = state.last_snapshot.as_ref().ok_or_else(|| {
            BridgeError::new(
                BridgeErrorCode::UnknownSession,
                "there is no current media item",
            )
        })?;
        if request.revision == 0
            || request.revision > state.last_revision
            || request.item_key != snapshot.item_key
        {
            return Err(BridgeError::invalid_request(
                "track notification does not match the current media item",
            ));
        }
        if state.last_notified_item_key.as_deref() == Some(request.item_key.as_str()) {
            return Ok(TrackChangedNotificationResult { shown: false });
        }
        let should_show = enabled && snapshot.playback_state == SystemMediaPlaybackState::Playing;
        let title = snapshot.title.clone();
        let body = notification_body(snapshot);
        state.last_notified_item_key = Some(request.item_key);
        drop(state);

        let window = navigation::main_window(&self.app)
            .map_err(|error| BridgeError::new(BridgeErrorCode::InternalError, error))?;
        if !should_show || window.is_focused().unwrap_or(true) {
            return Ok(TrackChangedNotificationResult { shown: false });
        }

        match self
            .app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
        {
            Ok(()) => Ok(TrackChangedNotificationResult { shown: true }),
            Err(error) => {
                log::warn!("could not show native track notification: {error}");
                Ok(TrackChangedNotificationResult { shown: false })
            }
        }
    }

    pub fn dispatch_menu_command(&self, command: SystemMediaCommand) {
        dispatch_command(&self.app, &self.state, command);
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, SystemMediaState>, BridgeError> {
        self.state.lock().map_err(|_| {
            BridgeError::new(
                BridgeErrorCode::InternalError,
                "system media state lock was poisoned",
            )
        })
    }
}

fn reset_owner_state(state: &mut SystemMediaState, owner: WebPlaybackOwner) {
    if let Some(previous) = state.artwork.take() {
        remove_cached_artwork(&previous.path);
    }
    state.owner = Some(owner);
    state.last_revision = 0;
    state.last_snapshot = None;
    state.last_notified_item_key = None;
    state.next_command_sequence = 1;
}

fn ensure_owner(state: &SystemMediaState, owner: &WebPlaybackOwner) -> Result<(), BridgeError> {
    if state.owner.as_ref() == Some(owner) {
        Ok(())
    } else {
        Err(BridgeError::new(
            BridgeErrorCode::UnknownSession,
            "the system media owner is no longer active",
        ))
    }
}

fn playback_for_snapshot(snapshot: &SystemMediaSnapshot) -> MediaPlayback {
    let progress = Some(MediaPosition(Duration::from_secs_f64(
        snapshot.position_seconds,
    )));
    match snapshot.playback_state {
        SystemMediaPlaybackState::Playing => MediaPlayback::Playing { progress },
        SystemMediaPlaybackState::Paused => MediaPlayback::Paused { progress },
    }
}

fn metadata_changed(previous: &SystemMediaSnapshot, next: &SystemMediaSnapshot) -> bool {
    previous.item_key != next.item_key
        || previous.title != next.title
        || previous.artist != next.artist
        || previous.album != next.album
        || previous.duration_seconds != next.duration_seconds
}

fn cache_artwork(
    cache_dir: &Path,
    artwork: &SystemMediaArtwork,
    revision: u64,
) -> Result<CachedArtwork, BridgeError> {
    validate_cache_key(&artwork.cache_key)?;
    let maximum_base64_bytes = MAX_ARTWORK_BYTES.saturating_mul(4) / 3 + 8;
    if artwork.base64_data.len() > maximum_base64_bytes {
        return Err(BridgeError::invalid_request(
            "system media artwork is too large",
        ));
    }
    let bytes = BASE64_STANDARD
        .decode(&artwork.base64_data)
        .map_err(|_| BridgeError::invalid_request("system media artwork is not valid base64"))?;
    if bytes.is_empty()
        || bytes.len() > MAX_ARTWORK_BYTES
        || !valid_image_magic(&bytes, artwork.mime_type)
    {
        return Err(BridgeError::invalid_request(
            "system media artwork does not match its declared image type",
        ));
    }
    fs::create_dir_all(cache_dir).map_err(|error| {
        BridgeError::new(
            BridgeErrorCode::InternalError,
            format!("could not create the system media artwork cache: {error}"),
        )
    })?;
    let path = cache_dir.join(format!(
        "now-playing-{revision}.{}",
        artwork.mime_type.extension()
    ));
    fs::write(&path, bytes).map_err(|error| {
        BridgeError::new(
            BridgeErrorCode::InternalError,
            format!("could not cache system media artwork: {error}"),
        )
    })?;
    let url = artwork_file_url(&path)?;
    Ok(CachedArtwork {
        cache_key: artwork.cache_key.clone(),
        path,
        url,
    })
}

fn artwork_file_url(path: &Path) -> Result<String, BridgeError> {
    #[cfg(target_os = "windows")]
    {
        Ok(format!("file://{}", path.to_string_lossy()))
    }
    #[cfg(not(target_os = "windows"))]
    {
        tauri::Url::from_file_path(path)
            .map(|url| url.to_string())
            .map_err(|_| {
                BridgeError::new(
                    BridgeErrorCode::InternalError,
                    "could not construct a local artwork URL",
                )
            })
    }
}

fn valid_image_magic(bytes: &[u8], mime: SystemMediaArtworkMime) -> bool {
    match mime {
        SystemMediaArtworkMime::Jpeg => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        SystemMediaArtworkMime::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
    }
}

fn remove_cached_artwork(path: &Path) {
    if let Err(error) = fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!("could not remove stale system media artwork: {error}");
        }
    }
}

fn validate_snapshot(snapshot: &SystemMediaSnapshot) -> Result<(), BridgeError> {
    if snapshot.revision == 0 {
        return Err(BridgeError::invalid_request(
            "system media revision must be positive",
        ));
    }
    validate_item_key(&snapshot.item_key)?;
    validate_text("title", &snapshot.title, MAX_TITLE_BYTES, false)?;
    if let Some(artist) = snapshot.artist.as_deref() {
        validate_text("artist", artist, MAX_SECONDARY_TEXT_BYTES, true)?;
    }
    if let Some(album) = snapshot.album.as_deref() {
        validate_text("album", album, MAX_SECONDARY_TEXT_BYTES, true)?;
    }
    if !snapshot.duration_seconds.is_finite()
        || !snapshot.position_seconds.is_finite()
        || snapshot.duration_seconds < 0.0
        || snapshot.position_seconds < 0.0
        || (snapshot.duration_seconds > 0.0
            && snapshot.position_seconds > snapshot.duration_seconds + 1.0)
        || snapshot.duration_seconds > 7.0 * 24.0 * 60.0 * 60.0
    {
        return Err(BridgeError::invalid_request(
            "system media duration or position is outside the allowed range",
        ));
    }
    if snapshot.duration_seconds == 0.0 && snapshot.position_seconds != 0.0 {
        return Err(BridgeError::invalid_request(
            "system media position requires a finite duration",
        ));
    }
    if let Some(artwork) = snapshot.artwork.as_ref() {
        validate_cache_key(&artwork.cache_key)?;
        if artwork.base64_data.len() > MAX_ARTWORK_BYTES.saturating_mul(4) / 3 + 8 {
            return Err(BridgeError::invalid_request(
                "system media artwork is too large",
            ));
        }
    }
    Ok(())
}

fn validate_item_key(value: &str) -> Result<(), BridgeError> {
    validate_text("item key", value, MAX_ITEM_KEY_BYTES, false)
}

fn validate_cache_key(value: &str) -> Result<(), BridgeError> {
    validate_text("artwork cache key", value, MAX_ITEM_KEY_BYTES, false)
}

fn validate_text(
    field: &str,
    value: &str,
    maximum_bytes: usize,
    allow_empty: bool,
) -> Result<(), BridgeError> {
    if (!allow_empty && value.trim().is_empty())
        || value.len() > maximum_bytes
        || value.chars().any(char::is_control)
    {
        return Err(BridgeError::invalid_request(format!(
            "system media {field} is invalid"
        )));
    }
    Ok(())
}

fn notification_body(snapshot: &SystemMediaSnapshot) -> String {
    match (
        snapshot.artist.as_deref().filter(|value| !value.is_empty()),
        snapshot.album.as_deref().filter(|value| !value.is_empty()),
    ) {
        (Some(artist), Some(album)) => format!("{artist} · {album}"),
        (Some(artist), None) => artist.to_string(),
        (None, Some(album)) => album.to_string(),
        (None, None) => "Now playing in Heya".to_string(),
    }
}

fn platform_error(action: &str, error: impl std::fmt::Display) -> BridgeError {
    BridgeError::new(
        BridgeErrorCode::InternalError,
        format!("could not {action}: {error}"),
    )
}

fn dispatch_platform_event(
    app: &AppHandle,
    state: &Arc<Mutex<SystemMediaState>>,
    event: MediaControlEvent,
) {
    match event {
        MediaControlEvent::Play => dispatch_command(app, state, SystemMediaCommand::Play),
        MediaControlEvent::Pause => dispatch_command(app, state, SystemMediaCommand::Pause),
        MediaControlEvent::Toggle => {
            dispatch_command(app, state, SystemMediaCommand::TogglePlayPause)
        }
        MediaControlEvent::Next => dispatch_command(app, state, SystemMediaCommand::Next),
        MediaControlEvent::Previous => dispatch_command(app, state, SystemMediaCommand::Previous),
        MediaControlEvent::Stop => dispatch_command(app, state, SystemMediaCommand::Stop),
        MediaControlEvent::Seek(direction) => dispatch_command(
            app,
            state,
            SystemMediaCommand::SeekBy {
                offset_seconds: signed_seek(direction, 10.0),
            },
        ),
        MediaControlEvent::SeekBy(direction, duration) => dispatch_command(
            app,
            state,
            SystemMediaCommand::SeekBy {
                offset_seconds: signed_seek(direction, duration.as_secs_f64()),
            },
        ),
        MediaControlEvent::SetPosition(position) => dispatch_command(
            app,
            state,
            SystemMediaCommand::SeekTo {
                position_seconds: position.0.as_secs_f64(),
            },
        ),
        MediaControlEvent::Raise => {
            if let Ok(window) = navigation::main_window(app) {
                let _ = window.show().and_then(|_| window.set_focus());
            }
        }
        // OS volume remains the system mixer's responsibility. Arbitrary URI,
        // quit, and volume requests never cross into the remote Heya page.
        MediaControlEvent::SetVolume(_)
        | MediaControlEvent::OpenUri(_)
        | MediaControlEvent::Quit => {}
    }
}

fn signed_seek(direction: SeekDirection, seconds: f64) -> f64 {
    match direction {
        SeekDirection::Forward => seconds,
        SeekDirection::Backward => -seconds,
    }
}

fn dispatch_command(
    app: &AppHandle,
    state: &Arc<Mutex<SystemMediaState>>,
    command: SystemMediaCommand,
) {
    let (owner, command_sequence) = {
        let Ok(mut state) = state.lock() else {
            return;
        };
        let Some(owner) = state.owner.clone() else {
            return;
        };
        let sequence = state.next_command_sequence;
        state.next_command_sequence = sequence.saturating_add(1);
        (owner, sequence)
    };
    if !owner_is_current(app, &owner) {
        return;
    }
    let event = SystemMediaCommandEvent {
        command_sequence,
        command,
    };
    let detail = json!({
        "pageInstanceId": owner.page_instance_id.as_str(),
        "command": event,
    });
    let Ok(detail) = serde_json::to_string(&detail) else {
        return;
    };
    let event_name = serde_json::to_string(SYSTEM_MEDIA_COMMAND_EVENT)
        .expect("static system media event name is valid");
    if let Ok(window) = navigation::main_window(app) {
        if let Err(error) = window.eval(format!(
            "window.dispatchEvent(new CustomEvent({event_name}, {{ detail: {detail} }}));"
        )) {
            log::warn!("could not publish OS media command: {error}");
        }
    }
}

fn owner_is_current(app: &AppHandle, owner: &WebPlaybackOwner) -> bool {
    let Some(profile) = app.state::<AppState>().profile() else {
        return false;
    };
    let Ok(selected) = normalize_origin(&profile.origin) else {
        return false;
    };
    let Ok(owner_origin) = normalize_origin(&owner.origin) else {
        return false;
    };
    let Ok(window) = navigation::main_window(app) else {
        return false;
    };
    let Ok(window_url) = window.url() else {
        return false;
    };
    same_origin(&selected, &owner_origin) && same_origin(&selected, &window_url)
}

fn current_platform() -> SystemMediaPlatform {
    #[cfg(target_os = "macos")]
    return SystemMediaPlatform::Macos;
    #[cfg(target_os = "windows")]
    return SystemMediaPlatform::Windows;
    #[cfg(target_os = "linux")]
    return SystemMediaPlatform::Linux;
    #[allow(unreachable_code)]
    SystemMediaPlatform::Unsupported
}

fn platform_config(window: &WebviewWindow) -> PlatformConfig<'static> {
    #[cfg(target_os = "windows")]
    let hwnd = window.hwnd().ok().map(|handle| handle.0);
    #[cfg(not(target_os = "windows"))]
    let hwnd = {
        let _ = window;
        None
    };
    PlatformConfig {
        display_name: "Heya",
        dbus_name: "media.heya.client",
        hwnd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> SystemMediaSnapshot {
        SystemMediaSnapshot {
            revision: 1,
            item_key: "track:42:deadbeef".into(),
            title: "A song".into(),
            artist: Some("An artist".into()),
            album: Some("An album".into()),
            duration_seconds: 180.0,
            position_seconds: 12.0,
            playback_state: SystemMediaPlaybackState::Playing,
            can_go_previous: true,
            can_go_next: true,
            can_seek: true,
            artwork: None,
        }
    }

    #[test]
    fn validates_normalized_snapshots_and_rejects_spoofing_shapes() {
        assert!(validate_snapshot(&snapshot()).is_ok());
        let mut invalid = snapshot();
        invalid.title = "bad\nnotification".into();
        assert_eq!(
            validate_snapshot(&invalid).unwrap_err().code,
            BridgeErrorCode::InvalidRequest
        );
        let mut invalid = snapshot();
        invalid.position_seconds = 999.0;
        assert!(validate_snapshot(&invalid).is_err());
    }

    #[test]
    fn accepts_only_declared_png_or_jpeg_bytes() {
        assert!(valid_image_magic(
            &[0xff, 0xd8, 0xff, 0x00],
            SystemMediaArtworkMime::Jpeg
        ));
        assert!(!valid_image_magic(
            b"not an image",
            SystemMediaArtworkMime::Png
        ));
    }

    #[test]
    fn derives_notification_copy_only_from_the_current_snapshot() {
        assert_eq!(notification_body(&snapshot()), "An artist · An album");
    }
}
