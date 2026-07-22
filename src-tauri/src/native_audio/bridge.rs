use super::{
    validate_audio_load, validate_audio_track, AudioCapabilities, AudioCommandRequest,
    AudioEventSink, AudioLoadRequest, AudioPreloadRequest, NativeAudioManager,
    NativeAudioStateEvent, NativeAudioVisualizerEvent, NATIVE_AUDIO_PROTOCOL_VERSION,
};
use crate::{
    native_playback::{
        BridgeError, BridgeErrorCode, BridgeResponse, DisposePlaybackRequest, TerminationReason,
        WebPlaybackOwner,
    },
    navigation,
    server_profile::{normalize_origin, same_origin, AppState},
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tauri::{AppHandle, Manager, RunEvent, Runtime, Url, WebviewWindow};

pub const AUDIO_BRIDGE_OBJECT_NAME: &str = "__HEYA_NATIVE_AUDIO__";
pub const AUDIO_BRIDGE_READY_EVENT: &str = "heya:native-audio:ready-v2";
pub const AUDIO_BRIDGE_STATE_EVENT: &str = "heya:native-audio:state-v2";
pub const AUDIO_BRIDGE_VISUALIZER_EVENT: &str = "heya:native-audio:visualizer-v2";
pub fn audio_initialization_script() -> String {
    include_str!("bridge.js").replace(
        "__HEYA_NATIVE_AUDIO_COMMAND__",
        crate::native_bridge::AUDIO_COMMAND,
    )
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
struct AudioOutputDeviceRequest {
    device_id: Option<String>,
}

pub(crate) fn handle_audio_ipc<R: Runtime>(
    app: &AppHandle<R>,
    webview: &WebviewWindow<R>,
    request: crate::native_bridge::NativeBridgeRequest,
) -> BridgeResponse<Value> {
    let origin = match authorize_webview(app, webview) {
        Ok(origin) => origin,
        Err(error) => return BridgeResponse::failure(error),
    };
    if let Err(error) = request.ensure_size("audio") {
        return BridgeResponse::failure(error);
    }
    if request.protocol_version != NATIVE_AUDIO_PROTOCOL_VERSION {
        return BridgeResponse::failure(BridgeError::new(
            BridgeErrorCode::ProtocolMismatch,
            "native audio protocol version is unsupported",
        ));
    }

    let owner = WebPlaybackOwner {
        origin: origin.clone(),
        page_instance_id: request.page_instance_id,
    };
    let path = operation_path(&request.operation);
    if path == Some("/v2/capabilities") {
        log::info!("native audio bridge activated for {origin}");
    }
    let result = path
        .ok_or_else(|| BridgeError::invalid_request("native audio operation is unsupported"))
        .and_then(|path| dispatch(app, path, owner, request.payload));
    match result {
        Ok(value) => BridgeResponse::success(value),
        Err(error) => BridgeResponse::failure(error),
    }
}

fn operation_path(operation: &str) -> Option<&'static str> {
    match operation {
        "capabilities" => Some("/v2/capabilities"),
        "output-devices" => Some("/v2/output-devices"),
        "output-device" => Some("/v2/output-device"),
        "load" => Some("/v2/load"),
        "state" => Some("/v2/state"),
        "preload" => Some("/v2/preload"),
        "command" => Some("/v2/command"),
        "dispose" => Some("/v2/dispose"),
        "owner-disappeared" => Some("/v2/owner-disappeared"),
        _ => None,
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
        "/v2/capabilities" => {
            let mut capabilities = manager.capabilities();
            apply_audio_preference(&mut capabilities, settings.native_audio_enabled);
            encode(capabilities)
        }
        "/v2/output-devices" => {
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
        "/v2/output-device" => {
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
        "/v2/load" => {
            if !settings.native_audio_enabled {
                return Err(BridgeError::new(
                    BridgeErrorCode::BackendUnavailable,
                    "native music playback is disabled in HeyaClient settings",
                ));
            }
            let request = serde_json::from_value::<AudioLoadRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("audio load request is malformed"))?;
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
        "/v2/state" => {
            let request = serde_json::from_value::<DisposePlaybackRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("audio state request is malformed"))?;
            encode(manager.state(&owner, &request.renderer_session_id)?)
        }
        "/v2/preload" => {
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
        "/v2/command" => {
            let command = serde_json::from_value::<AudioCommandRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("audio command is malformed"))?;
            let command_name = match &command.command {
                super::AudioCommand::Play => "play",
                super::AudioCommand::Pause => "pause",
                super::AudioCommand::Seek { .. } => "seek",
                super::AudioCommand::SetVolume { .. } => "set_volume",
                super::AudioCommand::SetMuted { .. } => "set_muted",
                super::AudioCommand::UpdateProcessing { .. } => "update_processing",
                super::AudioCommand::UpdateTrackAnalysis { .. } => "update_track_analysis",
                super::AudioCommand::Stop => "stop",
            };
            log::info!(
                "native audio bridge command renderer={} command={}",
                command.renderer_session_id.as_str(),
                command_name,
            );
            encode(manager.send_command(&owner, command)?)
        }
        "/v2/dispose" => {
            let dispose = serde_json::from_value::<DisposePlaybackRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("audio dispose request is malformed"))?;
            manager.dispose_owned(
                &owner,
                Some(&dispose.renderer_session_id),
                TerminationReason::Disposed,
            )?;
            Ok(Value::Null)
        }
        "/v2/owner-disappeared" => {
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

fn authorize_webview<R: Runtime>(
    app: &AppHandle<R>,
    webview: &WebviewWindow<R>,
) -> Result<String, BridgeError> {
    let window_url = webview.url().map_err(|_| origin_not_allowed())?;
    let profile = app
        .state::<AppState>()
        .profile()
        .ok_or_else(origin_not_allowed)?;
    validate_window_origin(webview.label(), &profile.origin, &window_url)
}

fn validate_window_origin(
    webview_label: &str,
    selected_origin: &str,
    window_url: &Url,
) -> Result<String, BridgeError> {
    if webview_label != navigation::MAIN_WINDOW_LABEL {
        return Err(origin_not_allowed());
    }
    let selected = normalize_origin(selected_origin).map_err(|_| origin_not_allowed())?;
    if !same_origin(&selected, window_url) {
        return Err(origin_not_allowed());
    }
    Ok(selected.as_str().trim_end_matches('/').to_string())
}

fn validate_owner_origin(
    webview_label: &str,
    selected_origin: &str,
    window_url: &Url,
    owner_origin: &str,
) -> Result<String, BridgeError> {
    let selected = validate_window_origin(webview_label, selected_origin, window_url)?;
    let owner = normalize_origin(owner_origin).map_err(|_| origin_not_allowed())?;
    let selected_url = normalize_origin(&selected).map_err(|_| origin_not_allowed())?;
    if !same_origin(&selected_url, &owner) {
        return Err(origin_not_allowed());
    }
    Ok(selected)
}

fn origin_not_allowed() -> BridgeError {
    BridgeError::new(
        BridgeErrorCode::OriginNotAllowed,
        "native audio is not available to this page",
    )
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
        if validate_owner_origin(window.label(), &profile.origin, &window_url, &owner.origin)
            .is_err()
        {
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
    use super::{
        apply_audio_preference, operation_path, validate_owner_origin, validate_window_origin,
    };
    use crate::native_audio::AudioCapabilities;
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
            unavailable_reason: None,
        }
    }

    #[test]
    fn validates_the_selected_origin() {
        let selected = "https://heya.example.com";
        assert!(validate_window_origin(
            "main",
            selected,
            &Url::parse("https://heya.example.com/music").unwrap(),
        )
        .is_ok());
        assert!(validate_owner_origin(
            "main",
            selected,
            &Url::parse("https://heya.example.com/music").unwrap(),
            "https://evil.example",
        )
        .is_err());
        assert!(validate_window_origin(
            "settings",
            selected,
            &Url::parse("https://heya.example.com/music").unwrap(),
        )
        .is_err());
    }

    #[test]
    fn maps_only_the_public_audio_operations() {
        assert_eq!(operation_path("capabilities"), Some("/v2/capabilities"));
        assert_eq!(operation_path("output-devices"), Some("/v2/output-devices"));
        assert_eq!(operation_path("state"), Some("/v2/state"));
        assert_eq!(operation_path("command"), Some("/v2/command"));
        assert_eq!(operation_path("shell"), None);
        assert_eq!(operation_path("/v2/command"), None);
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
