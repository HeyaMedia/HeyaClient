use super::validation::validate_load;
use super::{
    BridgeError, BridgeErrorCode, BridgeResponse, DisposePlaybackRequest, EngineMedia,
    NativeDiagnosticsEvent, NativePlaybackManager, NativeStateEvent, PageInstanceId,
    PlaybackCapabilities, PlaybackCommandRequest, PlaybackEventSink, PlaybackLoadRequest,
    PlaybackLoadResult, PlaybackOwner, TerminationReason, WebPlaybackOwner,
    NATIVE_PLAYBACK_PROTOCOL_VERSION,
};
use crate::{
    navigation,
    server_profile::{normalize_origin, same_origin, AppState},
};
use serde::Deserialize;
use serde_json::{json, Value};
#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{borrow::Cow, sync::Arc};
use tauri::{
    http::{header, Method, Request, Response, StatusCode},
    AppHandle, Manager, RunEvent, Runtime, Url,
};

pub const BRIDGE_SCHEME: &str = "heya-native-playback";
pub const BRIDGE_OBJECT_NAME: &str = "__HEYA_NATIVE_PLAYBACK__";
pub const BRIDGE_READY_EVENT: &str = "heya:native-playback:ready-v1";
pub const BRIDGE_STATE_EVENT: &str = "heya:native-playback:state-v1";
pub const BRIDGE_DIAGNOSTICS_EVENT: &str = "heya:native-playback:diagnostics-v1";
const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[cfg(any(target_os = "windows", target_os = "android"))]
const BRIDGE_ENDPOINT: &str = "https://heya-native-playback.localhost";
#[cfg(not(any(target_os = "windows", target_os = "android")))]
const BRIDGE_ENDPOINT: &str = "heya-native-playback://localhost";

pub fn initialization_script() -> String {
    include_str!("bridge.js").replace("__HEYA_PLAYBACK_ENDPOINT__", BRIDGE_ENDPOINT)
}

pub fn lifecycle_plugin<R: Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("native-playback-lifecycle")
        .on_event(|app, event| {
            #[cfg(all(feature = "native-mpv", target_os = "macos"))]
            if let RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::Resized(size),
                ..
            } = event
            {
                if label == navigation::MAIN_WINDOW_LABEL {
                    super::surface_macos::resize_active_surface(size.width, size.height);
                }
            }

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
            if let (Some(reason), Some(manager)) =
                (reason, app.try_state::<NativePlaybackManager>())
            {
                if let Err(error) = manager.dispose_active(reason) {
                    log::warn!(
                        "could not dispose native playback during app lifecycle: {}",
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

pub fn handle_protocol<R: Runtime>(
    app: &AppHandle<R>,
    webview_label: &str,
    request: Request<Vec<u8>>,
) -> Response<Cow<'static, [u8]>> {
    let authorization = authorize_request(app, webview_label, &request);
    let origin = match authorization {
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
                "native playback requests must use POST",
            )),
        );
    }
    if request.body().len() > MAX_REQUEST_BYTES {
        return json_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            Some(&origin),
            BridgeResponse::<Value>::failure(BridgeError::invalid_request(
                "native playback request is too large",
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
                    "native playback request is malformed",
                )),
            )
        }
    };
    if wire.protocol_version != NATIVE_PLAYBACK_PROTOCOL_VERSION {
        return json_response(
            StatusCode::OK,
            Some(&origin),
            BridgeResponse::<Value>::failure(BridgeError::new(
                BridgeErrorCode::ProtocolMismatch,
                "native playback protocol version is unsupported",
            )),
        );
    }

    let owner = WebPlaybackOwner {
        origin: origin.clone(),
        page_instance_id: wire.page_instance_id,
    };
    if request.uri().path() == "/v1/capabilities" {
        log::info!("native playback bridge activated for {origin}");
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
    let manager = app.state::<NativePlaybackManager>();
    match path {
        "/v1/capabilities" => {
            let enabled = app.state::<AppState>().settings().native_playback_enabled;
            encode(apply_playback_preference(manager.capabilities(), enabled))
        }
        "/v1/load" => {
            if !app.state::<AppState>().settings().native_playback_enabled {
                return Err(BridgeError::new(
                    BridgeErrorCode::BackendUnavailable,
                    "native playback is disabled in HeyaClient settings",
                ));
            }
            let request = serde_json::from_value::<PlaybackLoadRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("playback load request is malformed"))?;
            let load = validate_load(&owner.origin, request)
                .map_err(|error| BridgeError::invalid_request(error.to_string()))?;
            let started =
                manager.start(PlaybackOwner::Web(owner), EngineMedia::Production(load))?;
            encode(PlaybackLoadResult {
                renderer_session_id: started.renderer_session_id,
                video_surface: started.video_surface,
            })
        }
        "/v1/command" => {
            let command = serde_json::from_value::<PlaybackCommandRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("playback command is malformed"))?;
            encode(manager.send_command(&owner, command)?)
        }
        "/v1/dispose" => {
            let dispose = serde_json::from_value::<DisposePlaybackRequest>(payload)
                .map_err(|_| BridgeError::invalid_request("dispose request is malformed"))?;
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
            "native playback operation is unsupported",
        )),
    }
}

