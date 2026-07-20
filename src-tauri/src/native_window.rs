//! Origin-scoped window chrome for the remote Heya UI.
//!
//! This deliberately exposes a tiny desired-action surface instead of the
//! generic Tauri window API. Every call revalidates that the invoking WebView
//! is the main window and is currently displaying the selected Heya origin.

use crate::{
    native_bridge::NativeBridgeRequest,
    native_playback::{BridgeError, BridgeErrorCode, BridgeResponse},
    navigation,
    server_profile::{normalize_origin, same_origin, AppState},
};
#[cfg(target_os = "macos")]
use block2::RcBlock;
#[cfg(target_os = "macos")]
use objc2::{rc::Retained, MainThreadMarker};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSEvent, NSEventMask, NSEventType, NSWindow, NSWindowButton};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(target_os = "macos")]
use std::{cell::RefCell, ptr::NonNull};
use tauri::{AppHandle, Manager, Runtime, WebviewWindow};

const PROTOCOL_VERSION: u16 = 1;

#[cfg(target_os = "macos")]
const NATIVE_NAVBAR_HEIGHT: f64 = 64.0;

pub fn initialization_script() -> String {
    include_str!("native_window_bridge.js").replace(
        "__HEYA_NATIVE_WINDOW_COMMAND__",
        crate::native_bridge::WINDOW_COMMAND,
    )
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowCapabilities {
    protocol_version: u16,
    platform: &'static str,
    custom_titlebar: bool,
    native_controls: bool,
    draggable: bool,
    minimizable: bool,
    maximizable: bool,
    closable: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowState {
    maximized: bool,
    fullscreen: bool,
    focused: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeControlsVisibility {
    visible: bool,
}

pub(crate) fn handle_window_ipc<R: Runtime>(
    app: &AppHandle<R>,
    webview: &WebviewWindow<R>,
    request: NativeBridgeRequest,
) -> BridgeResponse<Value> {
    if let Err(error) = authorize_webview(app, webview) {
        return BridgeResponse::failure(error);
    }
    if let Err(error) = request.ensure_size("window") {
        return BridgeResponse::failure(error);
    }
    if request.protocol_version != PROTOCOL_VERSION {
        return BridgeResponse::failure(BridgeError::new(
            BridgeErrorCode::ProtocolMismatch,
            "native window protocol version is unsupported",
        ));
    }
    if request.operation != "set-native-controls-visible" && !is_empty_payload(&request.payload) {
        return BridgeResponse::failure(BridgeError::invalid_request(
            "native window operation payload must be empty",
        ));
    }

    match dispatch(webview, &request.operation, &request.payload) {
        Ok(value) => BridgeResponse::success(value),
        Err(error) => BridgeResponse::failure(error),
    }
}

fn dispatch<R: Runtime>(
    window: &WebviewWindow<R>,
    operation: &str,
    payload: &Value,
) -> Result<Value, BridgeError> {
    match operation {
        "capabilities" => encode(WindowCapabilities {
            protocol_version: PROTOCOL_VERSION,
            platform: platform_name(),
            custom_titlebar: !cfg!(target_os = "macos"),
            native_controls: cfg!(target_os = "macos"),
            draggable: true,
            minimizable: true,
            maximizable: true,
            closable: true,
        }),
        "state" => encode(window_state(window)?),
        "minimize" => {
            window.minimize().map_err(window_error)?;
            Ok(Value::Null)
        }
        "toggle-maximize" => {
            if window.is_maximized().map_err(window_error)? {
                window.unmaximize().map_err(window_error)?;
            } else {
                window.maximize().map_err(window_error)?;
            }
            encode(window_state(window)?)
        }
        "start-dragging" => {
            window.start_dragging().map_err(window_error)?;
            Ok(Value::Null)
        }
        "set-native-controls-visible" => {
            let visibility: NativeControlsVisibility = serde_json::from_value(payload.clone())
                .map_err(|_| {
                    BridgeError::invalid_request(
                        "native controls visibility must contain one boolean visible value",
                    )
                })?;
            set_native_controls_visible(window, visibility.visible)?;
            Ok(Value::Null)
        }
        "close" => {
            window.close().map_err(window_error)?;
            Ok(Value::Null)
        }
        _ => Err(BridgeError::invalid_request(
            "native window operation is unsupported",
        )),
    }
}

fn is_empty_payload(payload: &Value) -> bool {
    payload.as_object().is_some_and(serde_json::Map::is_empty)
}

#[cfg(target_os = "macos")]
pub(crate) fn configure_native_main_window<R: Runtime>(
    window: &WebviewWindow<R>,
) -> Result<(), BridgeError> {
    let handle = window.ns_window().map_err(window_error)?;
    if handle.is_null() {
        return Err(invalid_appkit_handle());
    }
    let _mtm = MainThreadMarker::new().ok_or_else(|| {
        BridgeError::new(
            BridgeErrorCode::CommandFailed,
            "native window operation failed: AppKit setup was not on the main thread",
        )
    })?;

    unsafe {
        let ns_window = Retained::retain(handle as *mut NSWindow).ok_or_else(|| {
            BridgeError::new(
                BridgeErrorCode::CommandFailed,
                "native window operation failed: AppKit returned an invalid window",
            )
        })?;
        // Receive mouse-move tracking even while Heya is not the key window,
        // so hovering the player can reveal its controls before activation.
        ns_window.setAcceptsMouseMovedEvents(true);

        install_native_navbar_drag_monitor(ns_window)?;
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn install_native_navbar_drag_monitor(ns_window: Retained<NSWindow>) -> Result<(), BridgeError> {
    let window_number = ns_window.windowNumber();
    let mouse_down = RefCell::<Option<Retained<NSEvent>>>::new(None);
    let block = RcBlock::new(move |event_ptr: NonNull<NSEvent>| -> *mut NSEvent {
        let event = unsafe { event_ptr.as_ref() };
        let pass_through = event_ptr.as_ptr();

        if event.windowNumber() != window_number {
            return pass_through;
        }

        match event.r#type() {
            NSEventType::LeftMouseDown => {
                let inside_navbar = ns_window.contentView().is_some_and(|content| {
                    let bounds = content.bounds();
                    let location = event.locationInWindow();
                    location.y >= (bounds.size.height - NATIVE_NAVBAR_HEIGHT).max(0.0)
                });
                *mouse_down.borrow_mut() = inside_navbar
                    .then(|| unsafe { Retained::retain(event_ptr.as_ptr()) })
                    .flatten();
            }
            NSEventType::LeftMouseDragged => {
                // Drop the RefCell borrow before entering AppKit's nested
                // native tracking loop.
                let mouse_down_event = mouse_down.borrow_mut().take();
                if let Some(mouse_down_event) = mouse_down_event {
                    // Normal mouse-down events still reach WebKit, preserving
                    // links and controls. Only a genuine drag gesture is
                    // claimed here, using AppKit's original mouse-down event
                    // so the move begins immediately and stays native.
                    ns_window.performWindowDragWithEvent(&mouse_down_event);
                    return std::ptr::null_mut();
                }
            }
            NSEventType::LeftMouseUp => {
                mouse_down.borrow_mut().take();
            }
            _ => {}
        }

        pass_through
    });
    let mask =
        NSEventMask::LeftMouseDown | NSEventMask::LeftMouseDragged | NSEventMask::LeftMouseUp;
    let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) }
        .ok_or_else(|| {
            BridgeError::new(
                BridgeErrorCode::CommandFailed,
                "native window operation failed: AppKit could not install the drag monitor",
            )
        })?;

    // AppKit owns the monitor registration, but it does not retain the token
    // returned to the caller. The Heya main window is process-lifetime today,
    // so retaining this one small token for that same lifetime is deliberate.
    std::mem::forget(monitor);
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn enable_history_swipe_gestures<R: Runtime>(
    window: &WebviewWindow<R>,
) -> Result<(), BridgeError> {
    // WKWebView ships with two-finger swipe history navigation disabled and
    // Tauri exposes no builder toggle for it, so the property is set on the
    // native view directly. with_webview runs the closure on the main thread.
    window
        .with_webview(|webview| unsafe {
            let wk_webview = webview.inner().cast::<objc2::runtime::AnyObject>();
            if let Some(wk_webview) = wk_webview.as_ref() {
                let _: () = objc2::msg_send![
                    wk_webview,
                    setAllowsBackForwardNavigationGestures: true
                ];
            }
        })
        .map_err(window_error)
}

#[cfg(target_os = "macos")]
pub(crate) fn set_native_controls_visible<R: Runtime>(
    window: &WebviewWindow<R>,
    visible: bool,
) -> Result<(), BridgeError> {
    let handle = window.ns_window().map_err(window_error)?;
    if handle.is_null() {
        return Err(invalid_appkit_handle());
    }

    // AppKit view mutations must run on the main thread. Passing the stable
    // NSWindow address as an integer keeps the closure Send without claiming
    // that AppKit objects themselves are thread-safe.
    let address = handle as usize;
    window
        .run_on_main_thread(move || unsafe {
            let ns_window = &*(address as *const NSWindow);
            for kind in [
                NSWindowButton::CloseButton,
                NSWindowButton::MiniaturizeButton,
                NSWindowButton::ZoomButton,
            ] {
                if let Some(button) = ns_window.standardWindowButton(kind) {
                    button.setHidden(!visible);
                }
            }
        })
        .map_err(window_error)
}

#[cfg(target_os = "macos")]
fn invalid_appkit_handle() -> BridgeError {
    BridgeError::new(
        BridgeErrorCode::CommandFailed,
        "native window operation failed: AppKit returned an invalid window handle",
    )
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_native_controls_visible<R: Runtime>(
    _window: &WebviewWindow<R>,
    _visible: bool,
) -> Result<(), BridgeError> {
    Err(BridgeError::invalid_request(
        "native window controls are unavailable on this platform",
    ))
}

fn window_state<R: Runtime>(window: &WebviewWindow<R>) -> Result<WindowState, BridgeError> {
    Ok(WindowState {
        maximized: window.is_maximized().map_err(window_error)?,
        fullscreen: window.is_fullscreen().map_err(window_error)?,
        focused: window.is_focused().map_err(window_error)?,
    })
}

fn encode(value: impl Serialize) -> Result<Value, BridgeError> {
    serde_json::to_value(value).map_err(|_| {
        BridgeError::new(
            BridgeErrorCode::InternalError,
            "native window response could not be encoded",
        )
    })
}

fn window_error(error: tauri::Error) -> BridgeError {
    BridgeError::new(
        BridgeErrorCode::CommandFailed,
        format!("native window operation failed: {error}"),
    )
}

fn authorize_webview<R: Runtime>(
    app: &AppHandle<R>,
    webview: &WebviewWindow<R>,
) -> Result<(), BridgeError> {
    if webview.label() != navigation::MAIN_WINDOW_LABEL {
        return Err(origin_not_allowed());
    }
    let window_url = webview.url().map_err(|_| origin_not_allowed())?;
    let profile = app
        .state::<AppState>()
        .profile()
        .ok_or_else(origin_not_allowed)?;
    let selected = normalize_origin(&profile.origin).map_err(|_| origin_not_allowed())?;
    if !same_origin(&selected, &window_url) {
        return Err(origin_not_allowed());
    }
    Ok(())
}

fn origin_not_allowed() -> BridgeError {
    BridgeError::new(
        BridgeErrorCode::OriginNotAllowed,
        "native window controls are not available to this page",
    )
}

fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_do_not_expose_generic_window_or_process_access() {
        let value = serde_json::to_value(WindowCapabilities {
            protocol_version: 1,
            platform: "macos",
            custom_titlebar: false,
            native_controls: true,
            draggable: true,
            minimizable: true,
            maximizable: true,
            closable: true,
        })
        .unwrap();
        let encoded = value.to_string();
        assert_eq!(value["customTitlebar"], false);
        assert_eq!(value["nativeControls"], true);
        assert!(!encoded.contains("invoke"));
        assert!(!encoded.contains("shell"));
        assert!(!encoded.contains("filesystem"));
    }

    #[test]
    fn window_operation_names_are_stable() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(
            platform_name(),
            if cfg!(target_os = "macos") {
                "macos"
            } else if cfg!(target_os = "windows") {
                "windows"
            } else {
                "linux"
            }
        );
    }

    #[test]
    fn native_controls_visibility_payload_is_narrow_and_typed() {
        assert!(serde_json::from_value::<NativeControlsVisibility>(
            serde_json::json!({ "visible": false })
        )
        .is_ok());
        assert!(serde_json::from_value::<NativeControlsVisibility>(
            serde_json::json!({ "visible": "no" })
        )
        .is_err());
        assert!(serde_json::from_value::<NativeControlsVisibility>(
            serde_json::json!({ "visible": true, "opacity": 0 })
        )
        .is_err());
    }
}
