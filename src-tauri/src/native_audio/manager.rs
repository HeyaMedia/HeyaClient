use super::core::dsp::crossfeed::CrossfeedPreset as EngineCrossfeedPreset;
use super::{
    core::{
        command::Command as EngineCommand,
        event::EngineEvent,
        output::{enumerate_output_devices, validate_output_device},
        types::EngineState,
        AudioEngine, AudioEngineClock,
    },
    AudioCapabilities, AudioCommand, AudioCommandRequest, AudioCommandResult, AudioLoadResult,
    AudioProcessingSettings, CrossfadeMode, CrossfeedPreset, DspBlockId, NativeAudioOutputDevice,
    NativeAudioOutputDevices, NativeAudioState, NativeAudioStateEvent, NativeAudioVisualizerEvent,
    ValidatedAudioLoad, ValidatedAudioTrack, NATIVE_AUDIO_PROTOCOL_VERSION,
};
use crate::native_playback::{
    BridgeError, BridgeErrorCode, CommandId, PlaybackError, RendererSessionId, TerminationReason,
    WebPlaybackOwner,
};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_COMMAND_RESULTS: usize = 512;

pub trait AudioEventSink: Send + Sync + 'static {
    fn state(&self, owner: &WebPlaybackOwner, event: &NativeAudioStateEvent);
    fn visualizer(&self, owner: &WebPlaybackOwner, event: &NativeAudioVisualizerEvent);
}

#[derive(Default)]
pub struct LogAudioEventSink;

impl AudioEventSink for LogAudioEventSink {
    fn state(&self, _owner: &WebPlaybackOwner, event: &NativeAudioStateEvent) {
        log::debug!(
            "native audio state renderer={} revision={} position={:.3}",
            event.renderer_session_id.as_str(),
            event.state_revision,
            event.payload.position_seconds
        );
    }

