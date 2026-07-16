import { Channel, invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

interface ServerProfile {
  id: string;
  name: string;
  origin: string;
  last_connected_at: string | null;
  server_version: string | null;
}

interface AppSettings {
  reconnect_on_launch: boolean;
  native_playback_enabled: boolean;
  native_audio_enabled: boolean;
  bit_perfect_audio_enabled: boolean;
  audio_output_device_id: string | null;
  track_change_notifications: boolean;
}

interface NativePlaybackStatus {
  backend: "mpv";
  available: boolean;
  build_includes_native_mpv: boolean;
  video_surface: "native-window" | "native-surface";
  unavailable_reason: string | null;
  installation: {
    supported: boolean;
    provider: string | null;
    release: string | null;
    downloadBytes: number | null;
  };
}

type MpvInstallProgress =
  | {
      event: "started";
      provider: string;
      release: string;
      total_bytes: number;
    }
  | {
      event: "downloading";
      downloaded_bytes: number;
      total_bytes: number;
    }
  | { event: "verifying" }
  | { event: "extracting" }
  | { event: "installed"; provider: string; release: string };

interface NativeAudioStatus {
  backend: "heya-rust-audio";
  available: boolean;
  gapless: boolean;
  crossfade: boolean;
  bit_perfect_available: boolean;
  bit_perfect_unavailable_reason: string | null;
}

interface UpdateStatus {
  currentVersion: string;
  available: boolean;
  version: string | null;
  notes: string | null;
  publishedAt: string | null;
}

type UpdateProgress =
  | { event: "started"; version: string }
  | {
      event: "downloading";
      downloaded_bytes: number;
      total_bytes: number | null;
    }
  | { event: "download_finished" }
  | { event: "installed"; version: string };

type StatusKind = "idle" | "checking" | "error" | "success";

const form = requiredElement<HTMLFormElement>("server-form");
const input = requiredElement<HTMLInputElement>("server-origin");
const connectButton = requiredElement<HTMLButtonElement>("connect-button");
const forgetButton = requiredElement<HTMLButtonElement>("forget-button");
const forgetSettingsButton = requiredElement<HTMLButtonElement>(
  "forget-settings-button",
);
const resetSessionButton = requiredElement<HTMLButtonElement>(
  "reset-session-button",
);
const reconnectOnLaunch = requiredElement<HTMLInputElement>(
  "reconnect-on-launch",
);
const nativePlaybackEnabled = requiredElement<HTMLInputElement>(
  "native-playback-enabled",
);
const nativeAudioEnabled = requiredElement<HTMLInputElement>(
  "native-audio-enabled",
);
const bitPerfectAudioEnabled = requiredElement<HTMLInputElement>(
  "bit-perfect-audio-enabled",
);
const trackChangeNotifications = requiredElement<HTMLInputElement>(
  "track-change-notifications",
);
const nativePlaybackStatus = requiredElement<HTMLDivElement>(
  "native-playback-status",
);
const nativePlaybackStatusTitle = requiredElement<HTMLElement>(
  "native-playback-status-title",
);
const nativePlaybackStatusDetail = requiredElement<HTMLElement>(
  "native-playback-status-detail",
);
const refreshNativePlaybackButton = requiredElement<HTMLButtonElement>(
  "refresh-native-playback-button",
);
const nativePlaybackInstallActions = requiredElement<HTMLDivElement>(
  "native-playback-install-actions",
);
const installNativePlaybackButton = requiredElement<HTMLButtonElement>(
  "install-native-playback-button",
);
const nativePlaybackInstallDetail = requiredElement<HTMLElement>(
  "native-playback-install-detail",
);
const nativeAudioStatus = requiredElement<HTMLDivElement>("native-audio-status");
const nativeAudioStatusTitle = requiredElement<HTMLElement>(
  "native-audio-status-title",
);
const nativeAudioStatusDetail = requiredElement<HTMLElement>(
  "native-audio-status-detail",
);
const refreshNativeAudioButton = requiredElement<HTMLButtonElement>(
  "refresh-native-audio-button",
);
const updateStatus = requiredElement<HTMLDivElement>("update-status");
const updateStatusTitle = requiredElement<HTMLElement>("update-status-title");
const updateStatusDetail = requiredElement<HTMLElement>("update-status-detail");
const checkUpdateButton = requiredElement<HTMLButtonElement>(
  "check-update-button",
);
const installUpdateButton = requiredElement<HTMLButtonElement>(
  "install-update-button",
);
const updateActions = requiredElement<HTMLDivElement>("update-actions");
const updateProgress = requiredElement<HTMLDivElement>("update-progress");
const updateProgressBar = requiredElement<HTMLProgressElement>(
  "update-progress-bar",
);
const updateProgressLabel = requiredElement<HTMLElement>(
  "update-progress-label",
);
const savedActions = requiredElement<HTMLDivElement>("saved-actions");
const httpWarning = requiredElement<HTMLParagraphElement>("http-warning");
const status = requiredElement<HTMLDivElement>("status");
const statusTitle = requiredElement<HTMLElement>("status-title");
const statusDetail = requiredElement<HTMLParagraphElement>("status-detail");
const windowChrome = requiredElement<HTMLDivElement>("window-chrome");
const windowDragRegion = requiredElement<HTMLDivElement>("window-drag-region");
const windowClose = requiredElement<HTMLButtonElement>("window-close");
const windowMinimize = requiredElement<HTMLButtonElement>("window-minimize");
const windowMaximize = requiredElement<HTMLButtonElement>("window-maximize");

let savedProfile: ServerProfile | null = null;
let appSettings: AppSettings = {
  reconnect_on_launch: true,
  native_playback_enabled: true,
  native_audio_enabled: true,
  bit_perfect_audio_enabled: false,
  audio_output_device_id: null,
  track_change_notifications: false,
};
let busy = false;
let savingPreferences = false;
let updateBusy = false;
const searchParams = new URLSearchParams(window.location.search);
const isSettingsPage = searchParams.has("settings");
const platform = navigator.userAgent.includes("Windows")
  ? "windows"
  : navigator.userAgent.includes("Mac")
    ? "macos"
    : "linux";
const usesCustomWindowChrome = !isSettingsPage && platform !== "macos";

document.body.dataset.page = isSettingsPage ? "settings" : "connect";
document.documentElement.dataset.windowChrome = usesCustomWindowChrome ? "custom" : "native";
document.body.dataset.platform = platform;
windowChrome.hidden = !usesCustomWindowChrome;
document.querySelectorAll<HTMLElement>(".connect-only").forEach((element) => {
  element.hidden = isSettingsPage;
});
document.querySelectorAll<HTMLElement>(".settings-only").forEach((element) => {
  element.hidden = !isSettingsPage;
});

if (usesCustomWindowChrome) {
  const appWindow = getCurrentWindow();
  windowDragRegion.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    event.preventDefault();
    if (event.detail === 2) void appWindow.toggleMaximize();
    else void appWindow.startDragging();
  });
  windowClose.addEventListener("click", () => void appWindow.close());
  windowMinimize.addEventListener("click", () => void appWindow.minimize());
  windowMaximize.addEventListener("click", () => void appWindow.toggleMaximize());
}

