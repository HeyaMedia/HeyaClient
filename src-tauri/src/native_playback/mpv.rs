use super::{
    sanitize_diagnostic_label, AudioDiagnostics, AudioOutputDiagnostics, AudioSourceDiagnostics,
    BridgeErrorCode, EngineError, EngineEvent, EngineMedia, HealthDiagnostics,
    NativePlaybackManager, NativePlaybackState, NativeTrack, NativeTrackKind, NativeVideoSurface,
    NormalizedTrackId, PlaybackCapabilities, PlaybackCommand, PlaybackDiagnostics, PlaybackEngine,
    PlaybackEngineFactory, StateUpdateKind, TerminationReason, TransportDiagnostics,
    ValidatedPlaybackLoad, VideoColorDiagnostics, VideoDecodedDiagnostics, VideoDiagnostics,
    VideoOutputDiagnostics, VideoSourceDiagnostics,
};
use libmpv2::{
    events::{Event, PropertyData as MpvPropertyData},
    mpv_end_file_reason, mpv_error, EndFileReason, Error as MpvError, Format, Mpv,
};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::AppHandle;

pub const NATIVE_MPV_SPIKE_MENU_ID: &str = "native-mpv-window-spike";
pub const NATIVE_MPV_FULLSCREEN_ON_MENU_ID: &str = "native-mpv-fullscreen-on";
pub const NATIVE_MPV_FULLSCREEN_OFF_MENU_ID: &str = "native-mpv-fullscreen-off";
const SYNTHETIC_VIDEO_SOURCE: &str = "av://lavfi:testsrc2=duration=30:size=1280x720:rate=30";
const PLAYBACK_GRANT_HEADER_NAME: &str = "X-Heya-Playback-Grant";

pub struct MpvEngineFactory {
    #[cfg(target_os = "macos")]
    app: AppHandle,
}

impl MpvEngineFactory {
    pub fn new(app: AppHandle) -> Self {
        #[cfg(not(target_os = "macos"))]
        let _ = app;

        Self {
            #[cfg(target_os = "macos")]
            app,
        }
    }
}

impl PlaybackEngineFactory for MpvEngineFactory {
    fn capabilities(&self) -> PlaybackCapabilities {
        static PROBE: OnceLock<Result<(), ()>> = OnceLock::new();
        let available = PROBE
            .get_or_init(|| {
                probe_libmpv().map_err(|error| log::warn!("libmpv probe failed: {error}"))
            })
            .is_ok();
        PlaybackCapabilities::mpv(
            available,
            if cfg!(target_os = "macos") {
                NativeVideoSurface::NativeSurface
            } else {
                NativeVideoSurface::NativeWindow
            },
            (!available).then_some(BridgeErrorCode::BackendUnavailable),
        )
    }

    fn create(&self, media: EngineMedia) -> Result<Box<dyn PlaybackEngine>, EngineError> {
        match media {
            EngineMedia::Production(load) => {
                #[cfg(target_os = "macos")]
                {
                    match MpvEngine::new_production_embedded(load.clone(), self.app.clone()) {
                        Ok(engine) => return Ok(Box::new(engine) as _),
                        Err(error) => log::warn!(
                            "embedded MPV initialization failed; using the native-window fallback: {}",
                            error.message
                        ),
                    }
                }
                MpvEngine::new_production_window(load).map(|engine| Box::new(engine) as _)
            }
            #[cfg(debug_assertions)]
            EngineMedia::DevelopmentFile(path) => {
                MpvEngine::new(DevelopmentSource::File(path)).map(|engine| Box::new(engine) as _)
            }
            #[cfg(debug_assertions)]
            EngineMedia::Synthetic => {
                MpvEngine::new(DevelopmentSource::Synthetic).map(|engine| Box::new(engine) as _)
            }
        }
    }
}

#[cfg(debug_assertions)]
enum DevelopmentSource {
    File(PathBuf),
    Synthetic,
}

struct MpvEngine {
    // Field order is a safety invariant: Rust drops fields in declaration
    // order, and libmpv requires every render context to be freed before the
    // owning mpv core. Keeping the embedded renderer before `mpv` prevents an
    // abort even if the engine is unwound after a partial teardown.
    #[cfg(target_os = "macos")]
    embedded_renderer: Option<super::surface_macos::MacEmbeddedRenderer>,
    mpv: Mpv,
    video_surface: NativeVideoSurface,
    state: NativePlaybackState,
    diagnostics: PlaybackDiagnostics,
    queued: VecDeque<EngineEvent>,
    audio_track_map: HashMap<NormalizedTrackId, i64>,
    subtitle_track_map: HashMap<NormalizedTrackId, i64>,
}