fn apply_playback_preference(
    mut capabilities: PlaybackCapabilities,
    enabled: bool,
) -> PlaybackCapabilities {
    if !enabled {
        capabilities.available = false;
        capabilities.diagnostics = false;
        capabilities.audio_track_selection = false;
        capabilities.subtitle_track_selection = false;
        capabilities.quality_selection = false;
        capabilities.unavailable_reason = Some(BridgeErrorCode::BackendUnavailable);
    }
    capabilities
}

fn encode(value: impl serde::Serialize) -> Result<Value, BridgeError> {
    serde_json::to_value(value).map_err(|_| {
        BridgeError::new(
            BridgeErrorCode::InternalError,
            "could not encode the native playback response",
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

    validate_origin(
        navigation::MAIN_WINDOW_LABEL,
        &profile.origin,
        &window_url,
        request_origin,
    )
}

fn validate_origin(
    webview_label: &str,
    selected_origin: &str,
    window_url: &Url,
    request_origin: &str,
) -> Result<String, BridgeError> {
    if webview_label != navigation::MAIN_WINDOW_LABEL {
        return Err(origin_not_allowed());
    }
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
        "native playback is not available to this page",
    )
}

fn json_response<T: serde::Serialize>(
    status: StatusCode,
    origin: Option<&str>,
    body: BridgeResponse<T>,
) -> Response<Cow<'static, [u8]>> {
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| {
        br#"{"ok":false,"error":{"code":"internal_error","message":"native playback response failed"}}"#.to_vec()
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
        .expect("the native playback response headers are valid")
}

pub struct TauriPlaybackEventSink<R: Runtime> {
    app: AppHandle<R>,
    #[cfg(debug_assertions)]
    harness_diagnostics_logged: AtomicBool,
    #[cfg(debug_assertions)]
    harness_fullscreen: AtomicBool,
}

impl<R: Runtime> TauriPlaybackEventSink<R> {
    pub fn new(app: AppHandle<R>) -> Arc<Self> {
        Arc::new(Self {
            app,
            #[cfg(debug_assertions)]
            harness_diagnostics_logged: AtomicBool::new(false),
            #[cfg(debug_assertions)]
            harness_fullscreen: AtomicBool::new(false),
        })
    }

    fn emit<T: serde::Serialize>(&self, owner: &PlaybackOwner, event_name: &str, event: &T) {
        let owner = match owner {
            PlaybackOwner::Web(owner) => owner,
            #[cfg(debug_assertions)]
            PlaybackOwner::NativeDevelopmentHarness => return,
        };
        let Some(window) = self.app.get_webview_window(navigation::MAIN_WINDOW_LABEL) else {
            return;
        };
        let Some(profile) = self.app.state::<AppState>().profile() else {
            return;
        };
        let Ok(window_url) = window.url() else {
            return;
        };
        if validate_origin(
            navigation::MAIN_WINDOW_LABEL,
            &profile.origin,
            &window_url,
            &owner.origin,
        )
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
            log::warn!("could not publish native playback event: {error}");
        }
    }
}

impl<R: Runtime> PlaybackEventSink for TauriPlaybackEventSink<R> {
    fn state(&self, owner: &PlaybackOwner, event: &NativeStateEvent) {
        #[cfg(debug_assertions)]
        if matches!(owner, PlaybackOwner::NativeDevelopmentHarness) {
            let previous = self
                .harness_fullscreen
                .swap(event.payload.fullscreen, Ordering::AcqRel);
            if previous != event.payload.fullscreen {
                log::info!(
                    "native MPV development fullscreen={}",
                    event.payload.fullscreen
                );
            }
            if let Some(reason) = event.payload.termination_reason {
                log::info!("native MPV development renderer terminated: {reason:?}");
            }
            return;
        }
        self.emit(owner, BRIDGE_STATE_EVENT, event);
    }

    fn diagnostics(&self, owner: &PlaybackOwner, event: &NativeDiagnosticsEvent) {
        #[cfg(debug_assertions)]
        if matches!(owner, PlaybackOwner::NativeDevelopmentHarness) {
            let video = event
                .payload
                .as_ref()
                .and_then(|value| value.video.as_ref());
            let decoded = video.and_then(|value| value.decoded.as_ref());
            if decoded
                .and_then(|value| value.hardware_decoder.as_ref())
                .is_some()
                && !self.harness_diagnostics_logged.swap(true, Ordering::AcqRel)
            {
                let source = video.and_then(|value| value.source.as_ref());
                log::info!(
                    "native MPV diagnostics: codec={:?} size={:?}x{:?} pixel_format={:?} hwdec={:?} interop={:?}",
                    source.and_then(|value| value.codec.as_deref()),
                    source.and_then(|value| value.width),
                    source.and_then(|value| value.height),
                    decoded.and_then(|value| value.pixel_format.as_deref()),
                    decoded.and_then(|value| value.hardware_decoder.as_deref()),
                    decoded.and_then(|value| value.hardware_interop.as_deref()),
                );
            }
            return;
        }
        self.emit(owner, BRIDGE_DIAGNOSTICS_EVENT, event);
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_playback_preference, validate_origin};
    use crate::native_playback::{BridgeErrorCode, NativeVideoSurface, PlaybackCapabilities};
    use tauri::Url;

    #[test]
    fn validates_selected_origin_for_window_and_request_on_every_operation() {
        assert_eq!(
            validate_origin(
                "main",
                "https://heya.example.com",
                &Url::parse("https://heya.example.com/movies/42").unwrap(),
                "https://heya.example.com",
            )
            .unwrap(),
            "https://heya.example.com"
        );

        for (window, request_origin) in [
            ("https://evil.example/movies/42", "https://heya.example.com"),
            ("https://heya.example.com/movies/42", "https://evil.example"),
            ("https://heya.example.com/movies/42", "null"),
        ] {
            assert_eq!(
                validate_origin(
                    "main",
                    "https://heya.example.com",
                    &Url::parse(window).unwrap(),
                    request_origin,
                )
                .unwrap_err()
                .code,
                BridgeErrorCode::OriginNotAllowed
            );
        }
        assert_eq!(
            validate_origin(
                "settings",
                "https://heya.example.com",
                &Url::parse("https://heya.example.com/").unwrap(),
                "https://heya.example.com",
            )
            .unwrap_err()
            .code,
            BridgeErrorCode::OriginNotAllowed
        );
    }

    #[test]
    fn local_preference_disables_every_native_playback_capability() {
        let capabilities = PlaybackCapabilities::mpv(true, NativeVideoSurface::NativeSurface, None);
        let disabled = apply_playback_preference(capabilities, false);

        assert!(!disabled.available);
        assert!(!disabled.diagnostics);
        assert!(!disabled.audio_track_selection);
        assert!(!disabled.subtitle_track_selection);
        assert!(!disabled.quality_selection);
        assert_eq!(
            disabled.unavailable_reason,
            Some(BridgeErrorCode::BackendUnavailable)
        );
    }
}
