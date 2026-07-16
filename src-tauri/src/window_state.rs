use std::{
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread,
    time::Duration,
};
use tauri::{App, AppHandle, Manager, Runtime, Window, WindowEvent};
use tauri_plugin_window_state::{AppHandleExt, StateFlags};

const MAIN_WINDOW_LABEL: &str = "main";
const SAVE_DEBOUNCE: Duration = Duration::from_millis(400);

enum SaveRequest {
    Debounced,
    Immediate,
}

struct WindowStateSaver(Sender<SaveRequest>);

pub fn plugin<R: Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri_plugin_window_state::Builder::default()
        .with_state_flags(state_flags())
        .with_denylist(&[crate::navigation::SETTINGS_WINDOW_LABEL])
        .build()
}

pub fn install<R: Runtime>(app: &mut App<R>) -> std::io::Result<()> {
    let (sender, receiver) = mpsc::channel();
    app.manage(WindowStateSaver(sender));

    let app_handle = app.handle().clone();
    thread::Builder::new()
        .name("heya-window-state".to_string())
        .spawn(move || persist_worker(app_handle, receiver))?;
    Ok(())
}

pub fn handle_window_event<R: Runtime>(window: &Window<R>, event: &WindowEvent) {
    if window.label() != MAIN_WINDOW_LABEL {
        return;
    }

    let request = match event {
        WindowEvent::Moved(_)
        | WindowEvent::Resized(_)
        | WindowEvent::ScaleFactorChanged { .. } => SaveRequest::Debounced,
        WindowEvent::CloseRequested { .. } | WindowEvent::Destroyed => SaveRequest::Immediate,
        _ => return,
    };

    if window.state::<WindowStateSaver>().0.send(request).is_err() {
        log::warn!("could not queue the Heya window state for saving");
    }
}

pub fn save_now<R: Runtime>(app: &AppHandle<R>) {
    persist(app);
}

fn persist_worker<R: Runtime>(app: AppHandle<R>, receiver: Receiver<SaveRequest>) {
    while let Ok(request) = receiver.recv() {
        match request {
            SaveRequest::Immediate => schedule_persist(&app),
            SaveRequest::Debounced => debounce_and_persist(&app, &receiver),
        }
    }
}

fn debounce_and_persist<R: Runtime>(app: &AppHandle<R>, receiver: &Receiver<SaveRequest>) {
    loop {
        match receiver.recv_timeout(SAVE_DEBOUNCE) {
            Ok(SaveRequest::Debounced) => {}
            Ok(SaveRequest::Immediate) | Err(RecvTimeoutError::Timeout) => {
                schedule_persist(app);
                return;
            }
            Err(RecvTimeoutError::Disconnected) => {
                schedule_persist(app);
                return;
            }
        }
    }
}

/// `save_window_state` holds the plugin cache while querying AppKit window
/// properties. Running it from this debounce worker can invert that lock with
/// the plugin's main-thread resize listener and deadlock during a live resize.
/// Only the timer runs off-thread; the actual snapshot is always taken on the
/// application event loop.
fn schedule_persist<R: Runtime>(app: &AppHandle<R>) {
    let app = app.clone();
    let schedule_handle = app.clone();
    if let Err(error) = schedule_handle.run_on_main_thread(move || persist(&app)) {
        log::warn!("could not schedule the Heya window state for saving: {error}");
    }
}

fn persist<R: Runtime>(app: &AppHandle<R>) {
    if let Err(error) = app.save_window_state(state_flags()) {
        log::warn!("could not save the Heya window state: {error}");
    }
}

fn state_flags() -> StateFlags {
    StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED | StateFlags::FULLSCREEN
}
