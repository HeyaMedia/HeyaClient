use super::{
    validate_audio_load, validate_audio_track, AudioCapabilities, AudioCommandRequest,
    AudioEventSink, AudioLoadRequest, AudioOutputMode, AudioPreloadRequest, NativeAudioManager,
    NativeAudioStateEvent, NativeAudioVisualizerEvent, NATIVE_AUDIO_PROTOCOL_VERSION,
};
use crate::{
    native_playback::{
        BridgeError, BridgeErrorCode, BridgeResponse, DisposePlaybackRequest, PageInstanceId,
        TerminationReason, WebPlaybackOwner,
    },
    navigation,
    server_profile::{normalize_origin, same_origin, AppState},
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{borrow::Cow, sync::Arc};
use tauri::{
    http::{header, Method, Request, Response, StatusCode},
    AppHandle, Manager, RunEvent, Runtime, Url,
};

pub const AUDIO_BRIDGE_SCHEME: &str = "heya-native-audio";
pub const AUDIO_BRIDGE_OBJECT_NAME: &str = "__HEYA_NATIVE_AUDIO__";
pub const AUDIO_BRIDGE_READY_EVENT: &str = "heya:native-audio:ready-v1";
pub const AUDIO_BRIDGE_STATE_EVENT: &str = "heya:native-audio:state-v1";
pub const AUDIO_BRIDGE_VISUALIZER_EVENT: &str = "heya:native-audio:visualizer-v1";
const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[cfg(any(target_os = "windows", target_os = "android"))]
const AUDIO_BRIDGE_ENDPOINT: &str = "https://heya-native-audio.localhost";
#[cfg(not(any(target_os = "windows", target_os = "android")))]
const AUDIO_BRIDGE_ENDPOINT: &str = "heya-native-audio://localhost";

pub fn audio_initialization_script() -> String {
    include_str!("bridge.js").replace("__HEYA_AUDIO_ENDPOINT__", AUDIO_BRIDGE_ENDPOINT)
}

pub fn audio_lifecycle_plugin<R: Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("native-audio-lifecycle")
        .on_event(|app, event| {
            let reason = match event {
                RunEvent::Exit | RunEvent::ExitRequested { .. } => Some(TerminationReason::AppQuit),
                RunEvent::WindowEvent { label, event, .. }
                    if label == navigation::MAIN_WINDOW_LABEL
                        && matches!(
                            event,
                            tauri::WindowEvent::CloseRequested { .. }
                                | tauri::WindowEvent::Destroyed
                        ) =>
                {
                    Some(TerminationReason::Disposed)
                }
                _ => None,
            };
            if let (Some(reason), Some(manager)) = (reason, app.try_state::<NativeAudioManager>()) {
                if let Err(error) = manager.dispose_active(reason) {
                    log::warn!(
                        "could not dispose native audio during app lifecycle: {}",
                        error.message
                    );
                }
            }
        })
        .build()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireRequest {
    protocol_version: u16,
    page_instance_id: PageInstanceId,
    payload: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AudioOutputModeRequest {
    mode: AudioOutputMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AudioOutputDeviceRequest {
    device_id: Option<String>,
}

pub fn handle_audio_protocol<R: Runtime>(
    app: &AppHandle<R>,
    webview_label: &str,
    request: Request<Vec<u8>>,
) -> Response<Cow<'static, [u8]>> {
    let origin = match authorize_request(app, webview_label, &request) {
        Ok(origin) => origin,
        Err(error) => {
            return json_response(
                StatusCode::FORBIDDEN,
                None,
                BridgeResponse::<Value>::failure(error),
            )
        }
    };

    if request.method() != Method::POST {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            Some(&origin),
            BridgeResponse::<Value>::failure(BridgeError::invalid_request(
                "native audio requests must use POST",
            )),
        );
    }
    if request.body().len() > MAX_REQUEST_BYTES {
        return json_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            Some(&origin),
            BridgeResponse::<Value>::failure(BridgeError::invalid_request(
                "native audio request is too large",
            )),
        );
    }

    let wire = match serde_json::from_slice::<WireRequest>(request.body()) {
        Ok(wire) => wire,
        Err(_) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                Some(&origin),
                BridgeResponse::<Value>::failure(BridgeError::invalid_request(
                    "native audio request is malformed",
                )),
            )
        }
    };
    if wire.protocol_version != NATIVE_AUDIO_PROTOCOL_VERSION {
        return json_response(
            StatusCode::OK,
            Some(&origin),
            BridgeResponse::<Value>::failure(BridgeError::new(
                BridgeErrorCode::ProtocolMismatch,
                "native audio protocol version is unsupported",
            )),
        );
    }

    let owner = WebPlaybackOwner {
        origin: origin.clone(),
        page_instance_id: wire.page_instance_id,
    };
    if request.uri().path() == "/v1/capabilities" {
        log::info!("native audio bridge activated for {origin}");
    }
    let result = dispatch(app, request.uri().path(), owner, wire.payload);
    match result {
        Ok(value) => json_response(
            StatusCode::OK,
            Some(&origin),
            BridgeResponse::success(value),
        ),
        Err(error) => json_response(
            StatusCode::OK,
            Some(&origin),
            BridgeResponse::<Value>::failure(error),
        ),
    }
}

