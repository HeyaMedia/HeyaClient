use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Mutex,
};
#[cfg(not(debug_assertions))]
use tauri::Manager;
use tauri::{ipc::Channel, AppHandle, State, WebviewWindow};
use tauri_plugin_updater::{Update, UpdaterExt};

#[derive(Default)]
pub struct AppUpdater {
    pending: Mutex<Option<Update>>,
    checking: AtomicBool,
    installing: AtomicBool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    current_version: String,
    available: bool,
    version: Option<String>,
    notes: Option<String>,
    published_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum UpdateProgress {
    Started {
        version: String,
    },
    Downloading {
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    },
    DownloadFinished,
    Installed {
        version: String,
    },
}

struct OperationGuard<'a>(&'a AtomicBool);

impl<'a> OperationGuard<'a> {
    fn acquire(flag: &'a AtomicBool, operation: &str) -> Result<Self, String> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| format!("an update {operation} is already in progress"))?;
        Ok(Self(flag))
    }
}

impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl AppUpdater {
    fn status(&self, app: &AppHandle) -> UpdateStatus {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        UpdateStatus {
            current_version: app.package_info().version.to_string(),
            available: pending.is_some(),
            version: pending.as_ref().map(|update| update.version.clone()),
            notes: pending.as_ref().and_then(|update| update.body.clone()),
            published_at: pending
                .as_ref()
                .and_then(|update| update.date.map(|date| date.to_string())),
        }
    }

    async fn check(&self, app: &AppHandle) -> Result<UpdateStatus, String> {
        let _guard = OperationGuard::acquire(&self.checking, "check")?;
        if self.installing.load(Ordering::Acquire) {
            return Err("an update installation is already in progress".to_string());
        }

        let update = app
            .updater()
            .map_err(|error| format!("could not initialize the updater: {error}"))?
            .check()
            .await
            .map_err(|error| format!("could not check for updates: {error}"))?;
        *self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = update;
        Ok(self.status(app))
    }
}

#[tauri::command]
pub fn get_update_status(
    app: AppHandle,
    invoking_window: WebviewWindow,
    updater: State<'_, AppUpdater>,
) -> Result<UpdateStatus, String> {
    ensure_settings_caller(&invoking_window)?;
    Ok(updater.status(&app))
}

#[tauri::command]
pub async fn check_for_update(
    app: AppHandle,
    invoking_window: WebviewWindow,
    updater: State<'_, AppUpdater>,
) -> Result<UpdateStatus, String> {
    ensure_settings_caller(&invoking_window)?;
    updater.check(&app).await
}

#[tauri::command]
pub async fn install_update(
    _app: AppHandle,
    invoking_window: WebviewWindow,
    updater: State<'_, AppUpdater>,
    on_event: Channel<UpdateProgress>,
) -> Result<(), String> {
    ensure_settings_caller(&invoking_window)?;
    let _guard = OperationGuard::acquire(&updater.installing, "installation")?;
    if updater.checking.load(Ordering::Acquire) {
        return Err("an update check is still in progress".to_string());
    }

    let update = updater
        .pending
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
        .ok_or_else(|| "check for an update before installing".to_string())?;
    let version = update.version.clone();
    let downloaded = AtomicU64::new(0);
    on_event
        .send(UpdateProgress::Started {
            version: version.clone(),
        })
        .map_err(|error| format!("could not report update progress: {error}"))?;

    let progress_channel = on_event.clone();
    let finished_channel = on_event.clone();
    update
        .download_and_install(
            |chunk_length, total_bytes| {
                let downloaded_bytes = downloaded
                    .fetch_add(chunk_length as u64, Ordering::Relaxed)
                    .saturating_add(chunk_length as u64);
                let _ = progress_channel.send(UpdateProgress::Downloading {
                    downloaded_bytes,
                    total_bytes,
                });
            },
            move || {
                let _ = finished_channel.send(UpdateProgress::DownloadFinished);
            },
        )
        .await
        .map_err(|error| format!("could not install Heya {version}: {error}"))?;

    *updater
        .pending
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = None;
    let _ = on_event.send(UpdateProgress::Installed {
        version: version.clone(),
    });

    #[cfg(not(target_os = "windows"))]
    _app.restart();

    #[cfg(target_os = "windows")]
    Ok(())
}

fn ensure_settings_caller(window: &WebviewWindow) -> Result<(), String> {
    crate::navigation::ensure_local_settings_window(window, "the updater")
}

pub fn check_on_startup(app: AppHandle) {
    #[cfg(not(debug_assertions))]
    tauri::async_runtime::spawn(async move {
        let updater = app.state::<AppUpdater>();
        match updater.check(&app).await {
            Ok(status) if status.available => {
                log::info!(
                    "Heya {} is available; opening client settings",
                    status.version.as_deref().unwrap_or("update")
                );
                crate::navigation::request_settings(&app);
            }
            Ok(_) => log::info!("HeyaClient is up to date"),
            Err(error) => log::warn!("automatic update check failed: {error}"),
        }
    });

    #[cfg(debug_assertions)]
    let _ = app;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_guard_rejects_overlap_and_releases_on_drop() {
        let flag = AtomicBool::new(false);
        let guard = OperationGuard::acquire(&flag, "check").unwrap();
        assert!(OperationGuard::acquire(&flag, "check").is_err());
        drop(guard);
        assert!(OperationGuard::acquire(&flag, "check").is_ok());
    }

    #[test]
    fn progress_events_use_stable_snake_case_names() {
        let value = serde_json::to_value(UpdateProgress::Downloading {
            downloaded_bytes: 128,
            total_bytes: Some(256),
        })
        .unwrap();
        assert_eq!(value["event"], "downloading");
        assert_eq!(value["downloaded_bytes"], 128);
        assert_eq!(value["total_bytes"], 256);
    }
}