enum PropertyData {
    Str(String),
    Flag(bool),
    Int64(i64),
    Double(f64),
}

impl PropertyData {
    fn from_mpv(value: MpvPropertyData<'_>) -> Self {
        match value {
            MpvPropertyData::Str(value) | MpvPropertyData::OsdStr(value) => {
                Self::Str(value.to_string())
            }
            MpvPropertyData::Flag(value) => Self::Flag(value),
            MpvPropertyData::Int64(value) => Self::Int64(value),
            MpvPropertyData::Double(value) => Self::Double(value),
        }
    }
}

impl MpvEngine {
    fn new_production_window(load: ValidatedPlaybackLoad) -> Result<Self, EngineError> {
        Self::new_production(load, MpvOutput::NativeWindow, None)
    }

    #[cfg(target_os = "macos")]
    fn new_production_embedded(
        load: ValidatedPlaybackLoad,
        app: AppHandle,
    ) -> Result<Self, EngineError> {
        Self::new_production(load, MpvOutput::Embedded, Some(app))
    }

    fn new_production(
        load: ValidatedPlaybackLoad,
        output: MpvOutput,
        #[cfg(target_os = "macos")] app: Option<AppHandle>,
        #[cfg(not(target_os = "macos"))] _app: Option<AppHandle>,
    ) -> Result<Self, EngineError> {
        let mpv = create_mpv(output)?;
        #[cfg(target_os = "macos")]
        let embedded_renderer = if output == MpvOutput::Embedded {
            Some(super::surface_macos::MacEmbeddedRenderer::attach(
                app.ok_or_else(|| EngineError::unavailable("the Heya app handle is unavailable"))?,
                &mpv,
            )?)
        } else {
            None
        };
        observe_properties(&mpv)?;

        // The remote page can provide only the opaque grant value. The header
        // name is fixed here and neither the grant nor arbitrary MPV options
        // are exposed back through state, diagnostics, or logs.
        let playback_header = format!(
            "{PLAYBACK_GRANT_HEADER_NAME}: {}",
            load.playback_grant_header_value()
        );
        mpv.set_property("http-header-fields", playback_header)
            .map_err(|error| mpv_command_error("could not configure native media access", error))?;
        load_source(
            &mpv,
            load.media_url().as_str(),
            load.start_position_seconds(),
        )
        .map_err(|error| mpv_command_error("could not load native Heya media", error))?;

        Ok(Self::initial(
            mpv,
            if output == MpvOutput::Embedded {
                NativeVideoSurface::NativeSurface
            } else {
                NativeVideoSurface::NativeWindow
            },
            #[cfg(target_os = "macos")]
            embedded_renderer,
        ))
    }

    #[cfg(debug_assertions)]
    fn new(source: DevelopmentSource) -> Result<Self, EngineError> {
        let mpv = create_mpv(MpvOutput::NativeWindow)?;
        observe_properties(&mpv)?;

        let source = match source {
            DevelopmentSource::File(path) => {
                if !path.is_file() {
                    return Err(EngineError::unavailable(
                        "the native development media file is unavailable",
                    ));
                }
                path.to_str()
                    .ok_or_else(|| {
                        EngineError::unavailable(
                            "the native development media path is not valid Unicode",
                        )
                    })?
                    .to_string()
            }
            DevelopmentSource::Synthetic => SYNTHETIC_VIDEO_SOURCE.to_string(),
        };

        mpv.command("loadfile", &[&source, "replace"])
            .map_err(|error| mpv_command_error("could not load native development media", error))?;

        Ok(Self::initial(
            mpv,
            NativeVideoSurface::NativeWindow,
            #[cfg(target_os = "macos")]
            None,
        ))
    }

    fn initial(
        mpv: Mpv,
        video_surface: NativeVideoSurface,
        #[cfg(target_os = "macos")] embedded_renderer: Option<
            super::surface_macos::MacEmbeddedRenderer,
        >,
    ) -> Self {
        Self {
            #[cfg(target_os = "macos")]
            embedded_renderer,
            mpv,
            video_surface,
            state: NativePlaybackState {
                loading: true,
                paused: false,
                volume: 1.0,
                ..NativePlaybackState::default()
            },
            diagnostics: PlaybackDiagnostics::default(),
            queued: VecDeque::new(),
            audio_track_map: HashMap::new(),
            subtitle_track_map: HashMap::new(),
        }
    }