form.addEventListener("submit", (event) => {
  event.preventDefault();
  void submitServer();
});

input.addEventListener("input", () => {
  updateHttpWarning();
  if (status.dataset.kind === "error") {
    setStatus("idle", "Ready to connect", "HTTPS is recommended.");
  }
});

forgetButton.addEventListener("click", () => {
  void forgetServer();
});

forgetSettingsButton.addEventListener("click", () => {
  void forgetServer();
});

resetSessionButton.addEventListener("click", () => {
  void resetServerSession();
});

reconnectOnLaunch.addEventListener("change", () => {
  void savePreferences();
});

nativePlaybackEnabled.addEventListener("change", () => {
  void savePreferences();
});

nativeAudioEnabled.addEventListener("change", () => {
  void savePreferences();
});

bitPerfectAudioEnabled.addEventListener("change", () => {
  void savePreferences();
});

trackChangeNotifications.addEventListener("change", () => {
  void savePreferences();
});

refreshNativePlaybackButton.addEventListener("click", () => {
  void refreshNativePlaybackStatus();
});

installNativePlaybackButton.addEventListener("click", () => {
  void installNativePlaybackRuntime();
});

refreshNativeAudioButton.addEventListener("click", () => {
  void refreshNativeAudioStatus();
});

checkUpdateButton.addEventListener("click", () => {
  void checkForUpdate();
});

installUpdateButton.addEventListener("click", () => {
  void installAvailableUpdate();
});

