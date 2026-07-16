use super::{
    ClearSystemMediaRequest, SystemMediaManager, SystemMediaSnapshot,
    TrackChangedNotificationRequest, MAX_SYSTEM_MEDIA_REQUEST_BYTES, SYSTEM_MEDIA_PROTOCOL_VERSION,
};
use crate::{
    native_playback::{BridgeError, BridgeErrorCode, BridgeResponse, WebPlaybackOwner},
    navigation,
    server_profile::{normalize_origin, same_origin, AppState},
};
use serde::Deserialize;
use serde_json::Value;
use tauri::{AppHandle, Manager, RunEvent, Runtime, WebviewWindow};

pub const SYSTEM_MEDIA_BRIDGE_OBJECT_NAME: &str = "__HEYA_SYSTEM_MEDIA__";
pub const SYSTEM_MEDIA_BRIDGE_READY_EVENT: &str = "heya:system-media:ready-v1";

pub fn system_media_initialization_script() -> String {
    include_str!("bridge.js").replace(
        "__HEYA_SYSTEM_MEDIA_COMMAND__",
        crate::native_bridge::SYSTEM_MEDIA_COMMAND,
    )
}

pub fn system_media_lifecycle_plugin<R: Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("system-media-lifecycle")
        .on_event(|app, event| {
            let should_clear = matches!(event, RunEvent::Exit | RunEvent::ExitRequested { .. })
                || matches!(
                    event,
                    RunEvent::WindowEvent { label, event, .. }
                        if label == navigation::MAIN_WINDOW_LABEL
                            && matches!(
                                event,
                                tauri::WindowEvent::CloseRequested { .. }
                                    | tauri::WindowEvent::Destroyed
                            )
                );
            if should_clear {
                if let Some(manager) = app.try_state::<SystemMediaManager>() {
                    manager.clear_all();
                }
            }
        })
        .build()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyRequest {}

pub(crate) fn handle_system_media_ipc<R: Runtime>(
    app: &AppHandle<R>,
    webview: &WebviewWindow<R>,
    request: crate::native_bridge::NativeBridgeRequest,
) -> BridgeResponse<Value> {
    let origin = match authorize_webview(app, webview) {
        Ok(origin) => origin,
        Err(error) => return BridgeResponse::failure(error),
    };
    if let Err(error) =
        request.ensure_size_with_limit("system media", MAX_SYSTEM_MEDIA_REQUEST_BYTES)
    {
        return BridgeResponse::failure(error);
    }
    if request.protocol_version != SYSTEM_MEDIA_PROTOCOL_VERSION {
        return BridgeResponse::failure(BridgeError::new(
            BridgeErrorCode::ProtocolMismatch,
            "system media protocol version is unsupported",
        ));
    }

    let owner = WebPlaybackOwner {
        origin,
        page_instance_id: request.page_instance_id,
    };
    let result = dispatch(app, &request.operation, owner, request.payload);
    match result {
        Ok(value) => BridgeResponse::success(value),
        Err(error) => BridgeResponse::failure(error),
    }
}

fn dispatch<R: Runtime>(
    app: &AppHandle<R>,
    operation: &str,
    owner: WebPlaybackOwner,
    payload: Value,
) -> Result<Value, BridgeError> {
    let manager = app.state::<SystemMediaManager>();
    match operation {
        "capabilities" => {
            decode_empty(payload)?;
            encode(
                manager.capabilities(
                    app.state::<AppState>()
                        .settings()
                        .track_change_notifications,
                ),
            )
        }
        "update" => {
            let snapshot = serde_json::from_value::<SystemMediaSnapshot>(payload)
                .map_err(|_| BridgeError::invalid_request("system media snapshot is malformed"))?;
            manager.update(owner, snapshot)?;
            Ok(Value::Null)
        }
        "clear" => {
            let clear =
                serde_json::from_value::<ClearSystemMediaRequest>(payload).map_err(|_| {
                    BridgeError::invalid_request("system media clear request is malformed")
                })?;
            manager.clear(&owner, clear.revision)?;
            Ok(Value::Null)
        }
        "notify-track-changed" => {
            let notification = serde_json::from_value::<TrackChangedNotificationRequest>(payload)
                .map_err(|_| {
                BridgeError::invalid_request("track notification request is malformed")
            })?;
            encode(
                manager.notify_track_changed(
                    &owner,
                    notification,
                    app.state::<AppState>()
                        .settings()
                        .track_change_notifications,
                )?,
            )
        }
        "owner-disappeared" => {
            decode_empty(payload)?;
            manager.clear_owned(&owner)?;
            Ok(Value::Null)
        }
        _ => Err(BridgeError::invalid_request(
            "system media operation is unsupported",
        )),
    }
}

fn decode_empty(payload: Value) -> Result<(), BridgeError> {
    serde_json::from_value::<EmptyRequest>(payload)
        .map(|_| ())
        .map_err(|_| BridgeError::invalid_request("system media request must be empty"))
}

fn encode(value: impl serde::Serialize) -> Result<Value, BridgeError> {
    serde_json::to_value(value).map_err(|_| {
        BridgeError::new(
            BridgeErrorCode::InternalError,
            "could not encode the system media response",
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
    if webview.label() != navigation::MAIN_WINDOW_LABEL {
        return Err(origin_not_allowed());
    }
    let selected = normalize_origin(&profile.origin).map_err(|_| origin_not_allowed())?;
    if !same_origin(&selected, &window_url) {
        return Err(origin_not_allowed());
    }
    Ok(selected.as_str().trim_end_matches('/').to_string())
}

fn origin_not_allowed() -> BridgeError {
    BridgeError::new(
        BridgeErrorCode::OriginNotAllowed,
        "system media integration is not available to this page",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_nonempty_capability_and_lifecycle_payloads() {
        assert!(decode_empty(json!({})).is_ok());
        assert!(decode_empty(json!({ "command": "shell" })).is_err());
    }
}
