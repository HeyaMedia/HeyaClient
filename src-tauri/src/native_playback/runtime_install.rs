//! Local-only MPV runtime discovery and installation.
//!
//! The selected remote Heya origin can query normalized playback capabilities,
//! but only the bootstrap settings WebView can invoke installation. Windows
//! downloads one pinned provider asset, verifies it, extracts only the expected
//! DLL into app data, then gives the runtime shim its absolute path.

use serde::Serialize;
use tauri::{ipc::Channel, AppHandle};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MpvInstallationOffer {
    pub supported: bool,
    pub provider: Option<String>,
    pub release: Option<String>,
    pub download_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum MpvInstallProgress {
    Started {
        provider: String,
        release: String,
        total_bytes: u64,
    },
    Downloading {
        downloaded_bytes: u64,
        total_bytes: u64,
    },
    Verifying,
    Extracting,
    Installed {
        provider: String,
        release: String,
    },
}

#[cfg(any(all(feature = "native-mpv", target_os = "windows"), test))]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod windows {
    use super::{MpvInstallProgress, MpvInstallationOffer};
    use chrono::Utc;
    use reqwest::{
        blocking::Client,
        redirect::{Attempt, Policy},
    };
    use serde::{Deserialize, Serialize};
    use sevenz_rust2::{ArchiveReader, Password};
    use sha2::{Digest, Sha256};
    use std::{
        fs::{self, File},
        io::{self, Read, Write},
        path::{Path, PathBuf},
        sync::atomic::{AtomicBool, Ordering},
    };
    use tauri::{ipc::Channel, AppHandle, Manager};
    use uuid::Uuid;

    #[cfg(target_os = "windows")]
    use std::os::windows::ffi::OsStrExt;

    const PROVIDERS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/native/mpv/providers-v1.json"
    ));
    const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
    static INSTALLING: AtomicBool = AtomicBool::new(false);

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ProviderManifest {
        windows: WindowsProvider,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WindowsProvider {
        provider: String,
        provider_page: String,
        release: String,
        mpv_revision: String,
        maximum_download_bytes: u64,
        assets: ProviderAssets,
    }

    #[derive(Debug, Deserialize)]
    struct ProviderAssets {
        x86_64: ProviderAsset,
        aarch64: ProviderAsset,
    }

    #[derive(Clone, Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ProviderAsset {
        url: String,
        sha256: String,
        download_bytes: u64,
        library: String,
    }

    #[derive(Debug, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ProviderReceipt<'a> {
        schema_version: u8,
        provider: &'a str,
        provider_page: &'a str,
        release: &'a str,
        mpv_revision: &'a str,
        architecture: &'a str,
        archive_sha256: &'a str,
        library: &'a str,
        installed_at: String,
    }

    struct InstallGuard;

    impl InstallGuard {
        fn acquire() -> Result<Self, String> {
            INSTALLING
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| "an MPV installation is already in progress".to_string())?;
            Ok(Self)
        }
    }

    impl Drop for InstallGuard {
        fn drop(&mut self) {
            INSTALLING.store(false, Ordering::Release);
        }
    }

    pub(super) fn offer() -> MpvInstallationOffer {
        match selected_provider() {
            Ok((provider, asset, _)) => MpvInstallationOffer {
                supported: true,
                provider: Some(provider.provider),
                release: Some(provider.release),
                download_bytes: Some(asset.download_bytes),
            },
            Err(_) => MpvInstallationOffer {
                supported: false,
                provider: None,
                release: None,
                download_bytes: None,
            },
        }
    }

    pub(super) fn configure(app: &AppHandle) -> Result<(), String> {
        let (_, asset, architecture) = selected_provider()?;
        let library = runtime_directory(app, architecture)?.join(&asset.library);
        if !library.is_file() {
            return Ok(());
        }
        configure_loader_path(&library)
    }

    pub(super) async fn install(
        app: AppHandle,
        on_event: Channel<MpvInstallProgress>,
    ) -> Result<(), String> {
        let _guard = InstallGuard::acquire()?;
        tauri::async_runtime::spawn_blocking(move || install_blocking(&app, &on_event))
            .await
            .map_err(|error| format!("the MPV installer task failed: {error}"))?
    }

    fn install_blocking(
        app: &AppHandle,
        on_event: &Channel<MpvInstallProgress>,
    ) -> Result<(), String> {
        let (provider, asset, architecture) = selected_provider()?;
        let final_runtime = runtime_directory(app, architecture)?;
        let final_library = final_runtime.join(&asset.library);

        on_event
            .send(MpvInstallProgress::Started {
                provider: provider.provider.clone(),
                release: provider.release.clone(),
                total_bytes: asset.download_bytes,
            })
            .map_err(|error| format!("could not report MPV installation progress: {error}"))?;

        let root = runtime_root(app)?;
        fs::create_dir_all(&root)
            .map_err(|error| format!("could not create the MPV runtime directory: {error}"))?;
        let staging = root.join(format!(".install-{}", Uuid::new_v4()));
        fs::create_dir(&staging)
            .map_err(|error| format!("could not create the MPV staging directory: {error}"))?;
        let cleanup = StagingCleanup(staging.clone());
        let archive_path = staging.join("provider.7z");

        download_provider(&provider, &asset, &archive_path, on_event)?;
        on_event
            .send(MpvInstallProgress::Verifying)
            .map_err(|error| format!("could not report MPV verification progress: {error}"))?;
        verify_sha256(&archive_path, &asset.sha256)?;
        on_event
            .send(MpvInstallProgress::Extracting)
            .map_err(|error| format!("could not report MPV extraction progress: {error}"))?;

        let staged_runtime = staging.join("runtime");
        fs::create_dir(&staged_runtime)
            .map_err(|error| format!("could not create the extracted MPV directory: {error}"))?;
        extract_library(&archive_path, &staged_runtime, &asset.library)?;
        write_receipt(&staged_runtime, &provider, &asset, architecture)?;
        fs::remove_file(&archive_path)
            .map_err(|error| format!("could not remove the verified MPV archive: {error}"))?;

        if let Some(parent) = final_runtime.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create the MPV provider directory: {error}"))?;
        }
        let previous_runtime = staging.join("previous-runtime");
        if final_runtime.exists() {
            fs::rename(&final_runtime, &previous_runtime)
                .map_err(|error| format!("could not replace the previous MPV runtime: {error}"))?;
        }
        if let Err(error) = fs::rename(&staged_runtime, &final_runtime) {
            if previous_runtime.exists() {
                if let Err(restore_error) = fs::rename(&previous_runtime, &final_runtime) {
                    return Err(format!(
                        "could not activate the MPV runtime ({error}) or restore the previous runtime ({restore_error})"
                    ));
                }
            }
            return Err(format!("could not activate the MPV runtime: {error}"));
        }
        configure_loader_path(&final_library)?;
        drop(cleanup);

        on_event
            .send(MpvInstallProgress::Installed {
                provider: provider.provider,
                release: provider.release,
            })
            .map_err(|error| format!("could not report MPV installation completion: {error}"))
    }

    struct StagingCleanup(PathBuf);

    impl Drop for StagingCleanup {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                if error.kind() != io::ErrorKind::NotFound {
                    log::warn!("could not remove MPV staging data: {error}");
                }
            }
        }
    }

    fn selected_provider() -> Result<(WindowsProvider, ProviderAsset, &'static str), String> {
        let manifest: ProviderManifest = serde_json::from_str(PROVIDERS)
            .map_err(|error| format!("the embedded MPV provider manifest is invalid: {error}"))?;
        let (asset, architecture) = match std::env::consts::ARCH {
            "x86_64" => (manifest.windows.assets.x86_64.clone(), "x86_64"),
            "aarch64" => (manifest.windows.assets.aarch64.clone(), "aarch64"),
            architecture => {
                return Err(format!(
                    "MPV installation is unsupported on Windows {architecture}"
                ))
            }
        };
        validate_provider(&manifest.windows, &asset)?;
        Ok((manifest.windows, asset, architecture))
    }

    fn validate_provider(provider: &WindowsProvider, asset: &ProviderAsset) -> Result<(), String> {
        let url = reqwest::Url::parse(&asset.url)
            .map_err(|error| format!("the MPV provider URL is invalid: {error}"))?;
        if !approved_download_url(&url)
            || url.host_str() != Some("github.com")
            || !url
                .path()
                .starts_with("/shinchiro/mpv-winbuild-cmake/releases/download/")
        {
            return Err("the MPV provider URL is outside the approved GitHub release path".into());
        }
        if asset.library != "libmpv-2.dll" {
            return Err("the MPV provider manifest names an unexpected library".into());
        }
        if asset.download_bytes == 0
            || asset.download_bytes > provider.maximum_download_bytes
            || asset.sha256.len() != 64
            || !asset.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("the MPV provider asset limits or digest are invalid".into());
        }
        Ok(())
    }

    fn runtime_root(app: &AppHandle) -> Result<PathBuf, String> {
        app.path()
            .app_local_data_dir()
            .map(|path| path.join("mpv"))
            .map_err(|error| format!("could not resolve Heya's local data directory: {error}"))
    }

    fn runtime_directory(app: &AppHandle, architecture: &str) -> Result<PathBuf, String> {
        let (provider, _, _) = selected_provider()?;
        Ok(runtime_root(app)?
            .join("runtimes")
            .join(format!("{}-{architecture}", provider.release)))
    }

    fn redirect_policy(attempt: Attempt<'_>) -> reqwest::redirect::Action {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many MPV provider redirects");
        }
        if approved_download_url(attempt.url()) {
            attempt.follow()
        } else {
            attempt.error("the MPV provider redirected outside approved GitHub hosts")
        }
    }

    fn approved_download_url(url: &reqwest::Url) -> bool {
        url.scheme() == "https"
            && matches!(
                url.host_str(),
                Some("github.com" | "release-assets.githubusercontent.com")
            )
    }

    fn download_provider(
        provider: &WindowsProvider,
        asset: &ProviderAsset,
        destination: &Path,
        on_event: &Channel<MpvInstallProgress>,
    ) -> Result<(), String> {
        let client = Client::builder()
            .redirect(Policy::custom(redirect_policy))
            .user_agent(concat!("HeyaClient/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("could not initialize the MPV downloader: {error}"))?;
        let mut response = client
            .get(&asset.url)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|error| format!("could not download MPV: {error}"))?;
        if response
            .content_length()
            .is_some_and(|length| length > provider.maximum_download_bytes)
        {
            return Err("the MPV provider download exceeds its declared maximum size".into());
        }

        let mut output = File::create(destination)
            .map_err(|error| format!("could not create the MPV download: {error}"))?;
        let mut downloaded = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = response
                .read(&mut buffer)
                .map_err(|error| format!("the MPV download was interrupted: {error}"))?;
            if read == 0 {
                break;
            }
            downloaded = downloaded.saturating_add(read as u64);
            if downloaded > provider.maximum_download_bytes {
                return Err("the MPV provider download exceeds its declared maximum size".into());
            }
            output
                .write_all(&buffer[..read])
                .map_err(|error| format!("could not save the MPV download: {error}"))?;
            let _ = on_event.send(MpvInstallProgress::Downloading {
                downloaded_bytes: downloaded,
                total_bytes: asset.download_bytes,
            });
        }
        output
            .sync_all()
            .map_err(|error| format!("could not finish the MPV download: {error}"))?;
        Ok(())
    }

    fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
        let mut file = File::open(path)
            .map_err(|error| format!("could not read the MPV download: {error}"))?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| format!("could not verify the MPV download: {error}"))?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        let actual = format!("{:x}", digest.finalize());
        if actual.eq_ignore_ascii_case(expected) {
            Ok(())
        } else {
            Err("the MPV provider download failed SHA-256 verification".into())
        }
    }

    fn extract_library(archive_path: &Path, destination: &Path, name: &str) -> Result<(), String> {
        let file = File::open(archive_path)
            .map_err(|error| format!("could not open the verified MPV archive: {error}"))?;
        let mut archive = ArchiveReader::new(file, Password::empty())
            .map_err(|error| format!("could not parse the verified MPV archive: {error}"))?;
        let unpacked = archive
            .archive()
            .files
            .iter()
            .try_fold(0_u64, |total, entry| {
                total
                    .checked_add(entry.size())
                    .ok_or_else(|| "the MPV archive unpacked size overflowed".to_string())
            })?;
        if unpacked > MAX_UNPACKED_BYTES {
            return Err("the MPV archive exceeds the maximum unpacked size".into());
        }
        let matching_entries = archive
            .archive()
            .files
            .iter()
            .filter(|entry| !entry.is_directory() && entry.name() == name)
            .count();
        if matching_entries != 1 {
            return Err("the MPV archive does not contain exactly one expected library".into());
        }

        let output_path = destination.join(name);
        let mut extracted = false;
        archive
            .for_each_entries(|entry, reader| {
                if !entry.is_directory() && entry.name() == name {
                    let mut output =
                        File::create(&output_path).map_err(sevenz_rust2::Error::from)?;
                    let written =
                        io::copy(reader, &mut output).map_err(sevenz_rust2::Error::from)?;
                    if written != entry.size() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "the MPV library size did not match its archive entry",
                        )
                        .into());
                    }
                    output.sync_all().map_err(sevenz_rust2::Error::from)?;
                    extracted = true;
                } else {
                    io::copy(reader, &mut io::sink()).map_err(sevenz_rust2::Error::from)?;
                }
                Ok(true)
            })
            .map_err(|error| format!("could not extract the verified MPV library: {error}"))?;
        if !extracted || !output_path.is_file() {
            return Err("the verified MPV library was not extracted".into());
        }
        Ok(())
    }

    fn write_receipt(
        destination: &Path,
        provider: &WindowsProvider,
        asset: &ProviderAsset,
        architecture: &str,
    ) -> Result<(), String> {
        let receipt = ProviderReceipt {
            schema_version: 1,
            provider: &provider.provider,
            provider_page: &provider.provider_page,
            release: &provider.release,
            mpv_revision: &provider.mpv_revision,
            architecture,
            archive_sha256: &asset.sha256,
            library: &asset.library,
            installed_at: Utc::now().to_rfc3339(),
        };
        let bytes = serde_json::to_vec_pretty(&receipt)
            .map_err(|error| format!("could not encode the MPV provider receipt: {error}"))?;
        fs::write(destination.join("provider-receipt.json"), bytes)
            .map_err(|error| format!("could not write the MPV provider receipt: {error}"))
    }

    #[cfg(target_os = "windows")]
    fn configure_loader_path(path: &Path) -> Result<(), String> {
        if !path.is_absolute() {
            return Err("the MPV runtime path is not absolute".into());
        }
        let mut path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let accepted = unsafe { heya_mpv_set_runtime_path(path.as_mut_ptr()) };
        if accepted == 1 {
            Ok(())
        } else {
            Err("the MPV runtime loader rejected its app-data path".into())
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn configure_loader_path(path: &Path) -> Result<(), String> {
        if path.is_absolute() {
            Ok(())
        } else {
            Err("the MPV runtime path is not absolute".into())
        }
    }

    #[cfg(target_os = "windows")]
    extern "C" {
        fn heya_mpv_set_runtime_path(path: *const u16) -> i32;
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn embedded_provider_is_pinned_and_valid_for_this_architecture() {
            let (provider, asset, architecture) = selected_provider().unwrap();
            assert_eq!(provider.provider, "shinchiro/mpv-winbuild-cmake");
            assert!(!provider.release.is_empty());
            assert!(matches!(architecture, "x86_64" | "aarch64"));
            validate_provider(&provider, &asset).unwrap();
        }

        #[test]
        fn redirect_policy_rejects_unapproved_hosts() {
            assert!(!approved_download_url(
                &reqwest::Url::parse("https://example.com/libmpv.7z").unwrap()
            ));
            assert!(approved_download_url(
                &reqwest::Url::parse("https://release-assets.githubusercontent.com/file").unwrap()
            ));
        }

        #[test]
        fn extracts_the_pinned_provider_archive_when_ci_supplies_it() {
            let Some(archive) = std::env::var_os("HEYA_MPV_PROVIDER_ARCHIVE").map(PathBuf::from)
            else {
                return;
            };
            let (_, asset, _) = selected_provider().unwrap();
            verify_sha256(&archive, &asset.sha256).unwrap();

            let destination =
                std::env::temp_dir().join(format!("heya-mpv-extraction-test-{}", Uuid::new_v4()));
            fs::create_dir(&destination).unwrap();
            extract_library(&archive, &destination, &asset.library).unwrap();
            assert!(destination.join(&asset.library).is_file());
            fs::remove_dir_all(destination).unwrap();
        }
    }
}

pub fn mpv_installation_offer() -> MpvInstallationOffer {
    #[cfg(all(feature = "native-mpv", target_os = "windows"))]
    return windows::offer();

    #[cfg(not(all(feature = "native-mpv", target_os = "windows")))]
    MpvInstallationOffer {
        supported: false,
        provider: None,
        release: None,
        download_bytes: None,
    }
}

pub fn configure_runtime_loader(app: &AppHandle) -> Result<(), String> {
    #[cfg(all(feature = "native-mpv", target_os = "windows"))]
    return windows::configure(app);

    #[cfg(not(all(feature = "native-mpv", target_os = "windows")))]
    {
        let _ = app;
        Ok(())
    }
}

pub async fn install_mpv_runtime(
    app: AppHandle,
    on_event: Channel<MpvInstallProgress>,
) -> Result<(), String> {
    #[cfg(all(feature = "native-mpv", target_os = "windows"))]
    return windows::install(app, on_event).await;

    #[cfg(not(all(feature = "native-mpv", target_os = "windows")))]
    {
        let _ = (app, on_event);
        Err("MPV installation is not supported on this platform".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_events_use_stable_names() {
        let value = serde_json::to_value(MpvInstallProgress::Verifying).unwrap();
        assert_eq!(value["event"], "verifying");
    }
}