void initialize();

async function initialize(): Promise<void> {
  try {
    [savedProfile, appSettings] = await Promise.all([
      invoke<ServerProfile | null>("get_server_profile"),
      invoke<AppSettings>("get_app_settings"),
    ]);
  } catch (error) {
    setStatus(
      "error",
      "Couldn’t read the saved server",
      errorMessage(error),
    );
    return;
  }

  reconnectOnLaunch.checked = appSettings.reconnect_on_launch;
  nativePlaybackEnabled.checked = appSettings.native_playback_enabled;
  nativeAudioEnabled.checked = appSettings.native_audio_enabled;
  bitPerfectAudioEnabled.checked = appSettings.bit_perfect_audio_enabled;
  trackChangeNotifications.checked = appSettings.track_change_notifications;
  updateSavedActionAvailability();
  void refreshNativePlaybackStatus();
  void refreshNativeAudioStatus();
  if (isSettingsPage) void refreshUpdateStatus();

  if (!savedProfile) {
    if (isSettingsPage) {
      connectButton.textContent = "Save & open";
      setStatus("idle", "No server saved", "Enter a Heya server to connect this app.");
    }
    input.focus();
    return;
  }

  input.value = savedProfile.origin;
  savedActions.hidden = false;
  updateHttpWarning();

  if (isSettingsPage) {
    connectButton.textContent = "Save & open";
    setStatus(
      "idle",
      `Connected server: ${savedProfile.name}`,
      profileDetail(savedProfile),
    );
    input.select();
    return;
  }

  const manualSelection = searchParams.has("manual");
  if (manualSelection || !appSettings.reconnect_on_launch) {
    setStatus(
      "idle",
      `Saved server: ${savedProfile.name}`,
      profileDetail(savedProfile),
    );
    input.select();
    return;
  }

  await connect(savedProfile.origin, true);
}

async function submitServer(): Promise<void> {
  if (isSettingsPage && !(await savePreferences(false))) return;
  await connect(input.value);
}

async function connect(origin: string, reconnecting = false): Promise<void> {
  if (busy) return;

  const trimmedOrigin = origin.trim();
  if (!trimmedOrigin) {
    setStatus("error", "Enter a server address", "For example: https://heya.example.com");
    input.focus();
    return;
  }

  setBusy(true);
  setStatus(
    "checking",
    reconnecting ? "Reconnecting to Heya…" : "Checking your Heya server…",
    "Looking for Heya’s health endpoint.",
  );

  try {
    savedProfile = await invoke<ServerProfile>("connect_to_server", {
      origin: trimmedOrigin,
    });
    setStatus(
      "success",
      `Connected to ${savedProfile.name}`,
      "Opening your server…",
    );
  } catch (error) {
    savedActions.hidden = savedProfile === null;
    setStatus("error", "Couldn’t connect", errorMessage(error));
    connectButton.textContent = "Retry";
    input.focus();
  } finally {
    setBusy(false);
  }
}

async function forgetServer(): Promise<void> {
  if (busy || !savedProfile) return;
  if (
    !window.confirm(
      `Forget ${savedProfile.origin} and clear its login session from this app?`,
    )
  ) {
    return;
  }

  setBusy(true);
  try {
    await invoke("forget_server");
    savedProfile = null;
    input.value = "";
    savedActions.hidden = true;
    updateSavedActionAvailability();
    updateHttpWarning();
    setStatus("idle", "Server forgotten", "Enter another Heya server address.");
    input.focus();
  } catch (error) {
    setStatus("error", "Couldn’t forget the server", errorMessage(error));
  } finally {
    setBusy(false);
  }
}

async function resetServerSession(): Promise<void> {
  if (busy || !savedProfile) return;
  if (
    !window.confirm(
      `Clear the login session for ${savedProfile.origin}? You’ll need to sign in again.`,
    )
  ) {
    return;
  }

  setBusy(true);
  setStatus(
    "checking",
    "Resetting the login session…",
    "Clearing cookies, cache, and browser storage from this app.",
  );
  try {
    await invoke("reset_server_session");
  } catch (error) {
    setStatus("error", "Couldn’t reset the session", errorMessage(error));
    setBusy(false);
  }
}