    fn apply_property(&mut self, name: &str, change: PropertyData) -> Option<EngineEvent> {
        let kind = match (name, change) {
            ("pause", PropertyData::Flag(paused)) => {
                self.state.paused = paused;
                self.state.playing = !paused && !self.state.loading && !self.state.ended;
                Some(StateUpdateKind::Structural)
            }
            ("time-pos", PropertyData::Double(value)) if finite_non_negative(value) => {
                self.state.current_time = value;
                Some(StateUpdateKind::Position)
            }
            ("duration", PropertyData::Double(value)) if finite_non_negative(value) => {
                self.state.duration = value;
                Some(StateUpdateKind::Structural)
            }
            ("demuxer-cache-duration", PropertyData::Double(value))
                if finite_non_negative(value) =>
            {
                self.state.buffered = (self.state.current_time + value).min(self.state.duration);
                diagnostics_transport(&mut self.diagnostics).buffered_seconds = Some(value);
                return Some(self.diagnostics_event(false));
            }
            ("paused-for-cache", PropertyData::Flag(buffering)) => {
                self.state.buffering = buffering;
                Some(StateUpdateKind::Structural)
            }
            ("volume", PropertyData::Double(value)) if value.is_finite() => {
                self.state.volume = (value / 100.0).clamp(0.0, 1.0);
                Some(StateUpdateKind::Structural)
            }
            ("mute", PropertyData::Flag(muted)) => {
                self.state.muted = muted;
                Some(StateUpdateKind::Structural)
            }
            ("fullscreen", PropertyData::Flag(fullscreen)) => {
                self.state.fullscreen = fullscreen;
                Some(StateUpdateKind::Structural)
            }
            ("video-codec", PropertyData::Str(value)) => {
                diagnostics_video_source(&mut self.diagnostics).codec =
                    sanitize_diagnostic_label(value);
                return Some(self.diagnostics_event(true));
            }
            ("video-params/w", PropertyData::Int64(value)) => {
                diagnostics_video_source(&mut self.diagnostics).width = positive_u32(value);
                return Some(self.diagnostics_event(true));
            }
            ("video-params/h", PropertyData::Int64(value)) => {
                diagnostics_video_source(&mut self.diagnostics).height = positive_u32(value);
                return Some(self.diagnostics_event(true));
            }
            ("video-params/pixelformat", PropertyData::Str(value)) => {
                diagnostics_video_decoded(&mut self.diagnostics).pixel_format =
                    sanitize_diagnostic_label(value);
                return Some(self.diagnostics_event(true));
            }
            ("video-out-params/w", PropertyData::Int64(value)) => {
                diagnostics_video_output(&mut self.diagnostics).width = positive_u32(value);
                return Some(self.diagnostics_event(true));
            }
            ("video-out-params/h", PropertyData::Int64(value)) => {
                diagnostics_video_output(&mut self.diagnostics).height = positive_u32(value);
                return Some(self.diagnostics_event(true));
            }
            ("video-out-params/pixelformat", PropertyData::Str(value)) => {
                diagnostics_video_output(&mut self.diagnostics).pixel_format =
                    sanitize_diagnostic_label(value);
                return Some(self.diagnostics_event(true));
            }
            ("estimated-vf-fps", PropertyData::Double(value)) if finite_non_negative(value) => {
                diagnostics_video_decoded(&mut self.diagnostics).measured_frames_per_second =
                    Some(value);
                return Some(self.diagnostics_event(false));
            }
            ("video-bitrate", PropertyData::Double(value)) if finite_non_negative(value) => {
                diagnostics_video_source(&mut self.diagnostics).bitrate_bits_per_second =
                    Some(value.round() as u64);
                return Some(self.diagnostics_event(false));
            }
            ("hwdec-current", PropertyData::Str(value)) => {
                diagnostics_video_decoded(&mut self.diagnostics).hardware_decoder =
                    sanitize_diagnostic_label(value);
                return Some(self.diagnostics_event(true));
            }
            ("hwdec-interop", PropertyData::Str(value)) => {
                diagnostics_video_decoded(&mut self.diagnostics).hardware_interop =
                    sanitize_diagnostic_label(value);
                return Some(self.diagnostics_event(true));
            }
            ("video-params/primaries", PropertyData::Str(value)) => {
                diagnostics_video_color(&mut self.diagnostics).primaries =
                    sanitize_diagnostic_label(value);
                return Some(self.diagnostics_event(true));
            }
            ("video-params/gamma", PropertyData::Str(value)) => {
                diagnostics_video_color(&mut self.diagnostics).transfer =
                    sanitize_diagnostic_label(value);
                return Some(self.diagnostics_event(true));
            }
            ("video-params/colormatrix", PropertyData::Str(value)) => {
                diagnostics_video_color(&mut self.diagnostics).matrix =
                    sanitize_diagnostic_label(value);
                return Some(self.diagnostics_event(true));
            }
            ("audio-codec-name", PropertyData::Str(value)) => {
                diagnostics_audio_source(&mut self.diagnostics).codec =
                    sanitize_diagnostic_label(value);
                return Some(self.diagnostics_event(true));
            }
            ("audio-bitrate", PropertyData::Double(value)) if finite_non_negative(value) => {
                diagnostics_audio_source(&mut self.diagnostics).bitrate_bits_per_second =
                    Some(value.round() as u64);
                return Some(self.diagnostics_event(false));
            }
            ("audio-params/samplerate", PropertyData::Int64(value)) => {
                diagnostics_audio_source(&mut self.diagnostics).sample_rate = positive_u32(value);
                return Some(self.diagnostics_event(true));
            }
            ("audio-params/channel-count", PropertyData::Int64(value)) if value > 0 => {
                diagnostics_audio_source(&mut self.diagnostics).channels = Some(value.to_string());
                return Some(self.diagnostics_event(true));
            }
            ("audio-params/format", PropertyData::Str(value)) => {
                diagnostics_audio_source(&mut self.diagnostics).sample_format =
                    sanitize_diagnostic_label(value);
                return Some(self.diagnostics_event(true));
            }
            ("audio-out-params/samplerate", PropertyData::Int64(value)) => {
                diagnostics_audio_output(&mut self.diagnostics).sample_rate = positive_u32(value);
                return Some(self.diagnostics_event(true));
            }
            ("audio-out-params/channel-count", PropertyData::Int64(value)) if value > 0 => {
                diagnostics_audio_output(&mut self.diagnostics).channels = Some(value.to_string());
                return Some(self.diagnostics_event(true));
            }
            ("audio-out-params/format", PropertyData::Str(value)) => {
                diagnostics_audio_output(&mut self.diagnostics).sample_format =
                    sanitize_diagnostic_label(value);
                return Some(self.diagnostics_event(true));
            }
            ("audio-device", PropertyData::Str(value)) => {
                diagnostics_audio_output(&mut self.diagnostics).device =
                    sanitize_diagnostic_label(value);
                return Some(self.diagnostics_event(true));
            }
            ("frame-drop-count", PropertyData::Int64(value)) => {
                diagnostics_health(&mut self.diagnostics).dropped_frames = non_negative_u64(value);
                return Some(self.diagnostics_event(false));
            }
            ("decoder-frame-drop-count", PropertyData::Int64(value)) => {
                diagnostics_health(&mut self.diagnostics).decoder_dropped_frames =
                    non_negative_u64(value);
                return Some(self.diagnostics_event(false));
            }
            ("mistimed-frame-count", PropertyData::Int64(value)) => {
                diagnostics_health(&mut self.diagnostics).mistimed_frames = non_negative_u64(value);
                return Some(self.diagnostics_event(false));
            }
            ("avsync", PropertyData::Double(value)) if value.is_finite() => {
                diagnostics_health(&mut self.diagnostics).av_sync_milliseconds =
                    Some(value * 1_000.0);
                return Some(self.diagnostics_event(false));
            }
            ("cache-speed", PropertyData::Int64(value)) => {
                diagnostics_transport(&mut self.diagnostics).input_bytes_per_second =
                    non_negative_u64(value);
                return Some(self.diagnostics_event(false));
            }
            _ => None,
        };

        kind.map(|kind| EngineEvent::State {
            state: self.state.clone(),
            kind,
        })
    }

