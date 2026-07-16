use crate::server_profile::{same_origin, AppState, ServerProfile};
#[cfg(all(feature = "native-mpv", target_os = "macos"))]
use objc2_app_kit::NSWindow;
use tauri::{
    webview::NewWindowResponse, App, AppHandle, Manager, Url, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use tauri_plugin_opener::OpenerExt;

pub const SWITCH_SERVER_MENU_ID: &str = "switch-server";
pub const SETTINGS_MENU_ID: &str = "settings";
pub const SETTINGS_WINDOW_LABEL: &str = "settings";
pub const MAIN_WINDOW_LABEL: &str = "main";
const CLIENT_MODE_QUERY_KEY: &str = "heya_client";

pub fn create_main_window(
    app: &mut App,
    state: AppState,
    playback: crate::native_playback::NativePlaybackManager,
) -> tauri::Result<WebviewWindow> {
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .ok_or(tauri::Error::WindowNotFound)?
        .clone();

    let navigation_app = app.handle().clone();
    let navigation_state = state.clone();
    let bootstrap_dev_url = app.config().build.dev_url.clone();
    let new_window_app = app.handle().clone();
    let navigation_playback = playback;
    let navigation_audio = app
        .state::<crate::native_audio::NativeAudioManager>()
        .inner()
        .clone();

    let initialization_script = native_initialization_script();

    let window = WebviewWindowBuilder::from_config(app, &config)?
        // The remote page remains visually opaque during normal browsing.
        // Transparency is used only by Heya's full-window native-player route
        // so the origin-validated MPV surface beneath WKWebView can show.
        .transparent(true)
        .initialization_script(initialization_script)
        .use_https_scheme(cfg!(target_os = "windows"))
        .on_navigation(move |url| {
            if is_switch_server_action(url) {
                request_settings(&navigation_app);
                return false;
            }

            let is_bootstrap = is_bootstrap_url(url, bootstrap_dev_url.as_ref());
            let is_selected_server = navigation_state.allows_url(url);
            log::info!(
                "top-level navigation to {} (bootstrap={is_bootstrap}, selected_server={is_selected_server})",
                origin_for_log(url)
            );

            if is_bootstrap || is_selected_server {
                if let Err(error) =
                    navigation_playback.dispose_active(crate::native_playback::TerminationReason::Disposed)
                {
                    log::warn!("could not dispose native playback before navigation: {}", error.message);
                }
                if let Err(error) =
                    navigation_audio.dispose_active(crate::native_playback::TerminationReason::Disposed)
                {
                    log::warn!("could not dispose native audio before navigation: {}", error.message);
                }
                true
            } else {
                open_external(&navigation_app, url);
                false
            }
        })
        .on_new_window(move |url, _features| {
            open_external(&new_window_app, &url);
            NewWindowResponse::Deny
        })
        .build()?;

    #[cfg(all(feature = "native-mpv", target_os = "macos"))]
    match window.ns_window() {
        Ok(handle) if !handle.is_null() => unsafe {
            // Receive mouse-move tracking even while Heya is not the key
            // window, so hovering the player can reveal its controls before
            // the user clicks to activate it.
            (&*(handle as *const NSWindow)).setAcceptsMouseMovedEvents(true);
        },
        Ok(_) => log::warn!("the Heya window returned an invalid AppKit handle"),
        Err(error) => log::warn!("could not enable inactive player hover tracking: {error}"),
    }

    Ok(window)
}

fn native_initialization_script() -> String {
    // Both bridge sources are IIFEs. A bare newline is not a JavaScript
    // statement boundary: without this semicolon WebKit parses the second
    // IIFE as a call on the first one's return value, so only video starts.
    let playback = crate::native_playback::initialization_script();
    let audio = crate::native_audio::audio_initialization_script();
    format!("{};\n{}", playback.trim_end(), audio.trim_start())
}

pub fn navigate_to_server(window: &WebviewWindow, profile: &ServerProfile) -> Result<(), String> {
    let mut url = Url::parse(&profile.origin)
        .map_err(|error| format!("the saved Heya URL is invalid: {error}"))?;
    url.query_pairs_mut()
        .append_pair(CLIENT_MODE_QUERY_KEY, "1");
    window
        .set_title("Heya")
        .map_err(|error| format!("could not reset the Heya window title: {error}"))?;
    window
        .navigate(url)
        .map_err(|error| format!("could not open the Heya server: {error}"))
}

pub fn navigate_main_to_server(app: &AppHandle, profile: &ServerProfile) -> Result<(), String> {
    navigate_to_server(&main_window(app)?, profile)
}

pub fn main_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    app.get_webview_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| "the main Heya window is not available".to_string())
}