async function savePreferences(showConfirmation = true): Promise<boolean> {
  if (busy || savingPreferences) return false;

  const previousSettings = appSettings;
  const nextSettings: AppSettings = {
    reconnect_on_launch: reconnectOnLaunch.checked,
    native_playback_enabled: nativePlaybackEnabled.checked,
    native_audio_enabled: nativeAudioEnabled.checked,
    bit_perfect_audio_enabled: bitPerfectAudioEnabled.checked,
    audio_output_device_id: appSettings.audio_output_device_id,
    track_change_notifications: trackChangeNotifications.checked,
  };
  setPreferencesSaving(true);
  try {
    appSettings = await invoke<AppSettings>("save_app_settings", {
      settings: nextSettings,
    });
    if (showConfirmation) {
      const launchDetail = appSettings.reconnect_on_launch
        ? "The saved server opens at launch."
        : "The connection screen opens at launch.";
      const playbackDetail = appSettings.native_playback_enabled
        ? "Native MPV may be used for new playback sessions."
        : "Video will use browser playback.";
      const audioDetail = appSettings.native_audio_enabled
        ? "Music will prefer the native Rust engine."
        : "Music will use browser playback.";
      const notificationDetail = appSettings.track_change_notifications
        ? "Track-change notifications are enabled while HeyaClient is in the background."
        : "Track-change notifications are disabled.";
      setStatus(
        "success",
        "Preferences saved",
        `${launchDetail} ${playbackDetail} ${audioDetail} ${notificationDetail}`,
      );
    }
    return true;
  } catch (error) {
    reconnectOnLaunch.checked = previousSettings.reconnect_on_launch;
    nativePlaybackEnabled.checked = previousSettings.native_playback_enabled;
    nativeAudioEnabled.checked = previousSettings.native_audio_enabled;
    bitPerfectAudioEnabled.checked = previousSettings.bit_perfect_audio_enabled;
    trackChangeNotifications.checked = previousSettings.track_change_notifications;
    setStatus("error", "Couldn’t save the preference", errorMessage(error));
    return false;
  } finally {
    setPreferencesSaving(false);
  }
}

function setPreferencesSaving(value: boolean): void {
  savingPreferences = value;
  const disabled = busy || savingPreferences;
  reconnectOnLaunch.disabled = disabled;
  nativePlaybackEnabled.disabled = disabled;
  nativeAudioEnabled.disabled = disabled;
  bitPerfectAudioEnabled.disabled = disabled || bitPerfectAudioEnabled.dataset.available !== "true";
  trackChangeNotifications.disabled = disabled;
}

function setBusy(value: boolean): void {
  busy = value;
  input.disabled = value;
  connectButton.disabled = value;
  forgetButton.disabled = value;
  forgetSettingsButton.disabled = value || savedProfile === null;
  resetSessionButton.disabled = value || savedProfile === null;
  setPreferencesSaving(savingPreferences);
  refreshNativePlaybackButton.disabled = value;
  refreshNativeAudioButton.disabled = value;
  checkUpdateButton.disabled = value || updateBusy;
  installUpdateButton.disabled = value || updateBusy;
  if (value) connectButton.textContent = "Connecting…";
  else if (connectButton.textContent === "Connecting…") {
    connectButton.textContent = isSettingsPage ? "Save & open" : "Connect";
  }
}

async function refreshUpdateStatus(): Promise<void> {
  try {
    renderUpdateStatus(await invoke<UpdateStatus>("get_update_status"));
  } catch (error) {
    updateStatus.dataset.available = "false";
    updateStatusTitle.textContent = "Couldn’t read update status";
    updateStatusDetail.textContent = errorMessage(error);
  }
}

async function checkForUpdate(): Promise<void> {
  if (busy || updateBusy) return;
  setUpdateBusy(true);
  updateStatus.dataset.available = "checking";
  updateStatusTitle.textContent = "Checking for updates…";
  updateStatusDetail.textContent = "Contacting the HeyaClient release feed.";
  updateActions.hidden = true;

  try {
    renderUpdateStatus(await invoke<UpdateStatus>("check_for_update"));
  } catch (error) {
    updateStatus.dataset.available = "false";
    updateStatusTitle.textContent = "Couldn’t check for updates";
    updateStatusDetail.textContent = errorMessage(error);
  } finally {
    setUpdateBusy(false);
  }
}

