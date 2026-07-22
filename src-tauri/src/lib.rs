#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
mod app_updates;
mod application;
pub mod native_audio;
mod native_bridge;
pub mod native_playback;
mod native_window;
mod navigation;
mod server_profile;
pub mod system_media;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
mod window_state;

use serde::Serialize;
use server_profile::{AppSettings, AppState, ServerProfile};
use std::sync::Arc;
use tauri::{AppHandle, Manager, State, WebviewWindow};

#[derive(Serialize)]
pub(crate) struct NativePlaybackStatus {
    pub(crate) backend: &'static str,
    pub(crate) available: bool,
    pub(crate) build_includes_native_mpv: bool,
    pub(crate) video_surface: native_playback::NativeVideoSurface,
    pub(crate) unavailable_reason: Option<native_playback::BridgeErrorCode>,
    pub(crate) installation: native_playback::MpvInstallationOffer,
}

#[derive(Serialize)]
pub(crate) struct NativeAudioStatus {
    pub(crate) backend: &'static str,
    pub(crate) available: bool,
    pub(crate) gapless: bool,
    pub(crate) crossfade: bool,
}

#[tauri::command]
fn get_server_profile(state: State<'_, AppState>) -> Option<ServerProfile> {
    state.profile()
}

#[tauri::command]
fn get_app_settings(state: State<'_, AppState>) -> AppSettings {
    state.settings()
}

#[tauri::command]
fn get_native_playback_status(
    playback: State<'_, native_playback::NativePlaybackManager>,
) -> NativePlaybackStatus {
    native_playback_status(&playback)
}

pub(crate) fn native_playback_status(
    playback: &native_playback::NativePlaybackManager,
) -> NativePlaybackStatus {
    let capabilities = playback.capabilities();
    NativePlaybackStatus {
        backend: capabilities.backend,
        available: capabilities.available,
        build_includes_native_mpv: cfg!(all(
            feature = "native-mpv",
            any(
                target_os = "macos",
                target_os = "windows",
                target_os = "linux"
            )
        )),
        video_surface: capabilities.video_surface,
        unavailable_reason: capabilities.unavailable_reason,
        installation: native_playback::mpv_installation_offer(),
    }
}

#[tauri::command]
async fn install_native_playback_runtime(
    app: AppHandle,
    invoking_window: WebviewWindow,
    playback: State<'_, native_playback::NativePlaybackManager>,
    on_event: tauri::ipc::Channel<native_playback::MpvInstallProgress>,
) -> Result<NativePlaybackStatus, String> {
    navigation::ensure_local_settings_window(&invoking_window, "MPV installation")?;
    native_playback::install_mpv_runtime(app, on_event).await?;
    let status = native_playback_status(&playback);
    if !status.available {
        return Err("MPV was installed but its native backend could not initialize".into());
    }
    Ok(status)
}

#[tauri::command]
fn get_native_audio_status(
    audio: State<'_, native_audio::NativeAudioManager>,
) -> NativeAudioStatus {
    native_audio_status(&audio)
}

pub(crate) fn native_audio_status(audio: &native_audio::NativeAudioManager) -> NativeAudioStatus {
    let capabilities = audio.capabilities();
    NativeAudioStatus {
        backend: capabilities.backend,
        available: capabilities.available,
        gapless: capabilities.gapless,
        crossfade: capabilities.crossfade,
    }
}

#[tauri::command]
fn save_app_settings(
    settings: AppSettings,
    state: State<'_, AppState>,
) -> Result<AppSettings, String> {
    state.save_settings(settings)
}

#[tauri::command]
async fn connect_to_server(
    origin: String,
    app: AppHandle,
    invoking_window: WebviewWindow,
    state: State<'_, AppState>,
    playback: State<'_, native_playback::NativePlaybackManager>,
    audio: State<'_, native_audio::NativeAudioManager>,
    system_media: State<'_, system_media::SystemMediaManager>,
) -> Result<ServerProfile, String> {
    let previous = state.profile();
    let profile = state.validate_and_store(&origin).await?;
    native_bridge::authorize_origin(&app, &profile.origin)?;
    if previous.is_some_and(|previous| previous.origin != profile.origin) {
        playback
            .dispose_active(native_playback::TerminationReason::ServerSwitched)
            .map_err(|error| error.message)?;
        audio
            .dispose_active(native_playback::TerminationReason::ServerSwitched)
            .map_err(|error| error.message)?;
        system_media.clear_all();
    }
    navigation::navigate_main_to_server(&app, &profile)?;
    close_settings_window(&invoking_window)?;
    Ok(profile)
}

#[tauri::command]
fn forget_server(app: AppHandle, invoking_window: WebviewWindow) -> Result<(), String> {
    forget_server_inner(&app)?;
    close_settings_window(&invoking_window)
}

pub(crate) fn forget_server_inner(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let playback = app.state::<native_playback::NativePlaybackManager>();
    let audio = app.state::<native_audio::NativeAudioManager>();
    let system_media = app.state::<system_media::SystemMediaManager>();
    playback
        .dispose_active(native_playback::TerminationReason::ServerSwitched)
        .map_err(|error| error.message)?;
    audio
        .dispose_active(native_playback::TerminationReason::ServerSwitched)
        .map_err(|error| error.message)?;
    system_media.clear_all();
    navigation::main_window(app)?
        .clear_all_browsing_data()
        .map_err(|error| format!("could not clear the Heya WebView session: {error}"))?;
    state.forget()?;
    navigation::navigate_main_to_picker(app)
}

#[tauri::command]
fn reset_server_session(app: AppHandle, invoking_window: WebviewWindow) -> Result<(), String> {
    reset_server_session_inner(&app)?;
    close_settings_window(&invoking_window)
}

