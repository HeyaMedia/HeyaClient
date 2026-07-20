use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Mutex,
};
use tauri::AppHandle;
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
    pub(crate) fn status(&self, app: &AppHandle) -> UpdateStatus {
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

    pub(crate) async fn check(&self, app: &AppHandle) -> Result<UpdateStatus, String> {
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

    async fn install_with_reporter<F>(&self, app: AppHandle, report: F) -> Result<(), String>
    where
        F: Fn(UpdateProgress) + Clone + Send + Sync + 'static,
    {
        let _guard = OperationGuard::acquire(&self.installing, "installation")?;
        if self.checking.load(Ordering::Acquire) {
            return Err("an update check is still in progress".to_string());
        }

        let update = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| "check for an update before installing".to_string())?;
        let version = update.version.clone();
        let downloaded = AtomicU64::new(0);
        report(UpdateProgress::Started {
            version: version.clone(),
        });

        let progress_reporter = report.clone();
        let finished_reporter = report.clone();
        update
            .download_and_install(
                |chunk_length, total_bytes| {
                    let downloaded_bytes = downloaded
                        .fetch_add(chunk_length as u64, Ordering::Relaxed)
                        .saturating_add(chunk_length as u64);
                    progress_reporter(UpdateProgress::Downloading {
                        downloaded_bytes,
                        total_bytes,
                    });
                },
                move || finished_reporter(UpdateProgress::DownloadFinished),
            )
            .await
            .map_err(|error| format!("could not install Heya {version}: {error}"))?;

        *self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        report(UpdateProgress::Installed {
            version: version.clone(),
        });

        #[cfg(not(target_os = "windows"))]
        app.restart();

        #[cfg(target_os = "windows")]
        Ok(())
    }

    pub(crate) async fn install_silent(&self, app: AppHandle) -> Result<(), String> {
        self.install_with_reporter(app, |_| {}).await
    }
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
