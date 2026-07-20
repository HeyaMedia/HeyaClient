use crate::server_profile::{same_origin, AppState, ServerProfile};
use tauri::{
    webview::NewWindowResponse, App, AppHandle, Manager, Url, WebviewWindow, WebviewWindowBuilder,
};
use tauri_plugin_opener::OpenerExt;

pub const SWITCH_SERVER_MENU_ID: &str = "switch-server";
pub const SETTINGS_MENU_ID: &str = "settings";
pub const SETTINGS_WINDOW_LABEL: &str = "settings";
pub const MAIN_WINDOW_LABEL: &str = "main";
const CLIENT_MODE_QUERY_KEY: &str = "heya_client";
const OPEN_APPLICATION_SETTINGS_SCRIPT: &str =
    "window.dispatchEvent(new CustomEvent('heya:application:open-settings-v1'))";

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

    let builder = WebviewWindowBuilder::from_config(app, &config)?
        // The remote page remains visually opaque during normal browsing.
        // Transparency is used only by Heya's full-window native-player route
        // so the origin-validated MPV surface beneath WKWebView can show.
        .transparent(true)
        .initialization_script(initialization_script)
        .use_https_scheme(cfg!(target_os = "windows"))
        .on_navigation(move |url| {
            if is_switch_server_action(url) {
                let picker_app = navigation_app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = navigate_main_to_picker(&picker_app) {
                        log::error!("could not open the Heya server picker: {error}");
                    }
                });
                return false;
            }

            let is_bootstrap = is_bootstrap_url(url, bootstrap_dev_url.as_ref());
            let is_selected_server = navigation_state.allows_url(url);
            log::info!(
                "top-level navigation to {} (bootstrap={is_bootstrap}, selected_server={is_selected_server})",
                origin_for_log(url)
            );

            if is_bootstrap || is_selected_server {
                #[cfg(target_os = "macos")]
                if let Some(window) =
                    navigation_app.get_webview_window(MAIN_WINDOW_LABEL)
                {
                    if let Err(error) =
                        crate::native_window::set_native_controls_visible(&window, true)
                    {
                        log::warn!(
                            "could not restore native window controls before navigation: {}",
                            error.message
                        );
                    }
                }
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
        });

    #[cfg(target_os = "macos")]
    let builder = builder
        // Keep the real AppKit titlebar while allowing Heya's topbar to paint
        // beneath its transparent overlay. A native gesture monitor is added
        // after construction so the entire Heya navbar can drag without
        // swallowing ordinary clicks on its controls. Native traffic lights
        // also restore the system's inactive-window, accessibility, shadow
        // and corner behaviour.
        .decorations(true)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(16.0, 20.0));

    let window = builder.build()?;

    #[cfg(target_os = "macos")]
    if let Err(error) = crate::native_window::configure_native_main_window(&window) {
        log::warn!(
            "could not configure native macOS window chrome: {}",
            error.message
        );
    }

    Ok(window)
}

fn native_initialization_script() -> String {
    // Every bridge source is an IIFE. Bare newlines are not JavaScript
    // statement boundaries, so keep explicit semicolons between them.
    let playback = crate::native_playback::initialization_script();
    let audio = crate::native_audio::audio_initialization_script();
    let system_media = crate::system_media::system_media_initialization_script();
    let application = crate::application::initialization_script();
    let window = crate::native_window::initialization_script();
    format!(
        "{};\n{};\n{};\n{};\n{}",
        playback.trim_end(),
        audio.trim(),
        system_media.trim(),
        application.trim(),
        window.trim_start()
    )
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
    if app.state::<AppState>().profile().is_none() {
        if let Err(error) = navigate_main_to_picker(app) {
            log::error!("could not open the Heya server picker: {error}");
        }
        return;
    }
    let result = main_window(app).and_then(|window| {
        window
            .eval(OPEN_APPLICATION_SETTINGS_SCRIPT)
            .map_err(|error| format!("could not request Heya application settings: {error}"))
    });
    if let Err(error) = result {
        log::error!("could not open application settings: {error}");
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn install_server_menu(app: &mut App) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItemBuilder, SubmenuBuilder};

    let play_pause =
        MenuItemBuilder::with_id(crate::system_media::PLAY_PAUSE_MENU_ID, "Play/Pause")
            .accelerator("CmdOrCtrl+Shift+Space")
            .build(app)?;
    let previous = MenuItemBuilder::with_id(crate::system_media::PREVIOUS_MENU_ID, "Previous")
        .accelerator("CmdOrCtrl+Shift+Left")
        .build(app)?;
    let next = MenuItemBuilder::with_id(crate::system_media::NEXT_MENU_ID, "Next")
        .accelerator("CmdOrCtrl+Shift+Right")
        .build(app)?;
    let playback_menu = SubmenuBuilder::new(app, "Playback")
        .item(&play_pause)
        .item(&previous)
        .item(&next)
        .build()?;

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
    menu.append(&playback_menu)?;
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

pub fn ensure_local_settings_window(window: &WebviewWindow, operation: &str) -> Result<(), String> {
    if window.label() == SETTINGS_WINDOW_LABEL && is_bootstrap_window(window) {
        Ok(())
    } else {
        Err(format!(
            "{operation} is available only from local HeyaClient settings"
        ))
    }
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
        assert!(script.contains("__HEYA_SYSTEM_MEDIA__"));
        assert!(script.contains("__HEYA_APPLICATION__"));
        assert!(script.contains("plugin:native-bridge|native_audio_request"));
        assert!(script.contains("plugin:native-bridge|native_playback_request"));
        assert!(script.contains("plugin:native-bridge|system_media_request"));
        assert!(script.contains("plugin:native-bridge|application_request"));
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