    fn visualizer(&self, _owner: &WebPlaybackOwner, _event: &NativeAudioVisualizerEvent) {}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSessionSnapshot {
    pub renderer_session_id: RendererSessionId,
    pub owner: WebPlaybackOwner,
    pub state_revision: u64,
    pub visualizer_revision: u64,
    pub next_command_sequence: u64,
}

struct ActiveSession {
    snapshot: AudioSessionSnapshot,
    engine: AudioEngine,
    state: NativeAudioState,
    processing: AudioProcessingSettings,
    command_results: HashMap<CommandId, AudioCommandResult>,
    track_formats: HashMap<i64, TrackFormat>,
    unmuted_volume: f64,
}

#[derive(Clone, Copy)]
struct TrackFormat {
    source_sample_rate: u32,
    source_channels: u16,
    output_sample_rate: u32,
    output_channels: u16,
}

#[derive(Default)]
struct ManagerState {
    active: Option<ActiveSession>,
}

struct Shared {
    state: Mutex<ManagerState>,
    command_gate: Mutex<()>,
    sink: Arc<dyn AudioEventSink>,
}

#[derive(Clone)]
pub struct NativeAudioManager {
    shared: Arc<Shared>,
}

impl NativeAudioManager {
    pub fn new(sink: Arc<dyn AudioEventSink>) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(ManagerState::default()),
                command_gate: Mutex::new(()),
                sink,
            }),
        }
    }

    pub fn capabilities(&self) -> AudioCapabilities {
        AudioCapabilities {
            protocol_version: NATIVE_AUDIO_PROTOCOL_VERSION,
            backend: "heya-rust-audio",
            available: true,
            gapless: true,
            crossfade: true,
            replay_gain: true,
            equalizer: true,
            visualizer: true,
            output_device_selection: true,
            unavailable_reason: None,
        }
    }

    pub fn output_devices(
        &self,
        preferred_device_id: Option<&str>,
    ) -> Result<NativeAudioOutputDevices, BridgeError> {
        let snapshot = enumerate_output_devices(preferred_device_id)
            .map_err(|message| BridgeError::new(BridgeErrorCode::BackendUnavailable, message))?;
        Ok(NativeAudioOutputDevices {
            devices: snapshot
                .devices
                .into_iter()
                .map(|device| NativeAudioOutputDevice {
                    device_id: device.id,
                    label: device.name,
                    is_default: device.is_default,
                })
                .collect(),
            active_device_id: snapshot.active_device_id,
            follows_system_default: snapshot.follows_system_default,
        })
    }

    pub fn validate_output_device(&self, device_id: &str) -> Result<(), BridgeError> {
        validate_output_device(device_id).map_err(BridgeError::invalid_request)
    }

    pub fn snapshot(&self) -> Option<AudioSessionSnapshot> {
        self.shared
            .state
            .lock()
            .ok()
            .and_then(|state| state.active.as_ref().map(|active| active.snapshot.clone()))
    }

    pub fn state(
        &self,
        owner: &WebPlaybackOwner,
        renderer_session_id: &RendererSessionId,
    ) -> Result<NativeAudioStateEvent, BridgeError> {
        let mut manager = self.shared.state.lock().map_err(|_| internal_error())?;
        let active = manager.active.as_mut().ok_or_else(unknown_session)?;
        ensure_owner(active, owner, renderer_session_id)?;
        sync_state_from_engine_clock(&mut active.state, active.engine.clock_snapshot());
        active.snapshot.state_revision = active.snapshot.state_revision.saturating_add(1);
        Ok(NativeAudioStateEvent {
            protocol_version: NATIVE_AUDIO_PROTOCOL_VERSION,
            renderer_session_id: renderer_session_id.clone(),
            state_revision: active.snapshot.state_revision,
            payload: active.state.clone(),
        })
    }

    pub fn start(
        &self,
        owner: WebPlaybackOwner,
        load: ValidatedAudioLoad,
        preferred_device_id: Option<&str>,
    ) -> Result<AudioLoadResult, BridgeError> {
        let _serial = self
            .shared
            .command_gate
            .lock()
            .map_err(|_| internal_error())?;
        self.dispose_locked(TerminationReason::Disposed)?;

        let engine = AudioEngine::start(preferred_device_id)
            .map_err(|message| BridgeError::new(BridgeErrorCode::BackendUnavailable, message))?;
        let events = engine.events();
        let renderer_session_id = RendererSessionId::generate();
        log::info!(
            "native audio load accepted renderer={} track={} start_seconds={:.3}",
            renderer_session_id.as_str(),
            load.track.meta.rating_key,
            load.track.start_position_seconds.unwrap_or(0.0),
        );
        let start_position_seconds = load.track.start_position_seconds.unwrap_or(0.0);
        let state = NativeAudioState {
            loading: true,
            current_track_id: Some(load.track.meta.rating_key),
            position_seconds: start_position_seconds,
            duration_seconds: load.track.meta.duration_ms as f64 / 1000.0,
            output_sample_rate_hz: Some(engine.device_sample_rate()),
            output_channels: Some(engine.device_channels()),
            output_device_id: Some(engine.output_device_id().to_string()),
            output_device_name: Some(engine.output_device_name().to_string()),
            dsp_active: processing_is_active(&load.processing),
            ..NativeAudioState::default()
        };
        configure_processing(&engine, &load.processing).map_err(command_error)?;
        engine
            .send(EngineCommand::Play {
                source: load.track.source,
                meta: load.track.meta,
            })
            .map_err(command_error)?;
        if let Some(position) = load.track.start_position_seconds {
            engine
                .send(EngineCommand::Seek {
                    position_ms: seconds_to_millis(position),
                })
                .map_err(command_error)?;
        }

        {
            let mut manager = self.shared.state.lock().map_err(|_| internal_error())?;
            manager.active = Some(ActiveSession {
                snapshot: AudioSessionSnapshot {
                    renderer_session_id: renderer_session_id.clone(),
                    owner: owner.clone(),
                    state_revision: 0,
                    visualizer_revision: 0,
                    next_command_sequence: 1,
                },
                engine,
                state: state.clone(),
                processing: load.processing,
                command_results: HashMap::new(),
                track_formats: HashMap::new(),
                unmuted_volume: 1.0,
            });
        }

        publish_state(&self.shared, &renderer_session_id);
        spawn_event_relay(self.shared.clone(), renderer_session_id.clone(), events)?;

        Ok(AudioLoadResult {
            renderer_session_id,
        })
    }

    pub fn preload(
        &self,
        owner: &WebPlaybackOwner,
        renderer_session_id: &RendererSessionId,
        command_id: CommandId,
        track: ValidatedAudioTrack,
    ) -> Result<AudioCommandResult, BridgeError> {
        self.execute(owner, renderer_session_id, command_id, |active| {
            active.engine.send(EngineCommand::PreloadNext {
                source: track.source,
                meta: track.meta,
            })
        })
    }

    pub fn send_command(
        &self,
        owner: &WebPlaybackOwner,
        request: AudioCommandRequest,
    ) -> Result<AudioCommandResult, BridgeError> {
        request.command.validate()?;
        self.execute(
            owner,
            &request.renderer_session_id,
            request.command_id,
            move |active| apply_command(active, request.command),
        )
    }

    fn execute(
        &self,
        owner: &WebPlaybackOwner,
        renderer_session_id: &RendererSessionId,
        command_id: CommandId,
        operation: impl FnOnce(&mut ActiveSession) -> Result<(), String>,
    ) -> Result<AudioCommandResult, BridgeError> {
        let _serial = self
            .shared
            .command_gate
            .lock()
            .map_err(|_| internal_error())?;
        let mut manager = self.shared.state.lock().map_err(|_| internal_error())?;
        let active = manager.active.as_mut().ok_or_else(unknown_session)?;
        ensure_owner(active, owner, renderer_session_id)?;

        if let Some(result) = active.command_results.get(&command_id) {
            let mut duplicate = result.clone();
            duplicate.duplicate = true;
            return Ok(duplicate);
        }

        let sequence = active.snapshot.next_command_sequence;
        active.snapshot.next_command_sequence = sequence.saturating_add(1);
        let result = match operation(active) {
            Ok(()) => AudioCommandResult {
                renderer_session_id: renderer_session_id.clone(),
                command_id: command_id.clone(),
                command_sequence: sequence,
                accepted: true,
                duplicate: false,
                error: None,
            },
            Err(message) => AudioCommandResult {
                renderer_session_id: renderer_session_id.clone(),
                command_id: command_id.clone(),
                command_sequence: sequence,
                accepted: false,
                duplicate: false,
                error: Some(PlaybackError {
                    code: BridgeErrorCode::CommandFailed,
                    message,
                }),
            },
        };
        if active.command_results.len() >= MAX_COMMAND_RESULTS {
            active.command_results.clear();
        }
        active.command_results.insert(command_id, result.clone());
        Ok(result)
    }

    pub fn dispose_owned(
        &self,
        owner: &WebPlaybackOwner,
        renderer_session_id: Option<&RendererSessionId>,
        reason: TerminationReason,
    ) -> Result<(), BridgeError> {
        let _serial = self
            .shared
            .command_gate
            .lock()
            .map_err(|_| internal_error())?;
        {
            let manager = self.shared.state.lock().map_err(|_| internal_error())?;
            let Some(active) = manager.active.as_ref() else {
                return Ok(());
            };
            if &active.snapshot.owner != owner
                || renderer_session_id.is_some_and(|id| id != &active.snapshot.renderer_session_id)
            {
                return Err(unknown_session());
            }
        }
        self.dispose_locked(reason)
    }

    pub fn dispose_active(&self, reason: TerminationReason) -> Result<(), BridgeError> {
        let _serial = self
            .shared
            .command_gate
            .lock()
            .map_err(|_| internal_error())?;
        self.dispose_locked(reason)
    }

    fn dispose_locked(&self, reason: TerminationReason) -> Result<(), BridgeError> {
        let removed = {
            let mut manager = self.shared.state.lock().map_err(|_| internal_error())?;
            manager.active.take()
        };
        if let Some(mut active) = removed {
            let _ = active.engine.send(EngineCommand::Stop);
            let _ = active.engine.send(EngineCommand::Shutdown);
            active.state.playing = false;
            active.state.paused = true;
            active.state.loading = false;
            active.state.buffering = false;
            active.state.ended = reason == TerminationReason::Ended;
            active.state.termination_reason = Some(reason);
            active.snapshot.state_revision = active.snapshot.state_revision.saturating_add(1);
            self.shared.sink.state(
                &active.snapshot.owner,
                &NativeAudioStateEvent {
                    protocol_version: NATIVE_AUDIO_PROTOCOL_VERSION,
                    renderer_session_id: active.snapshot.renderer_session_id,
                    state_revision: active.snapshot.state_revision,
                    payload: active.state,
                },
            );
        }
        Ok(())
    }
}

