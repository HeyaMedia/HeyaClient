use chrono::{DateTime, Utc};
use reqwest::{redirect::Policy, Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tauri::Url;

const PROFILE_FILE_NAME: &str = "server-profile.json";
const SETTINGS_FILE_NAME: &str = "app-settings.json";
const MAX_HEALTH_RESPONSE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerProfile {
    /// The origin is the temporary identity until Heya exposes a stable server ID.
    pub id: String,
    pub name: String,
    pub origin: String,
    pub last_connected_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppSettings {
    #[serde(default = "default_reconnect_on_launch")]
    pub reconnect_on_launch: bool,
    #[serde(default = "default_native_playback_enabled")]
    pub native_playback_enabled: bool,
    #[serde(default = "default_native_audio_enabled")]
    pub native_audio_enabled: bool,
    #[serde(default)]
    pub bit_perfect_audio_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_output_device_id: Option<String>,
    #[serde(default)]
    pub track_change_notifications: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            reconnect_on_launch: default_reconnect_on_launch(),
            native_playback_enabled: default_native_playback_enabled(),
            native_audio_enabled: default_native_audio_enabled(),
            bit_perfect_audio_enabled: false,
            audio_output_device_id: None,
            track_change_notifications: false,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    profile: Arc<RwLock<Option<ServerProfile>>>,
    profile_path: Arc<PathBuf>,
    settings: Arc<RwLock<AppSettings>>,
    settings_path: Arc<PathBuf>,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct HealthResponse {
    status: String,
    #[serde(default)]
    version: String,
}

impl AppState {
    pub fn new(config_dir: PathBuf) -> Result<Self, String> {
        let profile_path = config_dir.join(PROFILE_FILE_NAME);
        let profile = match load_profile(&profile_path) {
            Ok(profile) => profile,
            Err(error) => {
                log::warn!("ignoring invalid saved Heya server profile: {error}");
                None
            }
        };
        let settings_path = config_dir.join(SETTINGS_FILE_NAME);
        let settings = match load_settings(&settings_path) {
            Ok(settings) => settings,
            Err(error) => {
                log::warn!("using default Heya app settings: {error}");
                AppSettings::default()
            }
        };

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .redirect(Policy::limited(4))
            .user_agent(format!("HeyaClient/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("could not initialize the Heya connection client: {error}"))?;

        Ok(Self {
            profile: Arc::new(RwLock::new(profile)),
            profile_path: Arc::new(profile_path),
            settings: Arc::new(RwLock::new(settings)),
            settings_path: Arc::new(settings_path),
            client,
        })
    }

    pub fn profile(&self) -> Option<ServerProfile> {
        self.profile
            .read()
            .map(|profile| profile.clone())
            .unwrap_or_else(|error| {
                log::error!("server profile lock was poisoned: {error}");
                None
            })
    }

    pub fn allows_url(&self, url: &Url) -> bool {
        self.profile()
            .and_then(|profile| Url::parse(&profile.origin).ok())
            .is_some_and(|origin| same_origin(&origin, url))
    }

    pub fn settings(&self) -> AppSettings {
        self.settings
            .read()
            .map(|settings| settings.clone())
            .unwrap_or_else(|error| {
                log::error!("app settings lock was poisoned: {error}");
                AppSettings::default()
            })
    }

    pub fn save_settings(&self, settings: AppSettings) -> Result<AppSettings, String> {
        save_settings(&self.settings_path, &settings)?;
        *self
            .settings
            .write()
            .map_err(|error| format!("could not update the app settings: {error}"))? =
            settings.clone();
        Ok(settings)
    }

    pub async fn validate_and_store(&self, input: &str) -> Result<ServerProfile, String> {
        let requested_origin = normalize_origin(input)?;
        let health_url = requested_origin
            .join("api/health")
            .map_err(|error| format!("could not create the Heya health URL: {error}"))?;

        let response = self
            .client
            .get(health_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| connection_error(&requested_origin, &error))?;

        if response.status() != StatusCode::OK {
            return Err(format!(
                "{} returned HTTP {} from /api/health.",
                display_origin(&requested_origin),
                response.status().as_u16()
            ));
        }

        if response
            .content_length()
            .is_some_and(|length| length > MAX_HEALTH_RESPONSE_BYTES)
        {
            return Err("The /api/health response was unexpectedly large.".to_string());
        }

        let response_url = response.url().clone();
        let health = response
            .json::<HealthResponse>()
            .await
            .map_err(|_| "The server responded, but it did not look like Heya.".to_string())?;

        if health.status != "ok" {
            return Err(format!(
                "The Heya server reported status {:?}.",
                health.status
            ));
        }

        let final_origin = origin_from_url(&response_url)?;
        if requested_origin.scheme() == "https" && final_origin.scheme() != "https" {
            return Err("The server redirected from HTTPS to insecure HTTP.".to_string());
        }

        let origin = final_origin.to_string();
        let profile = ServerProfile {
            id: origin.clone(),
            name: profile_name(&final_origin),
            origin,
            last_connected_at: Some(Utc::now()),
            server_version: (!health.version.is_empty()).then_some(health.version),
        };

        save_profile(&self.profile_path, &profile)?;
        self.profile
            .write()
            .map_err(|error| format!("could not update the active server profile: {error}"))?
            .replace(profile.clone());

        Ok(profile)
    }

    pub fn forget(&self) -> Result<(), String> {
        if let Err(error) = fs::remove_file(self.profile_path.as_ref()) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(format!(
                    "could not remove the saved server profile: {error}"
                ));
            }
        }

        self.profile
            .write()
            .map_err(|error| format!("could not clear the active server profile: {error}"))?
            .take();
        Ok(())
    }
}

fn default_reconnect_on_launch() -> bool {
    true
}

fn default_native_playback_enabled() -> bool {
    true
}

fn default_native_audio_enabled() -> bool {
    true
}

pub fn normalize_origin(input: &str) -> Result<Url, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("Enter a Heya server address.".to_string());
    }

    let candidate = if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    let url = Url::parse(&candidate).map_err(|_| "Enter a valid server address.".to_string())?;
    origin_from_url(&url)
}

pub fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn origin_from_url(url: &Url) -> Result<Url, String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Heya server addresses must use HTTP or HTTPS.".to_string());
    }
    if url.host_str().is_none() {
        return Err("The Heya server address needs a host name or IP address.".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Do not include a username or password in the server address.".to_string());
    }

    let mut origin = url.clone();
    origin.set_path("/");
    origin.set_query(None);
    origin.set_fragment(None);
    Ok(origin)
}