pub fn navigate_main_to_picker(app: &AppHandle) -> Result<(), String> {
    let window = main_window(app)?;
    window
        .set_title("Connect to Heya")
        .map_err(|error| format!("could not update the Heya window title: {error}"))?;
    window
        .navigate(bootstrap_url(
            app.config().build.dev_url.as_ref(),
            "manual=1",
        ))
        .map_err(|error| format!("could not open the server picker: {error}"))
}

pub fn request_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(SETTINGS_WINDOW_LABEL) {
        if let Err(error) = window.show().and_then(|_| window.set_focus()) {
            log::error!("could not focus Heya client settings: {error}");
        }
        return;
    }

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = create_settings_window(&app) {
            log::error!("could not open Heya client settings: {error}");
        }
    });
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn create_settings_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    let settings_url = bootstrap_url(app.config().build.dev_url.as_ref(), "settings=1");
    let navigation_app = app.clone();
    let bootstrap_dev_url = app.config().build.dev_url.clone();
    let new_window_app = app.clone();

    WebviewWindowBuilder::new(
        app,
        SETTINGS_WINDOW_LABEL,
        WebviewUrl::External(settings_url),
    )
    .title("Heya Settings")
    .inner_size(620.0, 720.0)
    .min_inner_size(480.0, 560.0)
    .resizable(true)
    .maximizable(false)
    .fullscreen(false)
    .center()
    .prevent_overflow()
    .on_navigation(move |url| {
        if is_bootstrap_url(url, bootstrap_dev_url.as_ref()) {
            true
        } else {
            open_external(&navigation_app, url);
            false
        }
    })
    .on_new_window(move |url, _features| {
        open_external(&new_window_app, &url);
        NewWindowResponse::Deny
    })
    .build()
    .map_err(|error| format!("could not create the local settings window: {error}"))
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn create_settings_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    let window = main_window(app)?;
    window
        .navigate(bootstrap_url(
            app.config().build.dev_url.as_ref(),
            "settings=1",
        ))
        .map_err(|error| format!("could not open the local settings page: {error}"))?;
    Ok(window)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn install_server_menu(app: &mut App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItemBuilder, SubmenuBuilder};

    let settings = MenuItemBuilder::with_id(SETTINGS_MENU_ID, "Settings…")
        .accelerator("CmdOrCtrl+,")
        .build(app)?;
    let switch_server = MenuItemBuilder::with_id(SWITCH_SERVER_MENU_ID, "Switch Server…")
        .accelerator("CmdOrCtrl+Shift+S")
        .build(app)?;
    let server_menu = SubmenuBuilder::new(app, "Server")
        .item(&settings)
        .item(&switch_server)
        .build()?;

    #[cfg(all(debug_assertions, feature = "native-mpv"))]
    {
        let native_playback_test = MenuItemBuilder::with_id(
            crate::native_playback::NATIVE_MPV_SPIKE_MENU_ID,
            "Open Native MPV Test Window",
        )
        .build(app)?;
        server_menu.append(&native_playback_test)?;
        let native_fullscreen_on = MenuItemBuilder::with_id(
            crate::native_playback::NATIVE_MPV_FULLSCREEN_ON_MENU_ID,
            "Enter Native MPV Test Fullscreen",
        )
        .build(app)?;
        let native_fullscreen_off = MenuItemBuilder::with_id(
            crate::native_playback::NATIVE_MPV_FULLSCREEN_OFF_MENU_ID,
            "Exit Native MPV Test Fullscreen",
        )
        .build(app)?;
        server_menu.append(&native_fullscreen_on)?;
        server_menu.append(&native_fullscreen_off)?;
    }

    let menu = Menu::default(app.handle())?;
    menu.append(&server_menu)?;
    app.set_menu(menu)?;
    Ok(())
}