fn apply_command(active: &mut ActiveSession, command: AudioCommand) -> Result<(), String> {
    match command {
        AudioCommand::Play => active.engine.send(EngineCommand::Resume),
        AudioCommand::Pause => active.engine.send(EngineCommand::Pause),
        AudioCommand::Seek { position_seconds } => {
            log::info!(
                "native audio seek renderer={} position_seconds={position_seconds:.3}",
                active.snapshot.renderer_session_id.as_str(),
            );
            active.engine.send(EngineCommand::Seek {
                position_ms: seconds_to_millis(position_seconds),
            })
        }
        AudioCommand::SetVolume { volume } => {
            active.unmuted_volume = volume;
            active.state.volume = volume;
            let gain = if active.state.muted {
                0.0
            } else {
                volume as f32
            };
            active.engine.send(EngineCommand::SetVolume { gain })
        }
        AudioCommand::SetMuted { muted } => {
            active.state.muted = muted;
            active.engine.send(EngineCommand::SetVolume {
                gain: if muted {
                    0.0
                } else {
                    active.unmuted_volume as f32
                },
            })
        }
        AudioCommand::UpdateProcessing { settings } => {
            configure_processing(&active.engine, &settings)?;
            active.state.dsp_active = processing_is_active(&settings);
            active.processing = settings;
            Ok(())
        }
        AudioCommand::UpdateTrackAnalysis {
            track_id,
            gain_db,
            intro_end_ms,
            outro_start_ms,
            fade_start_ms,
            silence_start_ms,
        } => active.engine.send(EngineCommand::UpdateTrackAnalysis {
            rating_key: track_id,
            gain_db,
            intro_end_ms,
            outro_start_ms,
            fade_start_ms,
            silence_start_ms,
        }),
        AudioCommand::Stop => active.engine.send(EngineCommand::Stop),
    }
}