fn dispatch<R: Runtime>(
    app: &AppHandle<R>,
    path: &str,
    owner: WebPlaybackOwner,
    payload: Value,
) -> Result<Value, BridgeError> {
    let manager = app.state::<NativeAudioManager>();
    let settings = app.state::<AppState>().settings();
    match path {
        "/v1/capabilities" => {
            let mut capabilities = manager.capabilities();
            if settings.bit_perfect_audio_enabled && capabilities.bit_perfect.available {
                capabilities.preferred_output_mode = AudioOutputMode::BitPerfect;
            }
            apply_audio_preference(&mut capabilities, settings.native_audio_enabled);
            encode(capabilities)
        }
        "/v1/output-mode" => {
            let request = serde_json::from_value::<AudioOutputModeRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("audio output mode is malformed"))?;
            let capabilities = manager.capabilities();
            if request.mode == AudioOutputMode::BitPerfect && !capabilities.bit_perfect.available {
                return Err(BridgeError::new(
                    BridgeErrorCode::BackendUnavailable,
                    "bit-perfect output is unavailable on this platform",
                ));
            }
            let mut updated = settings;
            updated.bit_perfect_audio_enabled = request.mode == AudioOutputMode::BitPerfect;
            app.state::<AppState>()
                .save_settings(updated)
                .map_err(|error| BridgeError::new(BridgeErrorCode::InternalError, error))?;
            let mut response = capabilities;
            response.preferred_output_mode = request.mode;
            encode(response)
        }
        "/v1/output-devices" => {
            let snapshot = manager.output_devices(settings.audio_output_device_id.as_deref())?;
            if settings.audio_output_device_id.is_some() && snapshot.follows_system_default {
                let mut updated = settings;
                updated.audio_output_device_id = None;
                app.state::<AppState>()
                    .save_settings(updated)
                    .map_err(|error| BridgeError::new(BridgeErrorCode::InternalError, error))?;
            }
            encode(snapshot)
        }
        "/v1/output-device" => {
            let request = serde_json::from_value::<AudioOutputDeviceRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("audio output device is malformed"))?;
            if let Some(device_id) = request.device_id.as_deref() {
                manager.validate_output_device(device_id)?;
            }
            let mut updated = settings;
            updated.audio_output_device_id = request.device_id;
            app.state::<AppState>()
                .save_settings(updated.clone())
                .map_err(|error| BridgeError::new(BridgeErrorCode::InternalError, error))?;
            encode(manager.output_devices(updated.audio_output_device_id.as_deref())?)
        }
        "/v1/load" => {
            if !settings.native_audio_enabled {
                return Err(BridgeError::new(
                    BridgeErrorCode::BackendUnavailable,
                    "native music playback is disabled in HeyaClient settings",
                ));
            }
            let request = serde_json::from_value::<AudioLoadRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("audio load request is malformed"))?;
            if request.mode == AudioOutputMode::BitPerfect && !settings.bit_perfect_audio_enabled {
                return Err(BridgeError::new(
                    BridgeErrorCode::BackendUnavailable,
                    "bit-perfect music playback is not enabled in HeyaClient settings",
                ));
            }
            let load = validate_audio_load(&owner.origin, request)?;
            let preferred_device_id = match settings.audio_output_device_id.as_deref() {
                Some(device_id) if manager.validate_output_device(device_id).is_ok() => {
                    Some(device_id.to_string())
                }
                Some(_) => {
                    log::warn!(
                        "saved audio output disappeared; falling back to the system default"
                    );
                    let mut updated = settings;
                    updated.audio_output_device_id = None;
                    app.state::<AppState>()
                        .save_settings(updated)
                        .map_err(|error| BridgeError::new(BridgeErrorCode::InternalError, error))?;
                    None
                }
                None => None,
            };
            encode(manager.start(owner, load, preferred_device_id.as_deref())?)
        }
        "/v1/preload" => {
            let request = serde_json::from_value::<AudioPreloadRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("audio preload request is malformed"))?;
            let track = validate_audio_track(&owner.origin, request.track)?;
            encode(manager.preload(
                &owner,
                &request.renderer_session_id,
                request.command_id,
                track,
            )?)
        }
        "/v1/command" => {
            let command = serde_json::from_value::<AudioCommandRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("audio command is malformed"))?;
            let command_name = match &command.command {
                super::AudioCommand::Play => "play",
                super::AudioCommand::Pause => "pause",
                super::AudioCommand::Seek { .. } => "seek",
                super::AudioCommand::SetVolume { .. } => "set_volume",
                super::AudioCommand::SetMuted { .. } => "set_muted",
                super::AudioCommand::UpdateProcessing { .. } => "update_processing",
                super::AudioCommand::Stop => "stop",
            };
            log::info!(
                "native audio bridge command renderer={} command={}",
                command.renderer_session_id.as_str(),
                command_name,
            );
            encode(manager.send_command(&owner, command)?)
        }
        "/v1/dispose" => {
            let dispose = serde_json::from_value::<DisposePlaybackRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("audio dispose request is malformed"))?;
            manager.dispose_owned(
                &owner,
                Some(&dispose.renderer_session_id),
                TerminationReason::Disposed,
            )?;
            Ok(Value::Null)
        }
        "/v1/owner-disappeared" => {
            manager.dispose_owned(&owner, None, TerminationReason::Disposed)?;
            Ok(Value::Null)
        }
        _ => Err(BridgeError::invalid_request(
            "native audio operation is unsupported",
        )),
    }
}