fn profile_name(origin: &Url) -> String {
    let host = origin.host_str().unwrap_or("Heya server");
    match origin.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

fn display_origin(origin: &Url) -> String {
    origin.as_str().trim_end_matches('/').to_string()
}

fn connection_error(origin: &Url, error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return format!("Timed out while connecting to {}.", display_origin(origin));
    }

    format!("Could not reach {}: {}", display_origin(origin), error)
}

fn load_profile(path: &Path) -> Result<Option<ServerProfile>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };

    let mut profile: ServerProfile = serde_json::from_slice(&bytes)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    profile.origin = normalize_origin(&profile.origin)?.to_string();
    Ok(Some(profile))
}

fn load_settings(path: &Path) -> Result<AppSettings, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AppSettings::default());
        }
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };

    serde_json::from_slice(&bytes)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))
}

fn save_profile(path: &Path, profile: &ServerProfile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }

    let bytes = serde_json::to_vec_pretty(profile)
        .map_err(|error| format!("could not serialize the server profile: {error}"))?;
    fs::write(path, bytes).map_err(|error| format!("could not save the server profile: {error}"))
}

fn save_settings(path: &Path, settings: &AppSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }

    let bytes = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("could not serialize the app settings: {error}"))?;
    fs::write(path, bytes).map_err(|error| format!("could not save the app settings: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{normalize_origin, same_origin, AppSettings};
    use tauri::Url;

    #[test]
    fn normalizes_to_an_https_origin() {
        assert_eq!(
            normalize_origin("heya.example.com/library?sort=title")
                .unwrap()
                .as_str(),
            "https://heya.example.com/"
        );
    }

    #[test]
    fn preserves_explicit_http_and_ports() {
        assert_eq!(
            normalize_origin("http://192.168.1.20:8080/login")
                .unwrap()
                .as_str(),
            "http://192.168.1.20:8080/"
        );
    }

    #[test]
    fn rejects_credentials_and_non_web_schemes() {
        assert!(normalize_origin("https://user:secret@heya.example.com").is_err());
        assert!(normalize_origin("file:///tmp/heya").is_err());
    }

    #[test]
    fn compares_scheme_host_and_effective_port() {
        let origin = Url::parse("https://heya.example.com/").unwrap();
        assert!(same_origin(
            &origin,
            &Url::parse("https://heya.example.com/movies/42").unwrap()
        ));
        assert!(!same_origin(
            &origin,
            &Url::parse("http://heya.example.com/").unwrap()
        ));
        assert!(!same_origin(
            &origin,
            &Url::parse("https://cdn.heya.example.com/").unwrap()
        ));
    }

    #[test]
    fn defaults_to_reconnecting_on_launch() {
        let settings: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(settings.reconnect_on_launch);
        assert!(settings.native_playback_enabled);
        assert!(settings.native_audio_enabled);
        assert!(!settings.bit_perfect_audio_enabled);
        assert_eq!(settings.audio_output_device_id, None);
        assert!(!settings.track_change_notifications);
        assert_eq!(settings, AppSettings::default());
    }

    #[test]
    fn preserves_an_explicit_native_playback_preference() {
        let settings: AppSettings =
            serde_json::from_str(r#"{"reconnect_on_launch":true,"native_playback_enabled":false}"#)
                .unwrap();
        assert!(!settings.native_playback_enabled);
    }

    #[test]
    fn preserves_the_selected_audio_output_device() {
        let settings: AppSettings =
            serde_json::from_str(r#"{"audio_output_device_id":"stable-device-id"}"#).unwrap();
        assert_eq!(
            settings.audio_output_device_id.as_deref(),
            Some("stable-device-id")
        );
    }
}