fn configure_processing(
    engine: &AudioEngine,
    settings: &AudioProcessingSettings,
) -> Result<(), String> {
    engine.send(EngineCommand::SetNormalization {
        enabled: settings.replay_gain_enabled,
    })?;
    engine.send(EngineCommand::SetPreampGain {
        db: settings.preamp_db,
    })?;
    engine.send(EngineCommand::SetEq {
        gains_db: settings.eq_bands_db,
    })?;
    engine.send(EngineCommand::SetEqEnabled {
        enabled: settings.eq_enabled,
    })?;
    engine.send(EngineCommand::SetEqPostgain {
        db: settings.postgain_db,
    })?;
    engine.send(EngineCommand::SetLimiterEnabled {
        enabled: settings.limiter_enabled,
    })?;
    let preset = match settings.crossfeed_preset {
        CrossfeedPreset::Subtle => EngineCrossfeedPreset::Subtle,
        CrossfeedPreset::Natural => EngineCrossfeedPreset::Natural,
        CrossfeedPreset::Strong => EngineCrossfeedPreset::Strong,
    };
    engine.send(EngineCommand::SetCrossfeed {
        enabled: settings.crossfeed_enabled,
        preset,
    })?;
    engine.send(EngineCommand::SetCrossfeedBeforeEq {
        before: settings.dsp_order[0] == DspBlockId::Crossfeed,
    })?;
    let crossfade_ms = match settings.crossfade_mode {
        CrossfadeMode::Gapless => 0,
        CrossfadeMode::Crossfade | CrossfadeMode::Smart => {
            (settings.crossfade_seconds * 1000.0).round() as u32
        }
    };
    engine.send(EngineCommand::SetCrossfadeWindow { ms: crossfade_ms })?;
    engine.send(EngineCommand::SetSameAlbumCrossfade {
        // Album continuity is an invariant. Heya additionally marks exact
        // adjacent/repeat transitions with skip_crossfade, while this guards
        // same-release transitions if queue sequence metadata is incomplete.
        enabled: false,
    })?;
    engine.send(EngineCommand::SetSmartCrossfade {
        enabled: settings.crossfade_mode == CrossfadeMode::Smart,
    })?;
    engine.send(EngineCommand::SetSmartCrossfadeMax { ms: crossfade_ms })?;
    engine.send(EngineCommand::SetVisualizerEnabled {
        enabled: settings.visualizer_enabled,
    })
}

