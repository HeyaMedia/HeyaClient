use crate::{
    native_playback::{BridgeError, BridgeErrorCode, BridgeResponse},
    navigation,
    server_profile::{normalize_origin, same_origin, AppSettings, AppState, ServerProfile},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager, WebviewWindow};

pub const APPLICATION_PROTOCOL_VERSION: u16 = 1;

pub fn initialization_script() -> String {
    include_str!("application_bridge.js").replace(
        "__HEYA_APPLICATION_COMMAND__",
        crate::native_bridge::APPLICATION_COMMAND,
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationCapabilities {
    protocol_version: u16,
    available: bool,
    platform: &'static str,
    app_version: String,
    updater_supported: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplicationSnapshot {
    capabilities: ApplicationCapabilities,
    profile: Option<ApplicationProfile>,
    settings: ApplicationSettings,
    native_playback: ApplicationNativePlaybackStatus,
    native_audio: ApplicationNativeAudioStatus,
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    update: Option<crate::app_updates::UpdateStatus>,
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    update: Option<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplicationNativePlaybackStatus {
    backend: &'static str,
    available: bool,
    build_includes_native_mpv: bool,
    video_surface: crate::native_playback::NativeVideoSurface,
    unavailable_reason: Option<BridgeErrorCode>,
    installation: crate::native_playback::MpvInstallationOffer,
}

impl From<crate::NativePlaybackStatus> for ApplicationNativePlaybackStatus {
    fn from(status: crate::NativePlaybackStatus) -> Self {
        Self {
            backend: status.backend,
            available: status.available,
            build_includes_native_mpv: status.build_includes_native_mpv,
            video_surface: status.video_surface,
            unavailable_reason: status.unavailable_reason,
            installation: status.installation,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplicationNativeAudioStatus {
    backend: &'static str,
    available: bool,
    gapless: bool,
    crossfade: bool,
}

impl From<crate::NativeAudioStatus> for ApplicationNativeAudioStatus {
    fn from(status: crate::NativeAudioStatus) -> Self {
        Self {
            backend: status.backend,
            available: status.available,
            gapless: status.gapless,
            crossfade: status.crossfade,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApplicationProfile {
    id: String,
    name: String,
    origin: String,
    last_connected_at: Option<chrono::DateTime<chrono::Utc>>,
    server_version: Option<String>,
}

impl From<ServerProfile> for ApplicationProfile {
    fn from(profile: ServerProfile) -> Self {
        Self {
            id: profile.id,
            name: profile.name,
            origin: profile.origin,
            last_connected_at: profile.last_connected_at,
            server_version: profile.server_version,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApplicationSettings {
    reconnect_on_launch: bool,
    native_playback_enabled: bool,
    native_audio_enabled: bool,
    audio_output_device_id: Option<String>,
    track_change_notifications: bool,
}

impl From<AppSettings> for ApplicationSettings {
    fn from(settings: AppSettings) -> Self {
        Self {
            reconnect_on_launch: settings.reconnect_on_launch,
            native_playback_enabled: settings.native_playback_enabled,
            native_audio_enabled: settings.native_audio_enabled,
            audio_output_device_id: settings.audio_output_device_id,
            track_change_notifications: settings.track_change_notifications,
        }
    }
}

impl From<ApplicationSettings> for AppSettings {
    fn from(settings: ApplicationSettings) -> Self {
        Self {
            reconnect_on_launch: settings.reconnect_on_launch,
            native_playback_enabled: settings.native_playback_enabled,
            native_audio_enabled: settings.native_audio_enabled,
            audio_output_device_id: settings.audio_output_device_id,
            track_change_notifications: settings.track_change_notifications,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyRequest {}

pub(crate) async fn handle_application_ipc(
    app: &AppHandle,
    webview: &WebviewWindow,
    request: crate::native_bridge::NativeBridgeRequest,
) -> BridgeResponse<Value> {
    if let Err(error) = authorize_webview(app, webview) {
        return BridgeResponse::failure(error);
    }
    if let Err(error) = request.ensure_size("application") {
        return BridgeResponse::failure(error);
    }
    if request.protocol_version != APPLICATION_PROTOCOL_VERSION {
        return BridgeResponse::failure(BridgeError::new(
            BridgeErrorCode::ProtocolMismatch,
            "application protocol version is unsupported",
        ));
    }

    match dispatch(app, &request.operation, request.payload).await {
        Ok(value) => BridgeResponse::success(value),
        Err(error) => BridgeResponse::failure(error),
    }
}

async fn dispatch(app: &AppHandle, operation: &str, payload: Value) -> Result<Value, BridgeError> {
    match operation {
        "capabilities" => {
            decode_empty(payload)?;
            encode(capabilities(app))
        }
        "snapshot" => {
            decode_empty(payload)?;
            encode(snapshot(app))
        }
        "save-settings" => {
            let settings = serde_json::from_value::<ApplicationSettings>(payload)
                .map_err(|_| BridgeError::invalid_request("application settings are malformed"))?;
            let saved = app
                .state::<AppState>()
                .save_settings(settings.into())
                .map_err(command_failed)?;
            encode(ApplicationSettings::from(saved))
        }
        "check-for-update" => {
            decode_empty(payload)?;
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            {
                let status = app
                    .state::<crate::app_updates::AppUpdater>()
                    .check(app)
                    .await
                    .map_err(command_failed)?;
                encode(status)
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            Err(BridgeError::new(
                BridgeErrorCode::BackendUnavailable,
                "application updates are managed by the app store on this platform",
            ))
        }
        "install-update" => {
            decode_empty(payload)?;
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            {
                app.state::<crate::app_updates::AppUpdater>()
                    .install_silent(app.clone())
                    .await
                    .map_err(command_failed)?;
                Ok(Value::Null)
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            Err(BridgeError::new(
                BridgeErrorCode::BackendUnavailable,
                "application updates are managed by the app store on this platform",
            ))
        }
        "install-native-playback-runtime" => {
            decode_empty(payload)?;
            crate::native_playback::install_mpv_runtime_silent(app.clone())
                .await
                .map_err(command_failed)?;
            let playback = app.state::<crate::native_playback::NativePlaybackManager>();
            let status = crate::native_playback_status(&playback);
            if !status.available {
                return Err(command_failed(
                    "MPV was installed but its native backend could not initialize".into(),
                ));
            }
            encode(ApplicationNativePlaybackStatus::from(status))
        }
        "open-server-picker" => {
            decode_empty(payload)?;
            navigation::navigate_main_to_picker(app).map_err(command_failed)?;
            Ok(Value::Null)
        }
        "reset-server-session" => {
            decode_empty(payload)?;
            crate::reset_server_session_inner(app).map_err(command_failed)?;
            Ok(Value::Null)
        }
        "forget-server" => {
            decode_empty(payload)?;
            crate::forget_server_inner(app).map_err(command_failed)?;
            Ok(Value::Null)
        }
        _ => Err(BridgeError::invalid_request(
            "application operation is unsupported",
        )),
    }
}

fn snapshot(app: &AppHandle) -> ApplicationSnapshot {
    let state = app.state::<AppState>();
    let playback = app.state::<crate::native_playback::NativePlaybackManager>();
    let audio = app.state::<crate::native_audio::NativeAudioManager>();
    ApplicationSnapshot {
        capabilities: capabilities(app),
        profile: state.profile().map(Into::into),
        settings: state.settings().into(),
        native_playback: crate::native_playback_status(&playback).into(),
        native_audio: crate::native_audio_status(&audio).into(),
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        update: Some(app.state::<crate::app_updates::AppUpdater>().status(app)),
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        update: None,
    }
}

fn capabilities(app: &AppHandle) -> ApplicationCapabilities {
    ApplicationCapabilities {
        protocol_version: APPLICATION_PROTOCOL_VERSION,
        available: true,
        platform: std::env::consts::OS,
        app_version: app.package_info().version.to_string(),
        updater_supported: !cfg!(debug_assertions)
            && cfg!(any(
                target_os = "macos",
                target_os = "windows",
                target_os = "linux"
            )),
    }
}

fn authorize_webview(app: &AppHandle, webview: &WebviewWindow) -> Result<(), BridgeError> {
    let window_url = webview.url().map_err(|_| origin_not_allowed())?;
    let profile = app
        .state::<AppState>()
        .profile()
        .ok_or_else(origin_not_allowed)?;
    if webview.label() != navigation::MAIN_WINDOW_LABEL {
        return Err(origin_not_allowed());
    }
    let selected = normalize_origin(&profile.origin).map_err(|_| origin_not_allowed())?;
    if !same_origin(&selected, &window_url) {
        return Err(origin_not_allowed());
    }
    Ok(())
}

fn decode_empty(payload: Value) -> Result<(), BridgeError> {
    serde_json::from_value::<EmptyRequest>(payload)
        .map(|_| ())
        .map_err(|_| BridgeError::invalid_request("application request must be empty"))
}

fn encode(value: impl Serialize) -> Result<Value, BridgeError> {
    serde_json::to_value(value).map_err(|_| {
        BridgeError::new(
            BridgeErrorCode::InternalError,
            "could not encode the application response",
        )
    })
}

fn command_failed(message: String) -> BridgeError {
    BridgeError::new(BridgeErrorCode::CommandFailed, message)
}

fn origin_not_allowed() -> BridgeError {
    BridgeError::new(
        BridgeErrorCode::OriginNotAllowed,
        "application integration is not available to this page",
    )
}