async function installAvailableUpdate(): Promise<void> {
  if (busy || updateBusy) return;
  setUpdateBusy(true);
  updateProgress.hidden = false;
  updateProgressBar.removeAttribute("value");
  updateProgressLabel.textContent = "Preparing update…";

  const onEvent = new Channel<UpdateProgress>();
  onEvent.onmessage = (event) => {
    if (event.event === "started") {
      updateProgressLabel.textContent = `Downloading Heya ${event.version}…`;
      return;
    }
    if (event.event === "downloading") {
      if (event.total_bytes && event.total_bytes > 0) {
        const percent = Math.min(
          100,
          Math.round((event.downloaded_bytes / event.total_bytes) * 100),
        );
        updateProgressBar.value = percent;
        updateProgressLabel.textContent = `${percent}% downloaded`;
      } else {
        updateProgressBar.removeAttribute("value");
        updateProgressLabel.textContent = `${formatBytes(event.downloaded_bytes)} downloaded`;
      }
      return;
    }
    if (event.event === "download_finished") {
      updateProgressBar.value = 100;
      updateProgressLabel.textContent = "Installing update…";
      return;
    }
    updateProgressLabel.textContent = `Heya ${event.version} installed. Restarting…`;
  };

  try {
    await invoke("install_update", { onEvent });
  } catch (error) {
    updateStatus.dataset.available = "false";
    updateStatusTitle.textContent = "Couldn’t install the update";
    updateStatusDetail.textContent = errorMessage(error);
    updateProgress.hidden = true;
    setUpdateBusy(false);
  }
}

function renderUpdateStatus(update: UpdateStatus): void {
  updateProgress.hidden = true;
  if (update.available && update.version) {
    updateStatus.dataset.available = "checking";
    updateStatusTitle.textContent = `Heya ${update.version} is available`;
    updateStatusDetail.textContent = `Installed version: ${update.currentVersion}.`;
    updateActions.hidden = false;
    installUpdateButton.textContent = `Install Heya ${update.version}`;
    return;
  }

  updateStatus.dataset.available = "true";
  updateStatusTitle.textContent = "HeyaClient is up to date";
  updateStatusDetail.textContent = `Installed version: ${update.currentVersion}.`;
  updateActions.hidden = true;
}