fn is_bootstrap_url(url: &Url, dev_url: Option<&Url>) -> bool {
    (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
        || (matches!(url.scheme(), "http" | "https") && url.host_str() == Some("tauri.localhost"))
        || dev_url.is_some_and(|dev_url| same_origin(dev_url, url))
}

pub fn is_bootstrap_window(window: &WebviewWindow) -> bool {
    window.url().is_ok_and(|url| {
        is_bootstrap_url(&url, window.app_handle().config().build.dev_url.as_ref())
    })
}

fn bootstrap_url(dev_url: Option<&Url>, query: &str) -> Url {
    let mut url = dev_url.cloned().unwrap_or_else(|| {
        if cfg!(any(target_os = "windows", target_os = "android")) {
            Url::parse("http://tauri.localhost/").expect("the built-in Tauri URL is valid")
        } else {
            Url::parse("tauri://localhost/").expect("the built-in Tauri URL is valid")
        }
    });
    url.set_query(Some(query));
    url
}

fn is_switch_server_action(url: &Url) -> bool {
    url.scheme() == "heya-client"
        && url.host_str() == Some("switch-server")
        && matches!(url.path(), "" | "/")
        && url.query().is_none()
        && url.fragment().is_none()
}

fn open_external(app: &AppHandle, url: &Url) {
    if matches!(url.scheme(), "http" | "https" | "mailto" | "tel") {
        log::info!("opening external URL at {}", origin_for_log(url));
        if let Err(error) = app.opener().open_url(url.as_str(), None::<&str>) {
            log::warn!("could not open external URL {url}: {error}");
        }
    } else {
        log::warn!("blocked top-level navigation to unsupported URL scheme: {url}");
    }
}

fn origin_for_log(url: &Url) -> String {
    let host = url.host_str().unwrap_or("<no-host>");
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_bootstrap_url, is_switch_server_action, native_initialization_script};
    use tauri::Url;

    #[test]
    fn separates_native_bridge_iifes_with_a_statement_boundary() {
        let script = native_initialization_script();
        assert!(script.contains("})();\n(() => {"));
        assert!(script.contains("__HEYA_NATIVE_PLAYBACK__"));
        assert!(script.contains("__HEYA_NATIVE_AUDIO__"));
        assert!(script.contains("plugin:native-bridge|native_audio_request"));
        assert!(script.contains("plugin:native-bridge|native_playback_request"));
        assert!(!script.contains("heya-native-audio://"));
        assert!(!script.contains("heya-native-playback://"));
    }

    #[test]
    fn recognizes_tauri_asset_origins_only() {
        assert!(is_bootstrap_url(
            &Url::parse("tauri://localhost/?manual=1").unwrap(),
            None,
        ));
        assert!(is_bootstrap_url(
            &Url::parse("http://tauri.localhost/").unwrap(),
            None,
        ));
        assert!(is_bootstrap_url(
            &Url::parse("http://127.0.0.1:1430/settings").unwrap(),
            Some(&Url::parse("http://127.0.0.1:1430/").unwrap()),
        ));
        assert!(!is_bootstrap_url(
            &Url::parse("https://tauri.example.com/").unwrap(),
            None,
        ));
        assert!(!is_bootstrap_url(
            &Url::parse("http://127.0.0.1:8080/").unwrap(),
            Some(&Url::parse("http://127.0.0.1:1430/").unwrap()),
        ));
    }

    #[test]
    fn recognizes_only_the_narrow_switch_server_action() {
        assert!(is_switch_server_action(
            &Url::parse("heya-client://switch-server").unwrap()
        ));
        assert!(!is_switch_server_action(
            &Url::parse("heya-client://switch-server/anything").unwrap()
        ));
        assert!(!is_switch_server_action(
            &Url::parse("heya-client://reset-session").unwrap()
        ));
    }
}
