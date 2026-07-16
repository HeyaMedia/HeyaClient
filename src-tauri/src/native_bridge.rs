//! Narrow IPC transport shared by Heya's native audio and video bridges.
//!
//! The remote page never receives a generic application API. Its frozen
//! JavaScript bridge objects call only the semantic commands registered here, and
//! Rust validates the selected server origin plus every operation payload.

use crate::{
    native_audio, native_playback, navigation, server_profile::normalize_origin, system_media,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{hash_map::DefaultHasher, HashSet},
    hash::{Hash, Hasher},
    sync::Mutex,
};
use tauri::{
    ipc::CapabilityBuilder, plugin::TauriPlugin, AppHandle, Manager, Runtime, WebviewWindow,
};

pub const PLUGIN_NAME: &str = "native-bridge";
pub const AUDIO_COMMAND: &str = "plugin:native-bridge|native_audio_request";
pub const PLAYBACK_COMMAND: &str = "plugin:native-bridge|native_playback_request";
pub const SYSTEM_MEDIA_COMMAND: &str = "plugin:native-bridge|system_media_request";
pub const WINDOW_COMMAND: &str = "plugin:native-bridge|native_window_request";
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

const AUDIO_PERMISSION: &str = "native-bridge:allow-native-audio-request";
const PLAYBACK_PERMISSION: &str = "native-bridge:allow-native-playback-request";
const SYSTEM_MEDIA_PERMISSION: &str = "native-bridge:allow-system-media-request";
const WINDOW_PERMISSION: &str = "native-bridge:allow-native-window-request";
#[cfg(not(target_os = "macos"))]
const START_DRAGGING_PERMISSION: &str = "core:window:allow-start-dragging";
#[cfg(not(target_os = "macos"))]
const INTERNAL_TOGGLE_MAXIMIZE_PERMISSION: &str = "core:window:allow-internal-toggle-maximize";

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeBridgeRequest {
    pub protocol_version: u16,
    pub page_instance_id: native_playback::PageInstanceId,
    pub operation: String,
    pub payload: Value,
}

impl NativeBridgeRequest {
    pub fn ensure_size(&self, kind: &str) -> Result<(), native_playback::BridgeError> {
        self.ensure_size_with_limit(kind, MAX_REQUEST_BYTES)
    }

    pub fn ensure_size_with_limit(
        &self,
        kind: &str,
        limit: usize,
    ) -> Result<(), native_playback::BridgeError> {
        let within_limit = serde_json::to_vec(self)
            .map(|encoded| encoded.len() <= limit)
            .unwrap_or(false);
        if within_limit {
            Ok(())
        } else {
            Err(native_playback::BridgeError::invalid_request(format!(
                "native {kind} request is too large"
            )))
        }
    }
}

#[derive(Default)]
pub struct NativeBridgeAcl {
    origins: Mutex<HashSet<String>>,
}

pub fn plugin<R: Runtime>() -> TauriPlugin<R> {
    tauri::plugin::Builder::new(PLUGIN_NAME)
        .invoke_handler(tauri::generate_handler![
            native_audio_request,
            native_playback_request,
            system_media_request,
            native_window_request
        ])
        .build()
}

#[tauri::command]
fn native_audio_request<R: Runtime>(
    app: AppHandle<R>,
    webview: WebviewWindow<R>,
    request: NativeBridgeRequest,
) -> native_playback::BridgeResponse<Value> {
    native_audio::handle_audio_ipc(&app, &webview, request)
}

#[tauri::command]
async fn native_playback_request<R: Runtime>(
    app: AppHandle<R>,
    webview: WebviewWindow<R>,
    request: NativeBridgeRequest,
) -> native_playback::BridgeResponse<Value> {
    // Creating libmpv, attaching its native surface, and waiting for renderer
    // commands are deliberately synchronous inside the playback manager. Do
    // that work on Tauri's blocking pool: a synchronous command handler runs
    // on the window event loop and would freeze the WebView (and prevent the
    // AppKit/Win32 surface callback from running) during decoder/GPU startup.
    match tauri::async_runtime::spawn_blocking(move || {
        native_playback::handle_playback_ipc(&app, &webview, request)
    })
    .await
    {
        Ok(response) => response,
        Err(_) => native_playback::BridgeResponse::failure(native_playback::BridgeError::new(
            native_playback::BridgeErrorCode::InternalError,
            "the native playback request worker stopped unexpectedly",
        )),
    }
}