pub(crate) fn reset_server_session_inner(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let playback = app.state::<native_playback::NativePlaybackManager>();
    let audio = app.state::<native_audio::NativeAudioManager>();
    let system_media = app.state::<system_media::SystemMediaManager>();
    playback
        .dispose_active(native_playback::TerminationReason::LoggedOut)
        .map_err(|error| error.message)?;
    audio
        .dispose_active(native_playback::TerminationReason::LoggedOut)
        .map_err(|error| error.message)?;
    system_media.clear_all();
    let profile = state
        .profile()
        .ok_or_else(|| "Choose a Heya server before resetting its session.".to_string())?;
    navigation::main_window(app)?
        .clear_all_browsing_data()
        .map_err(|error| format!("could not clear the Heya WebView session: {error}"))?;
    navigation::navigate_main_to_server(app, &profile)
}

fn close_settings_window(window: &WebviewWindow) -> Result<(), String> {
    if window.label() == navigation::SETTINGS_WINDOW_LABEL {
        window
            .close()
            .map_err(|error| format!("could not close the Heya settings window: {error}"))?;
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(all(
        feature = "native-mpv",
        any(target_os = "macos", target_os = "windows", target_os = "linux")
    ))]
    native_playback::configure_bundled_vulkan_loader();

    let builder = tauri::Builder::default()
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .plugin(native_playback::lifecycle_plugin())
        .plugin(native_audio::audio_lifecycle_plugin())
        .plugin(system_media::system_media_lifecycle_plugin())
        .plugin(tauri_plugin_notification::init())
        .plugin(native_bridge::plugin())
        .invoke_handler(tauri::generate_handler![
            get_server_profile,
            get_app_settings,
            get_native_playback_status,
            install_native_playback_runtime,
            get_native_audio_status,
            save_app_settings,
            connect_to_server,
            forget_server,
            reset_server_session,
        ]);

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    let builder = builder
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(window_state::plugin())
        .on_window_event(window_state::handle_window_event);

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = builder.on_menu_event(|app, event| {
        if event.id().as_ref() == navigation::SETTINGS_MENU_ID {
            navigation::request_settings(app);
        } else if event.id().as_ref() == navigation::SWITCH_SERVER_MENU_ID {
            if let Err(error) = navigation::navigate_main_to_picker(app) {
                log::error!("could not open the Heya server picker: {error}");
            }
        } else if let Some(command) = system_media::menu_command(event.id().as_ref()) {
            app.state::<system_media::SystemMediaManager>()
                .dispatch_menu_command(command);
        } else {
            #[cfg(all(debug_assertions, feature = "native-mpv"))]
            if event.id().as_ref() == native_playback::NATIVE_MPV_SPIKE_MENU_ID {
                let manager = app.state::<native_playback::NativePlaybackManager>();
                if let Err(error) = native_playback::start_development_harness(&manager) {
                    log::error!("could not open the native MPV test window: {error}");
                }
            } else if event.id().as_ref() == native_playback::NATIVE_MPV_FULLSCREEN_ON_MENU_ID
                || event.id().as_ref() == native_playback::NATIVE_MPV_FULLSCREEN_OFF_MENU_ID
            {
                let fullscreen =
                    event.id().as_ref() == native_playback::NATIVE_MPV_FULLSCREEN_ON_MENU_ID;
                let manager = app.state::<native_playback::NativePlaybackManager>();
                if let Err(error) = manager.send_development_command(
                    native_playback::PlaybackCommand::SetFullscreen { fullscreen },
                ) {
                    log::error!(
                        "could not set native MPV test fullscreen: {}",
                        error.message
                    );
                }
            }
        }
    });

    builder
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }

            let config_dir = app.path().app_config_dir()?;
            let state = AppState::new(config_dir)
                .map_err(|error| std::io::Error::other(format!("Heya setup failed: {error}")))?;
            app.manage(state.clone());
            app.manage(native_bridge::NativeBridgeAcl::default());
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            app.manage(app_updates::AppUpdater::default());
            if let Some(profile) = state.profile() {
                native_bridge::authorize_origin(app.handle(), &profile.origin)
                    .map_err(std::io::Error::other)?;
            }

            #[cfg(all(
                feature = "native-mpv",
                any(target_os = "macos", target_os = "windows", target_os = "linux")
            ))]
            native_playback::configure_runtime_loader(app.handle())
                .map_err(std::io::Error::other)?;
            #[cfg(all(
                feature = "native-mpv",
                any(target_os = "macos", target_os = "windows", target_os = "linux")
            ))]
            let engine_factory: Arc<dyn native_playback::PlaybackEngineFactory> =
                Arc::new(native_playback::MpvEngineFactory::new(app.handle().clone()));
            #[cfg(not(all(
                feature = "native-mpv",
                any(target_os = "macos", target_os = "windows", target_os = "linux")
            )))]
            let engine_factory: Arc<dyn native_playback::PlaybackEngineFactory> =
                Arc::new(native_playback::UnavailableEngineFactory);
            let event_sink = native_playback::TauriPlaybackEventSink::new(app.handle().clone());
            let playback = native_playback::NativePlaybackManager::new(engine_factory, event_sink);
            app.manage(playback.clone());

            let audio_sink = native_audio::TauriAudioEventSink::new(app.handle().clone());
            let audio = native_audio::NativeAudioManager::new(audio_sink);
            app.manage(audio);

            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            window_state::install(app)?;

            let main_window = navigation::create_main_window(app, state, playback)?;
            let system_media_cache = app.path().app_cache_dir()?;
            let system_media = system_media::SystemMediaManager::new(
                app.handle().clone(),
                &main_window,
                system_media_cache,
            );
            app.manage(system_media);

            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            window_state::save_now(app.handle());

            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            navigation::install_server_menu(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Heya client");
}