    fn diagnostics_event(&mut self, structural: bool) -> EngineEvent {
        self.diagnostics.sampled_at_milliseconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok());
        EngineEvent::Diagnostics {
            diagnostics: Box::new(Some(self.diagnostics.clone())),
            structural,
        }
    }

    fn refresh_tracks(&mut self) {
        self.audio_track_map.clear();
        self.subtitle_track_map.clear();
        self.state.audio_tracks.clear();
        self.state.subtitle_tracks.clear();

        let count = self
            .mpv
            .get_property::<i64>("track-list/count")
            .unwrap_or(0);
        for index in 0..count.clamp(0, 512) {
            let prefix = format!("track-list/{index}");
            let Ok(kind) = self.mpv.get_property::<String>(&format!("{prefix}/type")) else {
                continue;
            };
            let Ok(raw_id) = self.mpv.get_property::<i64>(&format!("{prefix}/id")) else {
                continue;
            };
            let selected = self
                .mpv
                .get_property::<bool>(&format!("{prefix}/selected"))
                .unwrap_or(false);
            let language = self
                .mpv
                .get_property::<String>(&format!("{prefix}/lang"))
                .ok()
                .and_then(|value| sanitize_diagnostic_label(&value));
            let title = self
                .mpv
                .get_property::<String>(&format!("{prefix}/title"))
                .ok()
                .and_then(|value| sanitize_diagnostic_label(&value));

            let (normalized, track_kind) = match kind.as_str() {
                "audio" => (format!("audio:{index}"), NativeTrackKind::Audio),
                "sub" => (format!("subtitle:{index}"), NativeTrackKind::Subtitle),
                _ => continue,
            };
            let Ok(id) = NormalizedTrackId::parse(normalized) else {
                continue;
            };
            let track = NativeTrack {
                id: id.clone(),
                kind: track_kind,
                language,
                title,
                selected,
            };
            match track_kind {
                NativeTrackKind::Audio => {
                    self.audio_track_map.insert(id.clone(), raw_id);
                    if selected {
                        self.state.selected_audio_track_id = Some(id);
                    }
                    self.state.audio_tracks.push(track);
                }
                NativeTrackKind::Subtitle => {
                    self.subtitle_track_map.insert(id.clone(), raw_id);
                    if selected {
                        self.state.selected_subtitle_track_id = Some(id);
                    }
                    self.state.subtitle_tracks.push(track);
                }
            }
        }
    }

    fn refresh_structural_diagnostics(&mut self) {
        // Scalar reads are intentionally explicit. No raw node, path, URL,
        // filename, header, cookie or metadata property is queried.
        for property in [
            "video-codec",
            "video-params/pixelformat",
            "video-out-params/pixelformat",
            "hwdec-current",
            "hwdec-interop",
            "video-params/primaries",
            "video-params/gamma",
            "video-params/colormatrix",
            "audio-codec-name",
            "audio-params/format",
            "audio-out-params/format",
            "audio-device",
        ] {
            if let Ok(value) = self.mpv.get_property::<String>(property) {
                let _ = self.apply_property(property, PropertyData::Str(value));
            }
        }
        for property in [
            "video-params/w",
            "video-params/h",
            "video-out-params/w",
            "video-out-params/h",
            "audio-params/samplerate",
            "audio-params/channel-count",
            "audio-out-params/samplerate",
            "audio-out-params/channel-count",
        ] {
            if let Ok(value) = self.mpv.get_property::<i64>(property) {
                let _ = self.apply_property(property, PropertyData::Int64(value));
            }
        }
    }
}