#[tauri::command]
fn native_window_request<R: Runtime>(
    app: AppHandle<R>,
    webview: WebviewWindow<R>,
    request: NativeBridgeRequest,
) -> native_playback::BridgeResponse<Value> {
    crate::native_window::handle_window_ipc(&app, &webview, request)
}

#[tauri::command]
fn system_media_request<R: Runtime>(
    app: AppHandle<R>,
    webview: WebviewWindow<R>,
    request: NativeBridgeRequest,
) -> native_playback::BridgeResponse<Value> {
    system_media::handle_system_media_ipc(&app, &webview, request)
}

/// Authorize only the selected Heya origin to call the semantic bridge
/// commands. Previously authorized origins remain harmless after a server
/// switch: top-level navigation rejects them and each command independently
/// verifies the currently selected origin before dispatch.
pub fn authorize_origin<R: Runtime>(app: &AppHandle<R>, origin: &str) -> Result<(), String> {
    let origin = normalize_origin(origin)?
        .as_str()
        .trim_end_matches('/')
        .to_string();
    let acl = app.state::<NativeBridgeAcl>();
    {
        let mut origins = acl
            .origins
            .lock()
            .map_err(|error| format!("native bridge origin lock was poisoned: {error}"))?;
        if !origins.insert(origin.clone()) {
            return Ok(());
        }
    }

    let capability = CapabilityBuilder::new(capability_identifier(&origin))
        .remote(remote_pattern(&origin))
        .local(false)
        .window(navigation::MAIN_WINDOW_LABEL)
        .permission(AUDIO_PERMISSION)
        .permission(PLAYBACK_PERMISSION)
        .permission(SYSTEM_MEDIA_PERMISSION)
        .permission(WINDOW_PERMISSION);

    #[cfg(not(target_os = "macos"))]
    let capability = capability
        // Windows and Linux still use Heya's custom titlebar. macOS keeps a
        // real AppKit titlebar, so it must never take the delayed IPC drag
        // path.
        .permission(START_DRAGGING_PERMISSION)
        .permission(INTERNAL_TOGGLE_MAXIMIZE_PERMISSION);

    let result = app.add_capability(capability);
    if let Err(error) = result {
        if let Ok(mut origins) = acl.origins.lock() {
            origins.remove(&origin);
        }
        return Err(format!(
            "could not authorize the selected Heya origin for native playback: {error}"
        ));
    }
    Ok(())
}

fn remote_pattern(origin: &str) -> String {
    format!("{}/*", origin.trim_end_matches('/'))
}

fn capability_identifier(origin: &str) -> String {
    let mut hasher = DefaultHasher::new();
    origin.hash(&mut hasher);
    format!("heya-native-bridge-{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_origin_pattern_covers_paths_without_authorizing_subdomains() {
        assert_eq!(
            remote_pattern("https://heya.example.com"),
            "https://heya.example.com/*"
        );
        assert!(!remote_pattern("https://heya.example.com").contains("*."));
    }

    #[test]
    fn bridge_request_rejects_unknown_fields_and_oversized_payloads() {
        assert!(serde_json::from_str::<NativeBridgeRequest>(
            r#"{
                "protocolVersion": 1,
                "pageInstanceId": "d9428888-122b-11e1-b85c-61cd3cbb3210",
                "operation": "capabilities",
                "payload": {},
                "command": "shell"
            }"#,
        )
        .is_err());

        let request = NativeBridgeRequest {
            protocol_version: 1,
            page_instance_id: native_playback::PageInstanceId::parse(
                "d9428888-122b-11e1-b85c-61cd3cbb3210",
            )
            .unwrap(),
            operation: "capabilities".into(),
            payload: Value::String("x".repeat(MAX_REQUEST_BYTES)),
        };
        assert!(request.ensure_size("audio").is_err());
    }
}