fn apply_audio_preference(capabilities: &mut AudioCapabilities, enabled: bool) {
    if !enabled {
        capabilities.available = false;
        capabilities.gapless = false;
        capabilities.crossfade = false;
        capabilities.replay_gain = false;
        capabilities.equalizer = false;
        capabilities.visualizer = false;
        capabilities.unavailable_reason = Some(BridgeErrorCode::BackendUnavailable);
    }
}

fn encode(value: impl serde::Serialize) -> Result<Value, BridgeError> {
    serde_json::to_value(value).map_err(|_| {
        BridgeError::new(
            BridgeErrorCode::InternalError,
            "could not encode the native audio response",
        )
    })
}

fn authorize_request<R: Runtime>(
    app: &AppHandle<R>,
    webview_label: &str,
    request: &Request<Vec<u8>>,
) -> Result<String, BridgeError> {
    if webview_label != navigation::MAIN_WINDOW_LABEL {
        return Err(origin_not_allowed());
    }
    let window = app
        .get_webview_window(navigation::MAIN_WINDOW_LABEL)
        .ok_or_else(origin_not_allowed)?;
    let window_url = window.url().map_err(|_| origin_not_allowed())?;
    let profile = app
        .state::<AppState>()
        .profile()
        .ok_or_else(origin_not_allowed)?;
    let request_origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(origin_not_allowed)?;
    validate_origin(&profile.origin, &window_url, request_origin)
}

fn validate_origin(
    selected_origin: &str,
    window_url: &Url,
    request_origin: &str,
) -> Result<String, BridgeError> {
    let selected = normalize_origin(selected_origin).map_err(|_| origin_not_allowed())?;
    let request_origin = normalize_origin(request_origin).map_err(|_| origin_not_allowed())?;
    if !same_origin(&selected, window_url) || !same_origin(&selected, &request_origin) {
        return Err(origin_not_allowed());
    }
    Ok(selected.as_str().trim_end_matches('/').to_string())
}