impl PlaybackEngine for MpvEngine {
    fn video_surface(&self) -> NativeVideoSurface {
        self.video_surface
    }

    fn command(&mut self, command: &PlaybackCommand) -> Result<(), EngineError> {
        match command {
            PlaybackCommand::Play => self.mpv.set_property("pause", false),
            PlaybackCommand::Pause => self.mpv.set_property("pause", true),
            PlaybackCommand::Seek { position_seconds } => {
                self.state.seek_revision = self.state.seek_revision.saturating_add(1);
                self.mpv.set_property("time-pos", *position_seconds)
            }
            PlaybackCommand::SetVolume { volume } => {
                self.mpv.set_property("volume", volume * 100.0)
            }
            PlaybackCommand::SetMuted { muted } => self.mpv.set_property("mute", *muted),
            PlaybackCommand::SetFullscreen { fullscreen } => {
                #[cfg(target_os = "macos")]
                if let Some(renderer) = &self.embedded_renderer {
                    renderer.set_fullscreen(*fullscreen)?;
                    self.state.fullscreen = *fullscreen;
                    self.queued.push_back(EngineEvent::State {
                        state: self.state.clone(),
                        kind: StateUpdateKind::Structural,
                    });
                    return Ok(());
                }
                self.mpv.set_property("fullscreen", *fullscreen)
            }
            PlaybackCommand::SelectAudioTrack { track_id } => {
                let raw_id = self.audio_track_map.get(track_id).ok_or_else(|| {
                    EngineError::command("the selected audio track is unavailable")
                })?;
                self.mpv.set_property("aid", *raw_id)
            }
            PlaybackCommand::SelectSubtitleTrack { track_id } => match track_id {
                Some(track_id) => {
                    let raw_id = self.subtitle_track_map.get(track_id).ok_or_else(|| {
                        EngineError::command("the selected subtitle track is unavailable")
                    })?;
                    self.mpv.set_property("sid", *raw_id)
                }
                None => self.mpv.set_property("sid", "no"),
            },
            PlaybackCommand::SelectVariant { .. } => {
                return Err(EngineError::command(
                    "quality selection requires a replacement playback descriptor",
                ));
            }
            PlaybackCommand::Stop => unreachable!("stop is handled by the renderer worker"),
        }
        .map_err(|error| mpv_command_error("MPV rejected the playback command", error))
    }