function setUpdateBusy(value: boolean): void {
  updateBusy = value;
  checkUpdateButton.disabled = busy || value;
  installUpdateButton.disabled = busy || value;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

async function refreshNativeAudioStatus(): Promise<void> {
  nativeAudioStatus.dataset.available = "checking";
  nativeAudioStatusTitle.textContent = "Checking native audio…";
  nativeAudioStatusDetail.textContent = "Inspecting the Rust audio backend.";
  refreshNativeAudioButton.disabled = true;

  try {
    const audio = await invoke<NativeAudioStatus>("get_native_audio_status");
    nativeAudioStatus.dataset.available = String(audio.available);
    bitPerfectAudioEnabled.dataset.available = String(audio.bit_perfect_available);
    bitPerfectAudioEnabled.disabled = busy || savingPreferences || !audio.bit_perfect_available;
    if (!audio.bit_perfect_available && appSettings.bit_perfect_audio_enabled) {
      bitPerfectAudioEnabled.checked = false;
    }
    if (audio.available) {
      nativeAudioStatusTitle.textContent = "Native music is available";
      nativeAudioStatusDetail.textContent = audio.bit_perfect_available
        ? "Gapless, crossfade, DSP, and exclusive bit-perfect output are available."
        : `Gapless, crossfade, and DSP are ready. ${audio.bit_perfect_unavailable_reason ?? "Bit-perfect output is unavailable."}`;
    } else {
      nativeAudioStatusTitle.textContent = "Native music is unavailable";
      nativeAudioStatusDetail.textContent = "Browser music playback remains available.";
    }
  } catch (error) {
    nativeAudioStatus.dataset.available = "false";
    nativeAudioStatusTitle.textContent = "Couldn’t check native audio";
    nativeAudioStatusDetail.textContent = errorMessage(error);
    bitPerfectAudioEnabled.dataset.available = "false";
    bitPerfectAudioEnabled.disabled = true;
  } finally {
    refreshNativeAudioButton.disabled = busy;
  }
}

async function refreshNativePlaybackStatus(): Promise<void> {
  nativePlaybackStatus.dataset.available = "checking";
  nativePlaybackStatusTitle.textContent = "Checking for MPV…";
  nativePlaybackStatusDetail.textContent = "Testing the native playback backend.";
  refreshNativePlaybackButton.disabled = true;
  nativePlaybackInstallActions.hidden = true;

  try {
    const playback = await invoke<NativePlaybackStatus>(
      "get_native_playback_status",
    );
    nativePlaybackStatus.dataset.available = String(playback.available);
    if (playback.available) {
      nativePlaybackStatusTitle.textContent = "MPV is available";
      nativePlaybackStatusDetail.textContent =
        playback.video_surface === "native-surface"
          ? "Native video can render inside the Heya player."
          : "Native video can render in an MPV window.";
    } else {
      nativePlaybackStatusTitle.textContent = "MPV was not found";
      if (playback.installation.supported) {
        nativePlaybackStatusDetail.textContent =
          "Install the verified MPV runtime for native playback, or continue using browser video.";
        nativePlaybackInstallActions.hidden = false;
        nativePlaybackInstallDetail.textContent = `${playback.installation.provider ?? "MPV"} · ${formatBytes(playback.installation.downloadBytes ?? 0)}`;
      } else {
        nativePlaybackStatusDetail.textContent = playback.build_includes_native_mpv
          ? "Install MPV with Homebrew, then check again. Browser playback remains available."
          : "This build uses browser playback. Optional MPV installation support comes next.";
      }
    }
  } catch (error) {
    nativePlaybackStatus.dataset.available = "false";
    nativePlaybackStatusTitle.textContent = "Couldn’t check MPV";
    nativePlaybackStatusDetail.textContent = errorMessage(error);
  } finally {
    refreshNativePlaybackButton.disabled = busy;
  }
}

async function installNativePlaybackRuntime(): Promise<void> {
  if (busy) return;
  const confirmed = window.confirm(
    "Download and install the verified MPV runtime for native video playback? It will be stored only in HeyaClient’s local app data.",
  );
  if (!confirmed) return;

  installNativePlaybackButton.disabled = true;
  refreshNativePlaybackButton.disabled = true;
  nativePlaybackInstallDetail.textContent = "Preparing MPV installation…";
  const progress = new Channel<MpvInstallProgress>();
  progress.onmessage = (event) => {
    if (event.event === "started") {
      nativePlaybackInstallDetail.textContent = `Downloading ${event.provider}…`;
    } else if (event.event === "downloading") {
      const percent = event.total_bytes > 0
        ? Math.min(100, Math.round((event.downloaded_bytes / event.total_bytes) * 100))
        : 0;
      nativePlaybackInstallDetail.textContent = `${percent}% downloaded`;
    } else if (event.event === "verifying") {
      nativePlaybackInstallDetail.textContent = "Verifying download…";
    } else if (event.event === "extracting") {
      nativePlaybackInstallDetail.textContent = "Installing MPV…";
    } else {
      nativePlaybackInstallDetail.textContent = "MPV installed.";
    }
  };

  try {
    await invoke<NativePlaybackStatus>("install_native_playback_runtime", {
      onEvent: progress,
    });
    await refreshNativePlaybackStatus();
  } catch (error) {
    nativePlaybackStatus.dataset.available = "false";
    nativePlaybackStatusTitle.textContent = "Couldn’t install MPV";
    nativePlaybackStatusDetail.textContent = errorMessage(error);
    nativePlaybackInstallActions.hidden = false;
  } finally {
    installNativePlaybackButton.disabled = busy;
    refreshNativePlaybackButton.disabled = busy;
  }
}

function updateSavedActionAvailability(): void {
  forgetSettingsButton.disabled = savedProfile === null;
  resetSessionButton.disabled = savedProfile === null;
}

function setStatus(kind: StatusKind, title: string, detail: string): void {
  status.dataset.kind = kind;
  statusTitle.textContent = title;
  statusDetail.textContent = detail;
}

function updateHttpWarning(): void {
  httpWarning.hidden = !input.value.trim().toLowerCase().startsWith("http://");
}

function profileDetail(profile: ServerProfile): string {
  const version = profile.server_version
    ? `Heya ${profile.server_version}`
    : "Heya server";
  return `${version} at ${profile.origin}`;
}

function errorMessage(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return "An unexpected native client error occurred.";
}

function requiredElement<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!element) throw new Error(`Missing required element #${id}`);
  return element as T;
}