fn origin_not_allowed() -> BridgeError {
    BridgeError::new(
        BridgeErrorCode::OriginNotAllowed,
        "native audio is not available to this page",
    )
}

fn json_response<T: serde::Serialize>(
    status: StatusCode,
    origin: Option<&str>,
    body: BridgeResponse<T>,
) -> Response<Cow<'static, [u8]>> {
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| {
        br#"{"ok":false,"error":{"code":"internal_error","message":"native audio response failed"}}"#.to_vec()
    });
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::VARY, "Origin");
    if let Some(origin) = origin {
        builder = builder.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    builder
        .body(Cow::Owned(bytes))
        .expect("the native audio response headers are valid")
}

pub struct TauriAudioEventSink<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> TauriAudioEventSink<R> {
    pub fn new(app: AppHandle<R>) -> Arc<Self> {
        Arc::new(Self { app })
    }

    fn emit<T: serde::Serialize>(&self, owner: &WebPlaybackOwner, event_name: &str, event: &T) {
        let Some(window) = self.app.get_webview_window(navigation::MAIN_WINDOW_LABEL) else {
            return;
        };
        let Some(profile) = self.app.state::<AppState>().profile() else {
            return;
        };
        let Ok(window_url) = window.url() else {
            return;
        };
        if validate_origin(&profile.origin, &window_url, &owner.origin).is_err() {
            return;
        }
        let detail = json!({
            "pageInstanceId": owner.page_instance_id.as_str(),
            "event": event,
        });
        let Ok(detail) = serde_json::to_string(&detail) else {
            return;
        };
        let event_name = serde_json::to_string(event_name).expect("static event name is valid");
        if let Err(error) = window.eval(format!(
            "window.dispatchEvent(new CustomEvent({event_name}, {{ detail: {detail} }}));"
        )) {
            log::warn!("could not publish native audio event: {error}");
        }
    }
}

impl<R: Runtime> AudioEventSink for TauriAudioEventSink<R> {
    fn state(&self, owner: &WebPlaybackOwner, event: &NativeAudioStateEvent) {
        self.emit(owner, AUDIO_BRIDGE_STATE_EVENT, event);
    }

    fn visualizer(&self, owner: &WebPlaybackOwner, event: &NativeAudioVisualizerEvent) {
        self.emit(owner, AUDIO_BRIDGE_VISUALIZER_EVENT, event);
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_audio_preference, validate_origin};
    use crate::native_audio::{AudioCapabilities, BitPerfectCapabilities};
    use crate::native_playback::{BridgeErrorCode, NATIVE_PLAYBACK_PROTOCOL_VERSION};
    use tauri::Url;

    fn capabilities() -> AudioCapabilities {
        AudioCapabilities {
            protocol_version: NATIVE_PLAYBACK_PROTOCOL_VERSION,
            backend: "test",
            available: true,
            gapless: true,
            crossfade: true,
            replay_gain: true,
            equalizer: true,
            visualizer: true,
            output_device_selection: false,
            preferred_output_mode: crate::native_audio::AudioOutputMode::Processed,
            bit_perfect: BitPerfectCapabilities {
                available: false,
                requires_exclusive_device: true,
                unavailable_reason: Some("test"),
            },
            unavailable_reason: None,
        }
    }

    #[test]
    fn validates_the_selected_origin() {
        let selected = "https://heya.example.com";
        assert!(validate_origin(
            selected,
            &Url::parse("https://heya.example.com/music").unwrap(),
            selected,
        )
        .is_ok());
        assert!(validate_origin(
            selected,
            &Url::parse("https://heya.example.com/music").unwrap(),
            "https://evil.example",
        )
        .is_err());
    }

    #[test]
    fn local_preference_disables_native_audio() {
        let mut value = capabilities();
        apply_audio_preference(&mut value, false);
        assert!(!value.available);
        assert_eq!(
            value.unavailable_reason,
            Some(BridgeErrorCode::BackendUnavailable)
        );
    }
}