    fn poll_event(&mut self, timeout: Duration) -> Result<Option<EngineEvent>, EngineError> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(Some(event));
        }

        #[cfg(target_os = "macos")]
        if !self.state.video_surface_ready
            && self
                .embedded_renderer
                .as_ref()
                .is_some_and(super::surface_macos::MacEmbeddedRenderer::video_surface_ready)
        {
            self.state.video_surface_ready = true;
            return Ok(Some(EngineEvent::State {
                state: self.state.clone(),
                kind: StateUpdateKind::Structural,
            }));
        }

        let Some(event) = self.mpv.wait_event(timeout.as_secs_f64()) else {
            return Ok(None);
        };
        match event {
            Ok(Event::FileLoaded) => {
                self.state.loading = false;
                self.state.ended = false;
                if self.video_surface == NativeVideoSurface::NativeWindow {
                    self.state.video_surface_ready = true;
                }
                self.state.paused = self.mpv.get_property("pause").unwrap_or(false);
                self.state.playing = !self.state.paused;
                self.state.duration = self.mpv.get_property("duration").unwrap_or(0.0);
                self.refresh_tracks();
                self.refresh_structural_diagnostics();
                let diagnostics = self.diagnostics_event(true);
                self.queued.push_back(diagnostics);
                Ok(Some(EngineEvent::State {
                    state: self.state.clone(),
                    kind: StateUpdateKind::Structural,
                }))
            }
            Ok(Event::PropertyChange { name, change, .. }) => {
                let name = name.to_string();
                let change = PropertyData::from_mpv(change);
                Ok(self.apply_property(&name, change))
            }
            Ok(Event::VideoReconfig | Event::AudioReconfig) => {
                self.refresh_tracks();
                self.refresh_structural_diagnostics();
                let diagnostics = self.diagnostics_event(true);
                self.queued.push_back(diagnostics);
                Ok(Some(EngineEvent::State {
                    state: self.state.clone(),
                    kind: StateUpdateKind::Structural,
                }))
            }
            Ok(Event::Seek) => {
                self.state.seek_revision = self.state.seek_revision.saturating_add(1);
                Ok(Some(EngineEvent::State {
                    state: self.state.clone(),
                    kind: StateUpdateKind::Structural,
                }))
            }
            Ok(Event::EndFile(reason)) => {
                Ok(normalize_end_reason(reason).map(EngineEvent::Terminated))
            }
            Ok(Event::Shutdown) => Ok(Some(EngineEvent::Terminated(
                TerminationReason::NativeCrashed,
            ))),
            Ok(_) => Ok(None),
            Err(MpvError::Raw(
                mpv_error::PropertyUnavailable
                | mpv_error::PropertyNotFound
                | mpv_error::PropertyFormat,
            )) => Ok(None),
            Err(error) => Err(EngineError::command(format!(
                "MPV event processing failed: {error}"
            ))),
        }
    }

    fn stop(&mut self, _reason: TerminationReason) -> Result<(), EngineError> {
        self.mpv
            .command("stop", &[])
            .map_err(|error| mpv_command_error("could not stop MPV", error))
    }
}