fn spawn_event_relay(
    shared: Arc<Shared>,
    renderer_session_id: RendererSessionId,
    events: Receiver<EngineEvent>,
) -> Result<(), BridgeError> {
    thread::Builder::new()
        .name(format!(
            "heya-audio-events-{}",
            &renderer_session_id.as_str()[..8]
        ))
        .spawn(move || loop {
            match events.recv_timeout(EVENT_POLL_INTERVAL) {
                Ok(event) => handle_engine_event(&shared, &renderer_session_id, event),
                Err(RecvTimeoutError::Timeout) => {
                    let current = shared.state.lock().ok().and_then(|manager| {
                        manager
                            .active
                            .as_ref()
                            .map(|active| active.snapshot.renderer_session_id.clone())
                    });
                    if current.as_ref() != Some(&renderer_session_id) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        })
        .map(|_| ())
        .map_err(|error| {
            BridgeError::new(
                BridgeErrorCode::BackendUnavailable,
                format!("could not start the native audio event worker: {error}"),
            )
        })
}

fn handle_engine_event(
    shared: &Shared,
    renderer_session_id: &RendererSessionId,
    event: EngineEvent,
) {
    if let EngineEvent::VisFrame {
        samples,
        frequency_bins,
    } = event
    {
        let output = {
            let Ok(mut manager) = shared.state.lock() else {
                return;
            };
            let Some(active) = manager.active.as_mut() else {
                return;
            };
            if &active.snapshot.renderer_session_id != renderer_session_id {
                return;
            }
            active.snapshot.visualizer_revision =
                active.snapshot.visualizer_revision.saturating_add(1);
            (
                active.snapshot.owner.clone(),
                NativeAudioVisualizerEvent {
                    protocol_version: NATIVE_AUDIO_PROTOCOL_VERSION,
                    renderer_session_id: renderer_session_id.clone(),
                    visualizer_revision: active.snapshot.visualizer_revision,
                    samples,
                    frequency_bins,
                },
            )
        };
        shared.sink.visualizer(&output.0, &output.1);
        return;
    }

    {
        let Ok(mut manager) = shared.state.lock() else {
            return;
        };
        let Some(active) = manager.active.as_mut() else {
            return;
        };
        if &active.snapshot.renderer_session_id != renderer_session_id {
            return;
        }
        active.state.started_track_id = None;
        active.state.ended_track_id = None;
        match event {
            EngineEvent::State { state } => {
                log::info!(
                    "native audio state renderer={} state={state}",
                    renderer_session_id.as_str(),
                );
                match state.as_str() {
                    "playing" => {
                        active.state.playing = true;
                        active.state.paused = false;
                        active.state.loading = false;
                        active.state.buffering = false;
                    }
                    "paused" => {
                        active.state.playing = false;
                        active.state.paused = true;
                        active.state.loading = false;
                        active.state.buffering = false;
                    }
                    "buffering" => {
                        active.state.playing = false;
                        active.state.loading = false;
                        active.state.buffering = true;
                    }
                    "stopped" => {
                        active.state.playing = false;
                        active.state.paused = true;
                        active.state.buffering = false;
                    }
                    _ => {}
                }
            }
            EngineEvent::BufferUnderrun => {
                log::warn!(
                    "native audio buffer underrun renderer={}",
                    renderer_session_id.as_str()
                );
                active.state.playing = false;
                active.state.loading = false;
                active.state.buffering = true;
            }
            EngineEvent::TrackStarted {
                rating_key,
                duration_ms,
            } => {
                log::info!(
                    "native audio track started renderer={} track={} duration_ms={}",
                    renderer_session_id.as_str(),
                    rating_key,
                    duration_ms,
                );
                // The first TrackStarted event belongs to the load whose
                // requested resume position is already represented in state.
                // Preserve it until the callback publishes its authoritative
                // post-seek position. A genuinely different preloaded track,
                // however, always begins at zero.
                let replaces_track = active.state.current_track_id != Some(rating_key);
                active.state.current_track_id = Some(rating_key);
                active.state.started_track_id = Some(rating_key);
                active.state.duration_seconds = duration_ms as f64 / 1000.0;
                if replaces_track {
                    active.state.position_seconds = 0.0;
                }
                active.state.ended = false;
                active.state.termination_reason = None;
                if let Some(format) = active.track_formats.get(&rating_key).copied() {
                    apply_track_format(active, format);
                }
            }
            EngineEvent::TrackEnded { rating_key } => {
                active.state.ended_track_id = Some(rating_key);
                if active.state.current_track_id == Some(rating_key) {
                    active.state.ended = true;
                    active.state.playing = false;
                    active.state.paused = true;
                    active.state.termination_reason = Some(TerminationReason::Ended);
                }
            }
            EngineEvent::Format {
                rating_key,
                source_sample_rate,
                source_channels,
                output_sample_rate,
                output_channels,
            } => {
                let format = TrackFormat {
                    source_sample_rate,
                    source_channels,
                    output_sample_rate,
                    output_channels,
                };
                active.track_formats.insert(rating_key, format);
                if active.track_formats.len() > 8 {
                    let current_track_id = active.state.current_track_id;
                    active.track_formats.retain(|track_id, _| {
                        Some(*track_id) == current_track_id || *track_id == rating_key
                    });
                }
                if active.state.current_track_id == Some(rating_key) {
                    apply_track_format(active, format);
                }
            }
            EngineEvent::Error { message } => {
                active.state.loading = false;
                active.state.buffering = false;
                active.state.playing = false;
                active.state.paused = true;
                active.state.error = Some(PlaybackError {
                    code: BridgeErrorCode::CommandFailed,
                    message,
                });
                active.state.termination_reason = Some(TerminationReason::Failed);
            }
            EngineEvent::PreloadError {
                rating_key,
                message,
            } => {
                log::warn!(
                    "native audio preload failed renderer={} track={rating_key}: {message}",
                    renderer_session_id.as_str(),
                );
            }
            EngineEvent::VisFrame { .. } => unreachable!(),
        }
    }
    publish_state(shared, renderer_session_id);
}

fn apply_track_format(active: &mut ActiveSession, format: TrackFormat) {
    active.state.source_sample_rate_hz = Some(format.source_sample_rate);
    active.state.source_channels = Some(format.source_channels);
    active.state.output_sample_rate_hz = Some(format.output_sample_rate);
    active.state.output_channels = Some(format.output_channels);
    active.state.resampler_active = format.source_sample_rate != format.output_sample_rate;
}

fn sync_state_from_engine_clock(state: &mut NativeAudioState, clock: AudioEngineClock) {
    // Preserve the requested resume point until the callback has accepted the
    // initial deck/seek. A zeroed stopped clock during load is not newer truth.
    if !state.loading || clock.state != EngineState::Stopped || clock.position_seconds > 0.0 {
        state.position_seconds = clock.position_seconds;
    }
    if clock.duration_seconds > 0.0 {
        state.duration_seconds = clock.duration_seconds;
    }
    if let Some(track_id) = clock.active_track_id {
        state.current_track_id = Some(track_id);
    }

    match clock.state {
        EngineState::Playing => {
            state.playing = true;
            state.paused = false;
            state.loading = false;
            state.buffering = false;
        }
        EngineState::Paused => {
            state.playing = false;
            state.paused = true;
            state.loading = false;
            state.buffering = false;
        }
        EngineState::Buffering => {
            state.playing = false;
            state.paused = false;
            state.loading = false;
            state.buffering = true;
        }
        EngineState::Stopped if !state.loading => {
            state.playing = false;
            state.paused = true;
            state.buffering = false;
        }
        EngineState::Stopped => {}
    }
}

fn publish_state(shared: &Shared, renderer_session_id: &RendererSessionId) {
    let output = {
        let Ok(mut manager) = shared.state.lock() else {
            return;
        };
        let Some(active) = manager.active.as_mut() else {
            return;
        };
        if &active.snapshot.renderer_session_id != renderer_session_id {
            return;
        }
        // Lifecycle events remain push-based, but their clock fields come
        // straight from the callback atomics. Continuous position is pulled
        // by protocol v2 and projected in Heya, so no 4 Hz event relay is
        // needed merely to move the visible playhead.
        sync_state_from_engine_clock(&mut active.state, active.engine.clock_snapshot());
        active.snapshot.state_revision = active.snapshot.state_revision.saturating_add(1);
        (
            active.snapshot.owner.clone(),
            NativeAudioStateEvent {
                protocol_version: NATIVE_AUDIO_PROTOCOL_VERSION,
                renderer_session_id: renderer_session_id.clone(),
                state_revision: active.snapshot.state_revision,
                payload: active.state.clone(),
            },
        )
    };
    shared.sink.state(&output.0, &output.1);
}

fn ensure_owner(
    active: &ActiveSession,
    owner: &WebPlaybackOwner,
    renderer_session_id: &RendererSessionId,
) -> Result<(), BridgeError> {
    if &active.snapshot.owner != owner
        || &active.snapshot.renderer_session_id != renderer_session_id
    {
        return Err(unknown_session());
    }
    Ok(())
}

fn processing_is_active(settings: &AudioProcessingSettings) -> bool {
    settings.replay_gain_enabled
        || settings.eq_enabled
        || settings.preamp_db != 0.0
        || settings.postgain_db != 0.0
        || settings.limiter_enabled
        || settings.crossfeed_enabled
        || settings.crossfade_mode != CrossfadeMode::Gapless
}

fn seconds_to_millis(seconds: f64) -> u64 {
    (seconds * 1000.0).round().clamp(0.0, u64::MAX as f64) as u64
}

fn command_error(message: String) -> BridgeError {
    BridgeError::new(BridgeErrorCode::CommandFailed, message)
}

fn unknown_session() -> BridgeError {
    BridgeError::new(
        BridgeErrorCode::UnknownSession,
        "the native audio session is no longer active",
    )
}

fn internal_error() -> BridgeError {
    BridgeError::new(
        BridgeErrorCode::InternalError,
        "native audio state is unavailable",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_expose_the_supported_native_audio_path() {
        let manager = NativeAudioManager::new(Arc::new(LogAudioEventSink));
        let capabilities = manager.capabilities();

        assert!(capabilities.available);
        assert!(capabilities.output_device_selection);
        assert!(capabilities.gapless);
        assert!(capabilities.crossfade);
    }

    #[test]
    fn direct_engine_clock_repairs_a_stale_event_snapshot() {
        let mut state = NativeAudioState {
            playing: true,
            paused: false,
            position_seconds: 0.0,
            duration_seconds: 146.0,
            current_track_id: Some(7),
            ..NativeAudioState::default()
        };

        sync_state_from_engine_clock(
            &mut state,
            AudioEngineClock {
                state: EngineState::Playing,
                position_seconds: 31.25,
                duration_seconds: 146.0,
                active_track_id: Some(7),
            },
        );

        assert_eq!(state.position_seconds, 31.25);
        assert!(state.playing);
        assert!(!state.paused);
    }
}