fn probe_libmpv() -> Result<(), EngineError> {
    let _mpv = create_mpv(MpvOutput::Probe)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MpvOutput {
    NativeWindow,
    Embedded,
    Probe,
}

fn create_mpv(output: MpvOutput) -> Result<Mpv, EngineError> {
    Mpv::with_initializer(|initializer| {
        initializer.set_option("config", false)?;
        initializer.set_option("load-scripts", false)?;
        initializer.set_option("terminal", false)?;
        initializer.set_option(
            "vo",
            match output {
                MpvOutput::NativeWindow => "gpu-next",
                MpvOutput::Embedded => "libmpv",
                MpvOutput::Probe => "null",
            },
        )?;
        initializer.set_option(
            "audio",
            if output == MpvOutput::Probe {
                "no"
            } else {
                "auto"
            },
        )?;
        initializer.set_option("title", "Heya Native Player")?;
        initializer.set_option("hwdec", "auto-safe")?;
        initializer.set_option("input-default-bindings", output == MpvOutput::NativeWindow)?;
        initializer.set_option("input-vo-keyboard", output == MpvOutput::NativeWindow)?;
        initializer.set_option("sub-auto", "no")?;
        initializer.set_option("audio-file-auto", "no")?;
        Ok(())
    })
    .map_err(|error| EngineError::unavailable(format!("could not initialize libmpv: {error}")))
}

fn load_source(
    mpv: &Mpv,
    source: &str,
    start_position_seconds: Option<f64>,
) -> Result<(), MpvError> {
    if let Some(position) = start_position_seconds {
        let options = format!("start={position}");
        mpv.command("loadfile", &[source, "replace", "-1", &options])
    } else {
        mpv.command("loadfile", &[source, "replace"])
    }
}

fn observe_properties(mpv: &Mpv) -> Result<(), EngineError> {
    const PROPERTIES: &[(&str, Format)] = &[
        ("pause", Format::Flag),
        ("time-pos", Format::Double),
        ("duration", Format::Double),
        ("demuxer-cache-duration", Format::Double),
        ("paused-for-cache", Format::Flag),
        ("volume", Format::Double),
        ("mute", Format::Flag),
        ("fullscreen", Format::Flag),
        ("video-codec", Format::String),
        ("video-params/w", Format::Int64),
        ("video-params/h", Format::Int64),
        ("video-params/pixelformat", Format::String),
        ("video-out-params/w", Format::Int64),
        ("video-out-params/h", Format::Int64),
        ("video-out-params/pixelformat", Format::String),
        ("estimated-vf-fps", Format::Double),
        ("video-bitrate", Format::Double),
        ("hwdec-current", Format::String),
        ("hwdec-interop", Format::String),
        ("video-params/primaries", Format::String),
        ("video-params/gamma", Format::String),
        ("video-params/colormatrix", Format::String),
        ("audio-codec-name", Format::String),
        ("audio-bitrate", Format::Double),
        ("audio-params/samplerate", Format::Int64),
        ("audio-params/channel-count", Format::Int64),
        ("audio-params/format", Format::String),
        ("audio-out-params/samplerate", Format::Int64),
        ("audio-out-params/channel-count", Format::Int64),
        ("audio-out-params/format", Format::String),
        ("audio-device", Format::String),
        ("frame-drop-count", Format::Int64),
        ("decoder-frame-drop-count", Format::Int64),
        ("mistimed-frame-count", Format::Int64),
        ("avsync", Format::Double),
        ("cache-speed", Format::Int64),
    ];

    for (index, (name, format)) in PROPERTIES.iter().enumerate() {
        mpv.observe_property(name, *format, index as u64 + 1)
            .map_err(|error| {
                EngineError::unavailable(format!("could not observe MPV property {name}: {error}"))
            })?;
    }
    Ok(())
}

fn normalize_end_reason(reason: EndFileReason) -> Option<TerminationReason> {
    match reason {
        mpv_end_file_reason::Eof => Some(TerminationReason::Ended),
        mpv_end_file_reason::Stop => Some(TerminationReason::Stopped),
        mpv_end_file_reason::Quit => Some(TerminationReason::WindowClosed),
        mpv_end_file_reason::Error => Some(TerminationReason::Failed),
        mpv_end_file_reason::Redirect => None,
        _ => Some(TerminationReason::Failed),
    }
}

fn mpv_command_error(context: &str, error: MpvError) -> EngineError {
    EngineError::command(format!("{context}: {error}"))
}

fn finite_non_negative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

fn positive_u32(value: i64) -> Option<u32> {
    u32::try_from(value).ok().filter(|value| *value > 0)
}

fn non_negative_u64(value: i64) -> Option<u64> {
    u64::try_from(value).ok()
}

fn diagnostics_transport(value: &mut PlaybackDiagnostics) -> &mut TransportDiagnostics {
    value.transport.get_or_insert_with(Default::default)
}

fn diagnostics_video(value: &mut PlaybackDiagnostics) -> &mut VideoDiagnostics {
    value.video.get_or_insert_with(Default::default)
}

fn diagnostics_video_source(value: &mut PlaybackDiagnostics) -> &mut VideoSourceDiagnostics {
    diagnostics_video(value)
        .source
        .get_or_insert_with(Default::default)
}

fn diagnostics_video_decoded(value: &mut PlaybackDiagnostics) -> &mut VideoDecodedDiagnostics {
    diagnostics_video(value)
        .decoded
        .get_or_insert_with(Default::default)
}

fn diagnostics_video_output(value: &mut PlaybackDiagnostics) -> &mut VideoOutputDiagnostics {
    diagnostics_video(value)
        .output
        .get_or_insert_with(Default::default)
}

fn diagnostics_video_color(value: &mut PlaybackDiagnostics) -> &mut VideoColorDiagnostics {
    diagnostics_video(value)
        .color
        .get_or_insert_with(Default::default)
}

fn diagnostics_audio(value: &mut PlaybackDiagnostics) -> &mut AudioDiagnostics {
    value.audio.get_or_insert_with(Default::default)
}

fn diagnostics_audio_source(value: &mut PlaybackDiagnostics) -> &mut AudioSourceDiagnostics {
    diagnostics_audio(value)
        .source
        .get_or_insert_with(Default::default)
}

fn diagnostics_audio_output(value: &mut PlaybackDiagnostics) -> &mut AudioOutputDiagnostics {
    diagnostics_audio(value)
        .output
        .get_or_insert_with(Default::default)
}

fn diagnostics_health(value: &mut PlaybackDiagnostics) -> &mut HealthDiagnostics {
    value.health.get_or_insert_with(Default::default)
}

/// Restricts Vulkan discovery to Heya's bundled MoltenVK driver when running
/// from a macOS application bundle.
pub fn configure_bundled_vulkan_loader() {
    let Some(manifest) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(bundled_vulkan_manifest)
        .filter(|path| path.is_file())
    else {
        return;
    };
    std::env::set_var("VK_DRIVER_FILES", manifest);
}

fn bundled_vulkan_manifest(executable: &Path) -> Option<PathBuf> {
    let macos_dir = executable.parent()?;
    if macos_dir.file_name()? != "MacOS" {
        return None;
    }
    Some(
        macos_dir
            .parent()?
            .join("Resources/vulkan/icd.d/MoltenVK_icd.json"),
    )
}

/// Starts native-only local media. No path or URL crosses the WebView bridge.
#[cfg(debug_assertions)]
pub fn start_development_harness(manager: &NativePlaybackManager) -> Result<(), String> {
    let media = find_development_media()
        .map(EngineMedia::DevelopmentFile)
        .unwrap_or(EngineMedia::Synthetic);
    manager
        .start(super::PlaybackOwner::NativeDevelopmentHarness, media)
        .map(|started| {
            log::info!(
                "native MPV development renderer started: {}",
                started.renderer_session_id.as_str()
            );
        })
        .map_err(|error| error.message)
}

#[cfg(debug_assertions)]
fn find_development_media() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HEYA_MPV_TEST_MEDIA").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }

    let cwd = std::env::current_dir().ok()?;
    let directories = [
        cwd.join("../Heya/fulldata/Movies/Avatar (2009)"),
        cwd.join("fulldata/Movies/Avatar (2009)"),
    ];
    directories
        .into_iter()
        .filter_map(|directory| fs::read_dir(directory).ok())
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let is_mkv = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("mkv"));
            if !is_mkv {
                return None;
            }
            let size = entry.metadata().ok()?.len();
            Some((size, path))
        })
        .max_by_key(|(size, _)| *size)
        .map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::{
        bundled_vulkan_manifest, create_mpv, load_source, normalize_end_reason, MpvOutput,
        SYNTHETIC_VIDEO_SOURCE,
    };
    use crate::native_playback::TerminationReason;
    use libmpv2::mpv_end_file_reason;
    use std::path::Path;

    #[test]
    fn maps_mpv_end_reasons_without_treating_close_as_completion() {
        assert_eq!(
            normalize_end_reason(mpv_end_file_reason::Eof),
            Some(TerminationReason::Ended)
        );
        assert_eq!(
            normalize_end_reason(mpv_end_file_reason::Quit),
            Some(TerminationReason::WindowClosed)
        );
        assert_eq!(
            normalize_end_reason(mpv_end_file_reason::Error),
            Some(TerminationReason::Failed)
        );
        assert_eq!(normalize_end_reason(mpv_end_file_reason::Redirect), None);
    }

    #[test]
    fn resolves_the_vulkan_manifest_only_from_an_app_bundle() {
        assert_eq!(
            bundled_vulkan_manifest(Path::new(
                "/Applications/Heya.app/Contents/MacOS/heya-client"
            )),
            Some(
                Path::new(
                    "/Applications/Heya.app/Contents/Resources/vulkan/icd.d/MoltenVK_icd.json"
                )
                .to_path_buf()
            )
        );
        assert_eq!(
            bundled_vulkan_manifest(Path::new("/tmp/target/debug/heya-client")),
            None
        );
    }

    #[test]
    fn accepts_the_fixed_grant_header_and_start_position() {
        let mpv =
            create_mpv(MpvOutput::Probe).expect("libmpv should initialize for its adapter tests");
        mpv.set_property(
            "http-header-fields",
            "X-Heya-Playback-Grant: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .expect("the fixed playback header is a supported MPV property");
        load_source(&mpv, SYNTHETIC_VIDEO_SOURCE, Some(4.25))
            .expect("the per-load playback start option is supported by MPV");
    }
}
