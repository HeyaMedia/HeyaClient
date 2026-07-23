//! Native audio engine derived from Hibiki/Plexify's tested Rust core.
//!
//! The real-time callback owns decks, scheduling and DSP. Network fetch and
//! Symphonia decoding stay off the callback thread; all communication into the
//! callback uses bounded or lock-free channels.

pub mod callback;
pub mod command;
pub mod crossfade;
pub mod deck;
pub mod dsp;
pub mod event;
pub mod output;
pub mod types;
pub mod visualizer;

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use tracing::{debug, info, warn};

use self::callback::{AudioCallbackState, SharedAtomics};
use self::command::{AudioCommand, Command, SampleBatch};
use self::crossfade::types::{parse_ramp, CrossfadeSettings, TrackRamps};
use self::deck::decode;
use self::event::{EngineEvent, VisualizerFrame};
use self::output::resample::StreamingResampler;
use self::output::CpalOutput;
use self::types::{AudioSource, DeckId, EngineState, TrackMeta};
use self::visualizer::VisualizerProcessor;

pub const PLAYBACK_GRANT_HEADER: &str = "X-Heya-Playback-Grant";
const SAMPLE_BATCH_CHANNEL_CAPACITY: usize = 8;
const INITIAL_DECODE_SECONDS: usize = 3;
const BACKGROUND_DECODE_BATCH_SECONDS: usize = 1;
const PCM_BUFFER_SECONDS: usize = 24;
const MAX_COMPRESSED_SPOOL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const VISUALIZER_BUFFER_POOL_SIZE: usize = 8;
const VISUALIZER_BUFFER_CAPACITY_SAMPLES: usize = 64 * 1024;
const RETIRED_PCM_BUFFER_CAPACITY: usize = 32;
/// How long playback must sit paused/stopped before the OS output stream is
/// suspended. Long enough that pause→resume within a beat never touches the
/// stream, short enough that an idle window stops burning coreaudiod CPU.
const STREAM_IDLE_PAUSE_AFTER: Duration = Duration::from_secs(2);
const CONTROL_IDLE_POLL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug)]
enum StreamCtl {
    Pause,
    Resume,
    Shutdown,
}

fn pending_deck(atomics: &SharedAtomics) -> DeckId {
    if atomics.active_deck_id.load(Ordering::Relaxed) == 0 {
        DeckId::B
    } else {
        DeckId::A
    }
}

fn deck_generation(atomics: &SharedAtomics, deck: DeckId) -> Arc<AtomicU64> {
    match deck {
        DeckId::A => atomics.deck_a_generation.clone(),
        DeckId::B => atomics.deck_b_generation.clone(),
    }
}

fn next_deck_generation(atomics: &SharedAtomics, deck: DeckId) -> u64 {
    deck_generation(atomics, deck).fetch_add(1, Ordering::Relaxed) + 1
}

pub struct AudioEngine {
    cmd_tx: Sender<Command>,
    atomics: Arc<SharedAtomics>,
    event_rx: Receiver<EngineEvent>,
    visualizer_event_rx: Receiver<VisualizerFrame>,
    running: Arc<AtomicBool>,
    stream_ctl_tx: Sender<StreamCtl>,
    device_sample_rate: u32,
    device_channels: u16,
    output_sample_format: String,
    output_device_id: String,
    output_device_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioEngineClock {
    pub state: EngineState,
    pub position_seconds: f64,
    pub duration_seconds: f64,
    pub active_track_id: Option<i64>,
}

#[derive(Clone, Copy, Debug)]
enum LoadIntent {
    Play,
    Preload,
}

#[allow(clippy::too_many_arguments)]
fn spawn_load_job(
    intent: LoadIntent,
    source: AudioSource,
    meta: TrackMeta,
    deck: DeckId,
    generation: u64,
    audio_cmd_tx: Sender<AudioCommand>,
    sample_tx: Sender<SampleBatch>,
    event_tx: Sender<EngineEvent>,
    atomics: Arc<SharedAtomics>,
    normalization_enabled: bool,
    device_rate: u32,
    device_channels: u16,
) {
    let rating_key = meta.rating_key;
    let worker_event_tx = event_tx.clone();
    let worker_atomics = atomics.clone();
    let spawn_result = std::thread::Builder::new()
        .name(format!("heya-audio-load-{rating_key}"))
        .spawn(move || match intent {
            LoadIntent::Play => handle_play(
                &source,
                &meta,
                deck,
                generation,
                &audio_cmd_tx,
                &sample_tx,
                &worker_event_tx,
                &worker_atomics,
                normalization_enabled,
                device_rate,
                device_channels,
            ),
            LoadIntent::Preload => handle_preload(
                &source,
                &meta,
                deck,
                generation,
                &audio_cmd_tx,
                &sample_tx,
                &worker_event_tx,
                &worker_atomics,
                normalization_enabled,
                device_rate,
                device_channels,
            ),
        });

    if let Err(error) = spawn_result {
        let message = format!("could not start native audio loader: {error}");
        match intent {
            LoadIntent::Play => {
                let _ = event_tx.send(EngineEvent::Error { message });
            }
            LoadIntent::Preload => {
                atomics
                    .preload_error_rk
                    .store(rating_key, Ordering::Relaxed);
                let _ = event_tx.send(EngineEvent::PreloadError {
                    rating_key,
                    message,
                });
            }
        }
    }
}

fn read_engine_clock(atomics: &SharedAtomics, device_sample_rate: u32) -> AudioEngineClock {
    let duration_seconds = atomics.duration_ms.load(Ordering::Relaxed) as f64 / 1000.0;
    let position_seconds = if device_sample_rate == 0 {
        0.0
    } else {
        atomics.position_frames.load(Ordering::Relaxed) as f64 / f64::from(device_sample_rate)
    };
    let active_track_id = atomics.active_rating_key.load(Ordering::Relaxed);
    AudioEngineClock {
        state: atomics.get_state(),
        position_seconds: if duration_seconds > 0.0 {
            position_seconds.min(duration_seconds)
        } else {
            position_seconds
        },
        duration_seconds,
        active_track_id: (active_track_id > 0).then_some(active_track_id),
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = self.cmd_tx.try_send(Command::Shutdown);
        let _ = self.stream_ctl_tx.try_send(StreamCtl::Shutdown);
    }
}

impl AudioEngine {
    pub fn start(preferred_device_id: Option<&str>) -> Result<Self, String> {
        Self::start_with_output(preferred_device_id)
    }

    /// Read the callback-owned PCM clock without depending on the event relay.
    /// This is the recovery source for a dropped Rust event or WebView event.
    pub fn clock_snapshot(&self) -> AudioEngineClock {
        read_engine_clock(&self.atomics, self.device_sample_rate)
    }

    fn start_with_output(preferred_device_id: Option<&str>) -> Result<Self, String> {
        let atomics = Arc::new(SharedAtomics::new());
        let running = Arc::new(AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = bounded::<Command>(64);
        let (audio_cmd_tx, audio_cmd_rx) = bounded::<AudioCommand>(256);
        // Bound producer lead so a fast decoder cannot queue an entire track
        // for the callback to copy in one deadline.
        let (deck_a_tx, deck_a_rx) = bounded::<SampleBatch>(SAMPLE_BATCH_CHANNEL_CAPACITY);
        let (deck_b_tx, deck_b_rx) = bounded::<SampleBatch>(SAMPLE_BATCH_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = bounded::<EngineEvent>(256);
        let (visualizer_event_tx, visualizer_event_rx) = bounded::<VisualizerFrame>(2);
        let (vis_tx, vis_rx) = bounded::<Vec<f32>>(VISUALIZER_BUFFER_POOL_SIZE);
        let (vis_recycle_tx, vis_recycle_rx) = bounded::<Vec<f32>>(VISUALIZER_BUFFER_POOL_SIZE);
        for _ in 0..VISUALIZER_BUFFER_POOL_SIZE {
            let _ = vis_recycle_tx.send(Vec::with_capacity(VISUALIZER_BUFFER_CAPACITY_SAMPLES));
        }
        let (retired_buffer_tx, retired_buffer_rx) =
            bounded::<Vec<f32>>(RETIRED_PCM_BUFFER_CAPACITY);
        std::thread::Builder::new()
            .name("heya-audio-buffer-reclaimer".into())
            .spawn(move || {
                while let Ok(buffer) = retired_buffer_rx.recv() {
                    drop(buffer);
                }
            })
            .map_err(|error| format!("failed to start PCM buffer reclaimer: {error}"))?;

        let callback = AudioCallbackState::new(
            audio_cmd_rx,
            deck_a_rx,
            deck_b_rx,
            event_tx.clone(),
            vis_tx,
            vis_recycle_tx.clone(),
            vis_recycle_rx,
            retired_buffer_tx,
            atomics.clone(),
        );
        let output = CpalOutput::open(callback, preferred_device_id, event_tx.clone())
            .map_err(|error| format!("failed to open audio output: {error}"))?;
        let device_sample_rate = output.sample_rate;
        let device_channels = output.channels;
        let output_device_id = output.device_id.clone();
        let output_device_name = output.device_name.clone();
        let output_sample_format = output.sample_format.clone();
        info!(
            device = %output_device_name,
            device_id = %output_device_id,
            sample_rate = device_sample_rate,
            channels = device_channels,
            sample_format = %output_sample_format,
            "native audio engine started"
        );

        // The Stream is !Send-safe by convention (see CpalOutput) — it lives on
        // this holder thread, which also owns pausing/resuming it on behalf of
        // the control thread.
        let (stream_ctl_tx, stream_ctl_rx) = bounded::<StreamCtl>(8);
        std::thread::Builder::new()
            .name("heya-audio-output".into())
            .spawn(move || {
                let output = output;
                let mut stream_paused = false;
                loop {
                    match stream_ctl_rx.recv() {
                        Ok(StreamCtl::Pause) if !stream_paused => match output.pause() {
                            Ok(()) => {
                                stream_paused = true;
                                debug!("audio output stream suspended (idle)");
                            }
                            Err(error) => {
                                warn!(%error, "could not suspend idle audio output stream");
                            }
                        },
                        Ok(StreamCtl::Resume) if stream_paused => match output.resume() {
                            Ok(()) => {
                                stream_paused = false;
                                debug!("audio output stream resumed");
                            }
                            Err(error) => {
                                warn!(%error, "could not resume audio output stream");
                            }
                        },
                        Ok(StreamCtl::Pause) | Ok(StreamCtl::Resume) => {}
                        Ok(StreamCtl::Shutdown) | Err(_) => break,
                    }
                }
            })
            .map_err(|error| format!("failed to start audio output holder: {error}"))?;

        let running_visualizer = running.clone();
        std::thread::Builder::new()
            .name("heya-audio-visualizer".into())
            .spawn(move || {
                let mut visualizer = VisualizerProcessor::new(device_channels);
                while running_visualizer.load(Ordering::Acquire) {
                    let Ok(mut samples) = vis_rx.recv_timeout(Duration::from_millis(100)) else {
                        continue;
                    };
                    visualizer.push_samples(&samples);
                    samples.clear();
                    let _ = vis_recycle_tx.try_send(samples);
                    while let Ok(mut more) = vis_rx.try_recv() {
                        visualizer.push_samples(&more);
                        more.clear();
                        let _ = vis_recycle_tx.try_send(more);
                    }
                    if let Some((time_domain, frequency_bins)) = visualizer.compute() {
                        let _ = visualizer_event_tx.try_send(VisualizerFrame {
                            samples: time_domain,
                            frequency_bins,
                        });
                    }
                }
            })
            .map_err(|error| format!("failed to start visualizer: {error}"))?;

        let running_control = running.clone();
        let clock_atomics = atomics.clone();
        let stream_ctl_for_control = stream_ctl_tx.clone();
        std::thread::Builder::new()
            .name("heya-audio-control".into())
            .spawn(move || {
                control_thread_main(
                    cmd_rx,
                    audio_cmd_tx,
                    deck_a_tx,
                    deck_b_tx,
                    event_tx,
                    atomics,
                    device_sample_rate,
                    device_channels,
                    stream_ctl_for_control,
                );
                running_control.store(false, Ordering::Release);
            })
            .map_err(|error| format!("failed to start audio control: {error}"))?;

        Ok(Self {
            cmd_tx,
            atomics: clock_atomics,
            event_rx,
            visualizer_event_rx,
            running,
            stream_ctl_tx,
            device_sample_rate,
            device_channels,
            output_sample_format,
            output_device_id,
            output_device_name,
        })
    }

    pub fn send(&self, command: Command) -> Result<(), String> {
        self.cmd_tx
            .send(command)
            .map_err(|_| "native audio engine is unavailable".to_string())
    }

    pub fn events(&self) -> Receiver<EngineEvent> {
        self.event_rx.clone()
    }
    pub fn visualizer_events(&self) -> Receiver<VisualizerFrame> {
        self.visualizer_event_rx.clone()
    }
    pub fn atomics(&self) -> &Arc<SharedAtomics> {
        &self.atomics
    }
    pub fn device_sample_rate(&self) -> u32 {
        self.device_sample_rate
    }
    pub fn device_channels(&self) -> u16 {
        self.device_channels
    }
    pub fn output_sample_format(&self) -> &str {
        &self.output_sample_format
    }
    pub fn output_device_id(&self) -> &str {
        &self.output_device_id
    }
    pub fn output_device_name(&self) -> &str {
        &self.output_device_name
    }
}

#[allow(clippy::too_many_arguments)]
fn control_thread_main(
    cmd_rx: crossbeam_channel::Receiver<Command>,
    audio_cmd_tx: Sender<AudioCommand>,
    deck_a_tx: Sender<SampleBatch>,
    deck_b_tx: Sender<SampleBatch>,
    event_tx: Sender<EngineEvent>,
    atomics: Arc<SharedAtomics>,
    device_sample_rate: u32,
    device_channels: u16,
    stream_ctl_tx: Sender<StreamCtl>,
) {
    let mut crossfade_settings = CrossfadeSettings::default();
    let mut normalization_enabled = true;
    // Track what's preloaded on the pending deck so we can skip re-fetching
    let mut pending_rating_key: i64 = 0;

    let mut stream_paused = false;
    let mut idle_since: Option<Instant> = None;
    loop {
        match cmd_rx.recv_timeout(CONTROL_IDLE_POLL) {
            Ok(cmd) => {
                // The callback only drains commands while the stream ticks — wake
                // the stream before forwarding anything, or the command would sit
                // unprocessed until the next resume.
                if stream_paused {
                    let _ = stream_ctl_tx.send(StreamCtl::Resume);
                    stream_paused = false;
                }
                idle_since = None;
                match cmd {
                    Command::Play { source, meta } => {
                        let active_rk = atomics.active_rating_key.load(Ordering::Relaxed);
                        let active_id = atomics.active_deck_id.load(Ordering::Relaxed);
                        let pending = pending_deck(&atomics);
                        info!(
                            rating_key = meta.rating_key,
                            active_rk,
                            pending_rk = pending_rating_key,
                            active_deck = active_id,
                            ?pending,
                            "PLAY command received"
                        );

                        // Check if this track is already playing (scheduler beat us to it)
                        let already_active = active_rk == meta.rating_key;
                        // Check if the preload was truncated (stream error during download)
                        let preload_broken =
                            atomics.preload_error_rk.load(Ordering::Relaxed) == meta.rating_key;
                        let preload_ready =
                            atomics.preload_ready_rk.load(Ordering::Relaxed) == meta.rating_key;
                        if already_active {
                            info!(
                                rating_key = meta.rating_key,
                                "track already active, skipping play"
                            );
                            pending_rating_key = 0;
                        // If the pending deck already has this track preloaded, just transition
                        } else if pending_rating_key == meta.rating_key
                            && pending_rating_key != 0
                            && !preload_broken
                        {
                            if preload_ready {
                                info!(rating_key = meta.rating_key, "using preloaded deck");
                                cache_ramps(&meta, &audio_cmd_tx);
                                let _ = audio_cmd_tx.send(AudioCommand::TransitionToActive {
                                    user_skip: true,
                                    rating_key: meta.rating_key,
                                    generation: deck_generation(&atomics, pending)
                                        .load(Ordering::Acquire),
                                });
                                pending_rating_key = 0;
                                atomics.preload_ready_rk.store(0, Ordering::Relaxed);
                            } else {
                                info!(
                                    rating_key = meta.rating_key,
                                    "play will activate in-flight preload when ready"
                                );
                                atomics
                                    .activate_when_ready_rk
                                    .store(meta.rating_key, Ordering::Relaxed);
                            }
                        } else {
                            if preload_broken {
                                warn!(
                                    rating_key = meta.rating_key,
                                    "preload was truncated, re-fetching"
                                );
                                atomics.preload_error_rk.store(0, Ordering::Relaxed);
                            }
                            let deck = pending;
                            let generation = next_deck_generation(&atomics, deck);
                            atomics.preload_ready_rk.store(0, Ordering::Relaxed);
                            atomics.activate_when_ready_rk.store(0, Ordering::Relaxed);
                            if active_rk == 0 {
                                atomics.set_state(EngineState::Buffering);
                                let _ = event_tx.send(EngineEvent::State {
                                    state: "buffering".into(),
                                });
                            }
                            spawn_load_job(
                                LoadIntent::Play,
                                source,
                                meta,
                                deck,
                                generation,
                                audio_cmd_tx.clone(),
                                deck_tx(deck, &deck_a_tx, &deck_b_tx).clone(),
                                event_tx.clone(),
                                atomics.clone(),
                                normalization_enabled,
                                device_sample_rate,
                                device_channels,
                            );
                            pending_rating_key = 0;
                        }
                    }
                    Command::PreloadNext { source, meta } => {
                        let active_rk = atomics.active_rating_key.load(Ordering::Relaxed);
                        let preload_error_rk = atomics.preload_error_rk.load(Ordering::Relaxed);
                        let deck = pending_deck(&atomics);
                        if should_ignore_duplicate_preload(
                            pending_rating_key,
                            active_rk,
                            preload_error_rk,
                            meta.rating_key,
                        ) {
                            debug!(
                                rating_key = meta.rating_key,
                                ?deck,
                                "ignoring duplicate preload for prepared pending track"
                            );
                        } else {
                            info!(
                                rating_key = meta.rating_key,
                                ?deck,
                                active_deck = atomics.active_deck_id.load(Ordering::Relaxed),
                                "PRELOAD command"
                            );
                            pending_rating_key = meta.rating_key;
                            atomics.preload_ready_rk.store(0, Ordering::Relaxed);
                            atomics.activate_when_ready_rk.store(0, Ordering::Relaxed);
                            atomics.preload_error_rk.store(0, Ordering::Relaxed);
                            let generation = next_deck_generation(&atomics, deck);
                            let _ = event_tx.send(EngineEvent::PreloadLoading {
                                rating_key: meta.rating_key,
                            });
                            spawn_load_job(
                                LoadIntent::Preload,
                                source,
                                meta,
                                deck,
                                generation,
                                audio_cmd_tx.clone(),
                                deck_tx(deck, &deck_a_tx, &deck_b_tx).clone(),
                                event_tx.clone(),
                                atomics.clone(),
                                normalization_enabled,
                                device_sample_rate,
                                device_channels,
                            );
                        }
                    }
                    Command::Pause => {
                        let _ = audio_cmd_tx.send(AudioCommand::Pause);
                    }
                    Command::Resume => {
                        let _ = audio_cmd_tx.send(AudioCommand::Resume);
                    }
                    Command::Stop => {
                        atomics.deck_a_generation.fetch_add(1, Ordering::Relaxed);
                        atomics.deck_b_generation.fetch_add(1, Ordering::Relaxed);
                        atomics.preload_ready_rk.store(0, Ordering::Relaxed);
                        atomics.activate_when_ready_rk.store(0, Ordering::Relaxed);
                        pending_rating_key = 0;
                        let _ = audio_cmd_tx.send(AudioCommand::Stop);
                    }
                    Command::Seek { position_ms } => {
                        handle_seek(position_ms, &audio_cmd_tx);
                    }
                    Command::SetVolume { gain } => {
                        let _ = audio_cmd_tx.send(AudioCommand::SetVolume(gain));
                    }
                    Command::SetNormalization { enabled } => {
                        normalization_enabled = enabled;
                        let _ = audio_cmd_tx.send(AudioCommand::SetNormalization(enabled));
                    }
                    Command::UpdateTrackAnalysis {
                        rating_key,
                        gain_db,
                        intro_end_ms,
                        outro_start_ms,
                        fade_start_ms,
                        silence_start_ms,
                    } => {
                        let _ = audio_cmd_tx.send(AudioCommand::UpdateTrackAnalysis {
                            rating_key,
                            gain_db,
                            intro_end_ms,
                            outro_start_ms,
                            fade_start_ms,
                            silence_start_ms,
                        });
                    }
                    Command::SetPreampGain { db } => {
                        let _ = audio_cmd_tx.send(AudioCommand::SetPreampGain(db));
                    }
                    Command::SetEq { gains_db } => {
                        let _ = audio_cmd_tx.send(AudioCommand::SetEq(gains_db));
                    }
                    Command::SetEqEnabled { enabled } => {
                        let _ = audio_cmd_tx.send(AudioCommand::SetEqEnabled(enabled));
                    }
                    Command::SetEqPostgain { db } => {
                        let _ = audio_cmd_tx.send(AudioCommand::SetEqPostgain(db));
                    }
                    Command::SetLimiterEnabled { enabled } => {
                        let _ = audio_cmd_tx.send(AudioCommand::SetLimiterEnabled(enabled));
                    }
                    Command::SetCrossfeed { enabled, preset } => {
                        let _ = audio_cmd_tx.send(AudioCommand::SetCrossfeed { enabled, preset });
                    }
                    Command::SetCrossfeedBeforeEq { before } => {
                        let _ = audio_cmd_tx.send(AudioCommand::SetCrossfeedBeforeEq(before));
                    }
                    Command::SetCrossfadeWindow { ms } => {
                        crossfade_settings.crossfade_window_ms = ms;
                        let _ = audio_cmd_tx.send(AudioCommand::UpdateCrossfadeSettings(
                            crossfade_settings.clone(),
                        ));
                    }
                    Command::SetSameAlbumCrossfade { enabled } => {
                        crossfade_settings.same_album_crossfade = enabled;
                        let _ = audio_cmd_tx.send(AudioCommand::UpdateCrossfadeSettings(
                            crossfade_settings.clone(),
                        ));
                    }
                    Command::SetSmartCrossfade { enabled } => {
                        crossfade_settings.smart_crossfade = enabled;
                        let _ = audio_cmd_tx.send(AudioCommand::UpdateCrossfadeSettings(
                            crossfade_settings.clone(),
                        ));
                    }
                    Command::SetSmartCrossfadeMax { ms } => {
                        crossfade_settings.smart_crossfade_max_ms = ms;
                        let _ = audio_cmd_tx.send(AudioCommand::UpdateCrossfadeSettings(
                            crossfade_settings.clone(),
                        ));
                    }
                    Command::SetMixrampDb { db } => {
                        crossfade_settings.mixramp_db = db;
                        let _ = audio_cmd_tx.send(AudioCommand::UpdateCrossfadeSettings(
                            crossfade_settings.clone(),
                        ));
                    }
                    Command::SetVisualizerEnabled { enabled } => {
                        let _ = audio_cmd_tx.send(AudioCommand::SetVisualizerEnabled(enabled));
                    }
                    Command::SetCacheMaxBytes { .. } | Command::ClearCache => {
                        // HeyaClient deliberately does not persist credentialed media bytes.
                    }
                    Command::DuckAndApply { duck_ms } => {
                        let _ = audio_cmd_tx.send(AudioCommand::DuckAndApply { duck_ms });
                    }
                    Command::Shutdown => {
                        atomics.deck_a_generation.fetch_add(1, Ordering::Relaxed);
                        atomics.deck_b_generation.fetch_add(1, Ordering::Relaxed);
                        info!("audio engine control thread shutting down");
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // Nothing in flight. Once playback has sat paused/stopped for
                // a grace period, suspend the OS output stream so the render
                // callback (and coreaudiod) stop burning CPU on silence.
                let idle = matches!(
                    atomics.get_state(),
                    EngineState::Paused | EngineState::Stopped
                );
                if !idle {
                    idle_since = None;
                } else if !stream_paused {
                    let since = *idle_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= STREAM_IDLE_PAUSE_AFTER {
                        let _ = stream_ctl_tx.send(StreamCtl::Pause);
                        stream_paused = true;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                info!("command channel disconnected, control thread exiting");
                break;
            }
        }
    }
}

/// Get the sample channel sender for the given deck.
fn deck_tx<'a>(
    deck: DeckId,
    a: &'a Sender<SampleBatch>,
    b: &'a Sender<SampleBatch>,
) -> &'a Sender<SampleBatch> {
    match deck {
        DeckId::A => a,
        DeckId::B => b,
    }
}

fn should_ignore_duplicate_preload(
    pending_rating_key: i64,
    active_rating_key: i64,
    preload_error_rating_key: i64,
    requested_rating_key: i64,
) -> bool {
    requested_rating_key != 0
        && pending_rating_key == requested_rating_key
        && active_rating_key != requested_rating_key
        && preload_error_rating_key != requested_rating_key
}

// ---------------------------------------------------------------------------
// Play / Preload handlers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn handle_play(
    source: &AudioSource,
    meta: &TrackMeta,
    deck: DeckId,
    generation: u64,
    audio_cmd_tx: &Sender<AudioCommand>,
    sample_tx: &Sender<SampleBatch>,
    event_tx: &Sender<EngineEvent>,
    atomics: &Arc<SharedAtomics>,
    normalization_enabled: bool,
    device_rate: u32,
    device_channels: u16,
) {
    debug!(rating_key = meta.rating_key, ?deck, "play command");

    // Cache ramps
    cache_ramps(meta, audio_cmd_tx);

    let generation_signal = deck_generation(atomics, deck);
    match fetch_and_decode_incremental(
        source,
        meta.rating_key,
        device_rate,
        device_channels,
        generation_signal.clone(),
        generation,
    ) {
        Ok(result) => {
            if generation_signal.load(Ordering::Relaxed) != generation {
                return;
            }
            if atomics.preload_error_rk.load(Ordering::Relaxed) == meta.rating_key {
                atomics.preload_error_rk.store(0, Ordering::Relaxed);
            }
            let source_rate = result.source_rate;
            let source_channels = result.source_channels;
            let has_more = result.has_more;

            let _ = event_tx.send(EngineEvent::Format {
                rating_key: meta.rating_key,
                source_sample_rate: source_rate,
                source_channels,
                output_sample_rate: device_rate,
                output_channels: device_channels,
            });

            // The client resolves track/album loudness policy and passes the
            // resulting gain. The engine applies that scalar in the DSP chain.
            let gain_db = meta.gain_db;
            let norm_gain = if normalization_enabled {
                gain_db.map_or(1.0, |db| 10.0_f32.powf(db / 20.0))
            } else {
                1.0
            };

            let sample_buffer = match allocate_pcm_buffer(
                result.initial_samples.len(),
                device_rate,
                result.channels,
            ) {
                Ok(buffer) => buffer,
                Err(message) => {
                    let _ = event_tx.send(EngineEvent::Error { message });
                    return;
                }
            };

            // Tell audio callback to prepare the deck. Its PCM allocation was
            // made here on the control thread, not inside the audio callback.
            let _ = audio_cmd_tx.send(AudioCommand::LoadDeck {
                deck,
                generation,
                meta: meta.clone(),
                sample_rate: result.sample_rate,
                channels: result.channels,
                norm_gain,
                sample_buffer,
            });

            // Don't mark as fully decoded if the stream was truncated
            let fully_decoded = !has_more && !result.aborted;
            // Send initial samples
            let _ = sample_tx.send(SampleBatch {
                rating_key: meta.rating_key,
                generation,
                samples: result.initial_samples,
                fully_decoded,
            });

            if generation_signal.load(Ordering::Acquire) != generation {
                return;
            }

            // Tell audio callback to swap pending → active
            let _ = audio_cmd_tx.send(AudioCommand::TransitionToActive {
                user_skip: true,
                rating_key: meta.rating_key,
                generation,
            });

            // Continue decoding in background
            if let Some(decoder) = result.decoder {
                spawn_background_decode(
                    decoder,
                    result.resampler,
                    meta.rating_key,
                    source_rate,
                    source_channels,
                    deck,
                    generation,
                    sample_tx.clone(),
                    event_tx.clone(),
                    atomics.clone(),
                    device_rate,
                    device_channels,
                );
            }

            if result.aborted {
                warn!(
                    rating_key = meta.rating_key,
                    "stream was truncated during play"
                );
            }
        }
        Err(e) => {
            if generation_signal.load(Ordering::Relaxed) != generation {
                return;
            }
            warn!(rating_key = meta.rating_key, error = %e, "play failed");
            let _ = event_tx.send(EngineEvent::Error {
                message: format!("load failed: {}", e),
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_preload(
    source: &AudioSource,
    meta: &TrackMeta,
    deck: DeckId,
    generation: u64,
    audio_cmd_tx: &Sender<AudioCommand>,
    sample_tx: &Sender<SampleBatch>,
    event_tx: &Sender<EngineEvent>,
    atomics: &Arc<SharedAtomics>,
    normalization_enabled: bool,
    device_rate: u32,
    device_channels: u16,
) {
    debug!(rating_key = meta.rating_key, ?deck, "preload command");

    cache_ramps(meta, audio_cmd_tx);

    let generation_signal = deck_generation(atomics, deck);
    match fetch_and_decode_incremental(
        source,
        meta.rating_key,
        device_rate,
        device_channels,
        generation_signal.clone(),
        generation,
    ) {
        Ok(result) => {
            if generation_signal.load(Ordering::Relaxed) != generation {
                return;
            }
            if atomics.preload_error_rk.load(Ordering::Relaxed) == meta.rating_key {
                atomics.preload_error_rk.store(0, Ordering::Relaxed);
            }
            let source_rate = result.source_rate;
            let source_channels = result.source_channels;
            let has_more = result.has_more;

            let _ = event_tx.send(EngineEvent::Format {
                rating_key: meta.rating_key,
                source_sample_rate: source_rate,
                source_channels,
                output_sample_rate: device_rate,
                output_channels: device_channels,
            });

            let gain_db = meta.gain_db;
            let norm_gain = if normalization_enabled {
                gain_db.map_or(1.0, |db| 10.0_f32.powf(db / 20.0))
            } else {
                1.0
            };

            let sample_buffer = match allocate_pcm_buffer(
                result.initial_samples.len(),
                device_rate,
                result.channels,
            ) {
                Ok(buffer) => buffer,
                Err(message) => {
                    atomics
                        .preload_error_rk
                        .store(meta.rating_key, Ordering::Relaxed);
                    let _ = event_tx.send(EngineEvent::PreloadError {
                        rating_key: meta.rating_key,
                        message,
                    });
                    return;
                }
            };

            let _ = audio_cmd_tx.send(AudioCommand::LoadDeck {
                deck,
                generation,
                meta: meta.clone(),
                sample_rate: result.sample_rate,
                channels: result.channels,
                norm_gain,
                sample_buffer,
            });

            let fully_decoded = !has_more && !result.aborted;
            let initial_buffered_ms = if result.sample_rate == 0 || result.channels == 0 {
                0
            } else {
                (result.initial_samples.len() as u64).saturating_mul(1000)
                    / (u64::from(result.sample_rate) * u64::from(result.channels))
            };

            let _ = sample_tx.send(SampleBatch {
                rating_key: meta.rating_key,
                generation,
                samples: result.initial_samples,
                fully_decoded,
            });

            if generation_signal.load(Ordering::Acquire) != generation {
                return;
            }

            if !result.aborted {
                atomics
                    .preload_ready_rk
                    .store(meta.rating_key, Ordering::Release);
                let _ = event_tx.send(EngineEvent::PreloadReady {
                    rating_key: meta.rating_key,
                    buffered_ms: initial_buffered_ms,
                });
                if atomics
                    .activate_when_ready_rk
                    .compare_exchange(meta.rating_key, 0, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    atomics.preload_ready_rk.store(0, Ordering::Relaxed);
                    let _ = audio_cmd_tx.send(AudioCommand::TransitionToActive {
                        user_skip: true,
                        rating_key: meta.rating_key,
                        generation,
                    });
                }
            }

            if result.aborted {
                warn!(rating_key = meta.rating_key, "preload stream was truncated");
                atomics
                    .preload_error_rk
                    .store(meta.rating_key, Ordering::Relaxed);
                let _ = event_tx.send(EngineEvent::PreloadError {
                    rating_key: meta.rating_key,
                    message: "preload stream was truncated".into(),
                });
            }

            debug!(rating_key = meta.rating_key, "preload initial batch ready");
            // NOTE: No TransitionToActive here — the audio callback's scheduler handles it

            if let Some(decoder) = result.decoder {
                spawn_background_decode(
                    decoder,
                    result.resampler,
                    meta.rating_key,
                    source_rate,
                    source_channels,
                    deck,
                    generation,
                    sample_tx.clone(),
                    event_tx.clone(),
                    atomics.clone(),
                    device_rate,
                    device_channels,
                );
            }
        }
        Err(e) => {
            if generation_signal.load(Ordering::Relaxed) != generation {
                return;
            }
            warn!(rating_key = meta.rating_key, error = %e, "preload failed");
            atomics
                .preload_error_rk
                .store(meta.rating_key, Ordering::Relaxed);
            let _ = event_tx.send(EngineEvent::PreloadError {
                rating_key: meta.rating_key,
                message: format!("preload failed: {}", e),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Seek handler
// ---------------------------------------------------------------------------

fn handle_seek(position_ms: u64, audio_cmd_tx: &Sender<AudioCommand>) {
    // Do not gate this on atomically-published duration/format state. A load
    // deliberately queues Play followed by Seek, and the control thread can
    // reach this point before the audio callback has published either value.
    // AudioCommand ordering guarantees the callback sees LoadDeck and the deck
    // transition first; the callback is the authoritative place to validate
    // the active deck and decide between an in-buffer or decoder seek.
    let _ = audio_cmd_tx.send(AudioCommand::SeekInBuffer {
        position: position_ms as usize, // Encode as ms, callback converts
    });

    // Also reset DSP on seek
    let _ = audio_cmd_tx.send(AudioCommand::ResetDsp);
}

// ---------------------------------------------------------------------------
// Background decode
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn spawn_background_decode(
    mut decoder: decode::DecoderSetup,
    mut resampler: Option<StreamingResampler>,
    rating_key: i64,
    source_rate: u32,
    source_channels: u16,
    deck: DeckId,
    generation: u64,
    sample_tx: Sender<SampleBatch>,
    event_tx: Sender<EngineEvent>,
    atomics: Arc<SharedAtomics>,
    device_sample_rate: u32,
    device_channels: u16,
) {
    let seek_signal = match deck {
        DeckId::A => atomics.deck_a_seek_ms.clone(),
        DeckId::B => atomics.deck_b_seek_ms.clone(),
    };
    let gen_signal = match deck {
        DeckId::A => atomics.deck_a_generation.clone(),
        DeckId::B => atomics.deck_b_generation.clone(),
    };

    std::thread::Builder::new()
        .name(format!("audio-bgdec-{}", rating_key))
        .spawn(move || {
            let batch_size =
                source_rate as usize * source_channels as usize * BACKGROUND_DECODE_BATCH_SECONDS;
            let mut current_gen = generation;
            let mut at_eof = false;

            loop {
                // Check for seek request
                let seek_ms = seek_signal.load(Ordering::Relaxed);
                if seek_ms >= 0 {
                    seek_signal.store(-1, Ordering::Relaxed);
                    current_gen = gen_signal.load(Ordering::Acquire);
                    let seek_secs = seek_ms as f64 / 1000.0;
                    log::info!(
                        "native audio decoder seek started track={} deck={:?} position_seconds={seek_secs:.3}",
                        rating_key,
                        deck,
                    );
                    debug!(
                        rating_key,
                        seek_secs = format!("{:.2}", seek_secs),
                        "bg decode: seeking"
                    );

                    use symphonia::core::formats::{SeekMode, SeekTo};
                    use symphonia::core::units::Time;
                    match decoder.format.seek(
                        SeekMode::Coarse,
                        SeekTo::Time {
                            time: Time {
                                seconds: seek_secs as u64,
                                frac: seek_secs.fract(),
                            },
                            track_id: None,
                        },
                    ) {
                        Ok(seeked) => {
                            decoder.decoder.reset();
                            decoder.finished = false;
                            at_eof = false;
                            if source_rate != device_sample_rate {
                                resampler = match StreamingResampler::new(
                                    source_channels,
                                    source_rate,
                                    device_sample_rate,
                                ) {
                                    Ok(resampler) => Some(resampler),
                                    Err(error) => {
                                        warn!(rating_key, %error, "could not reset resampler after seek");
                                        report_decode_failure(
                                            &event_tx,
                                            &atomics,
                                            &gen_signal,
                                            current_gen,
                                            rating_key,
                                            format!("could not reset resampler after seek: {error}"),
                                        );
                                        return;
                                    }
                                };
                            }
                            debug!(
                                rating_key,
                                seeked_ts = seeked.actual_ts,
                                gen = current_gen,
                                "bg decode: seek done"
                            );
                            log::info!(
                                "native audio decoder seek completed track={} deck={:?} requested_ts={} actual_ts={} generation={}",
                                rating_key,
                                deck,
                                seeked.required_ts,
                                seeked.actual_ts,
                                current_gen,
                            );
                        }
                        Err(e) => {
                            warn!(rating_key, error = %e, "bg decode: seek failed");
                            log::warn!(
                                "native audio decoder seek failed track={} deck={:?}: {}",
                                rating_key,
                                deck,
                                e,
                            );
                            report_decode_failure(
                                &event_tx,
                                &atomics,
                                &gen_signal,
                                current_gen,
                                rating_key,
                                format!("audio seek failed: {e}"),
                            );
                            return;
                        }
                    }
                }

                // Loading a different track into this physical deck advances
                // its generation. Stop the superseded decoder instead of
                // wasting I/O/CPU and filling the channel with stale batches.
                if gen_signal.load(Ordering::Relaxed) != current_gen {
                    debug!(rating_key, ?deck, current_gen, "bg decode: superseded");
                    return;
                }

                // Keep the decoder alive after EOF so a later backward seek
                // can refill the bounded PCM ring without retaining the whole
                // decoded track. The generation check above still tears this
                // worker down immediately when its physical deck is reused.
                if at_eof {
                    std::thread::park_timeout(Duration::from_millis(20));
                    continue;
                }

                match decode::decode_batch(&mut decoder, batch_size) {
                    Ok(batch) if !batch.is_empty() => {
                        let mut samples = if let Some(resampler) = resampler.as_mut() {
                            match resampler.process(&batch) {
                                Ok(samples) => samples,
                                Err(error) => {
                                    warn!(rating_key, %error, "background resampling failed");
                                    report_decode_failure(
                                        &event_tx,
                                        &atomics,
                                        &gen_signal,
                                        current_gen,
                                        rating_key,
                                        format!("background resampling failed: {error}"),
                                    );
                                    return;
                                }
                            }
                        } else {
                            batch
                        };

                        if source_channels == 1 && device_channels >= 2 {
                            let mut stereo = Vec::with_capacity(samples.len() * 2);
                            for &s in &samples {
                                stereo.push(s);
                                stereo.push(s);
                            }
                            samples = stereo;
                        }

                        if sample_tx
                            .send(SampleBatch {
                                rating_key,
                                generation: current_gen,
                                samples,
                                fully_decoded: false,
                            })
                            .is_err()
                        {
                            return; // Channel closed
                        }
                    }
                    Ok(_) => {
                        if decoder.aborted {
                            // Stream was truncated (download error) — don't mark
                            // as fully decoded so is_finished() won't fire and
                            // cause premature auto-advance.
                            warn!(rating_key, "bg decode: incomplete (stream truncated)");
                            report_decode_failure(
                                &event_tx,
                                &atomics,
                                &gen_signal,
                                current_gen,
                                rating_key,
                                "audio stream was truncated".into(),
                            );
                        } else {
                            debug!(rating_key, "bg decode: complete");
                            let mut samples = match resampler.as_mut() {
                                Some(resampler) => match resampler.finish() {
                                    Ok(samples) => samples,
                                    Err(error) => {
                                        warn!(rating_key, %error, "could not flush audio resampler");
                                        report_decode_failure(
                                            &event_tx,
                                            &atomics,
                                            &gen_signal,
                                            current_gen,
                                            rating_key,
                                            format!("could not flush audio resampler: {error}"),
                                        );
                                        return;
                                    }
                                },
                                None => Vec::new(),
                            };
                            if source_channels == 1 && device_channels >= 2 {
                                let mut stereo = Vec::with_capacity(samples.len() * 2);
                                for &sample in &samples {
                                    stereo.extend_from_slice(&[sample, sample]);
                                }
                                samples = stereo;
                            }
                            let _ = sample_tx.send(SampleBatch {
                                rating_key,
                                generation: current_gen,
                                samples,
                                fully_decoded: true,
                            });
                            at_eof = true;
                            continue;
                        }
                        return;
                    }
                    Err(e) => {
                        warn!(rating_key, error = %e, "bg decode: error");
                        report_decode_failure(
                            &event_tx,
                            &atomics,
                            &gen_signal,
                            current_gen,
                            rating_key,
                            format!("background decode failed: {e}"),
                        );
                        return;
                    }
                }
            }
        })
        .ok();
}

fn report_decode_failure(
    event_tx: &Sender<EngineEvent>,
    atomics: &SharedAtomics,
    generation_signal: &AtomicU64,
    generation: u64,
    rating_key: i64,
    message: String,
) {
    if generation_signal.load(Ordering::Acquire) != generation {
        return;
    }
    atomics
        .preload_error_rk
        .store(rating_key, Ordering::Release);
    let event = if atomics.active_rating_key.load(Ordering::Acquire) == rating_key {
        EngineEvent::Error { message }
    } else {
        EngineEvent::PreloadError {
            rating_key,
            message,
        }
    };
    let _ = event_tx.send(event);
}

// ---------------------------------------------------------------------------
// Streaming fetch + decode
// ---------------------------------------------------------------------------

/// Result of fetch + incremental decode.
struct IncrementalDecodeResult {
    initial_samples: Vec<f32>,
    sample_rate: u32,
    channels: u16,
    has_more: bool,
    decoder: Option<decode::DecoderSetup>,
    source_rate: u32,
    source_channels: u16,
    resampler: Option<StreamingResampler>,
    aborted: bool,
}

/// Start a credential-scoped HTTP download and decode enough PCM to begin.
/// Redirects are rejected so the fixed grant can never cross origins.
fn fetch_and_decode_incremental(
    source: &AudioSource,
    rating_key: i64,
    device_rate: u32,
    device_channels: u16,
    generation_signal: Arc<AtomicU64>,
    generation: u64,
) -> Result<IncrementalDecodeResult, String> {
    use self::deck::streaming::{SharedBuffer, StreamingReader};

    let shared = SharedBuffer::new(None)
        .map_err(|error| format!("could not create audio download spool: {error}"))?;
    let writer = shared.clone();
    let source = source.clone();
    // Symphonia's demuxers determine seekability while probing. Do not race
    // that probe against the HTTP worker learning Content-Length; otherwise a
    // perfectly seekable file is permanently classified as a stream and every
    // resume/manual seek fails. Chunked responses fall back to being fully
    // downloaded so their final byte length is still known before probing.
    let (response_ready_tx, response_ready_rx) = bounded::<Result<u64, &'static str>>(1);
    std::thread::Builder::new()
        .name(format!("heya-audio-fetch-{rating_key}"))
        .spawn(move || {
            if generation_signal.load(Ordering::Relaxed) != generation {
                let _ = response_ready_tx.send(Err("native audio load was cancelled"));
                writer.abort();
                return;
            }
            let client = match reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(format!("HeyaClient/{}", env!("CARGO_PKG_VERSION")))
                .build()
            {
                Ok(client) => client,
                Err(error) => {
                    warn!(rating_key, %error, "could not create native audio fetch client");
                    let _ = response_ready_tx.send(Err("could not create native audio client"));
                    writer.abort();
                    return;
                }
            };
            let mut response = match client
                .get(&source.media_url)
                .header(PLAYBACK_GRANT_HEADER, source.playback_grant.as_str())
                .send()
            {
                Ok(response) => response,
                Err(error) => {
                    let kind = if error.is_timeout() {
                        "timeout"
                    } else if error.is_connect() {
                        "connect"
                    } else if error.is_request() {
                        "request"
                    } else {
                        "transport"
                    };
                    // reqwest's Display output can contain the requested URL;
                    // publish only a coarse category across this boundary.
                    warn!(rating_key, kind, "native audio fetch failed");
                    let _ = response_ready_tx.send(Err("native audio fetch failed"));
                    writer.abort();
                    return;
                }
            };
            if !response.status().is_success() {
                warn!(rating_key, status = %response.status(), "native audio fetch was rejected");
                let _ = response_ready_tx.send(Err("native audio fetch was rejected"));
                writer.abort();
                return;
            }
            let content_length = response.content_length();
            if let Some(length) = content_length {
                if length > MAX_COMPRESSED_SPOOL_BYTES {
                    let _ = response_ready_tx.send(Err("native audio source is too large"));
                    writer.abort();
                    return;
                }
                writer.set_content_length(length);
                let _ = response_ready_tx.send(Ok(length));
            }
            let mut chunk = vec![0_u8; 128 * 1024];
            let mut received = 0_u64;
            loop {
                if generation_signal.load(Ordering::Relaxed) != generation
                    || writer.is_abandoned()
                {
                    writer.abort();
                    return;
                }
                match response.read(&mut chunk) {
                    Ok(0) => {
                        if content_length.is_none() {
                            writer.set_content_length(received);
                            let _ = response_ready_tx.send(Ok(received));
                        }
                        writer.finish();
                        return;
                    }
                    Ok(count) => {
                        received = received.saturating_add(count as u64);
                        if received > MAX_COMPRESSED_SPOOL_BYTES {
                            if content_length.is_none() {
                                let _ = response_ready_tx.send(Err("native audio source is too large"));
                            }
                            writer.abort();
                            return;
                        }
                        if let Err(error) = writer.push(&chunk[..count]) {
                            warn!(rating_key, kind = ?error.kind(), "could not spool native audio stream");
                            if content_length.is_none() {
                                let _ = response_ready_tx.send(Err("could not spool audio stream"));
                            }
                            writer.abort();
                            return;
                        }
                    }
                    Err(error) => {
                        warn!(rating_key, kind = ?error.kind(), "native audio stream was truncated");
                        if content_length.is_none() {
                            let _ = response_ready_tx.send(Err("native audio stream was truncated"));
                        }
                        writer.abort();
                        return;
                    }
                }
            }
        })
        .map_err(|error| format!("could not start native audio download: {error}"))?;

    let content_length = response_ready_rx
        .recv_timeout(Duration::from_secs(60))
        .map_err(|_| "native audio response metadata timed out".to_string())?
        .map_err(str::to_string)?;
    let initial_bytes = shared
        .wait_until_readable()
        .map_err(|error| format!("native audio response body is unavailable: {error}"))?;
    log::info!(
        "native audio response ready track={} content_length={} initial_bytes={}",
        rating_key,
        content_length,
        initial_bytes,
    );

    let reader = StreamingReader::new(shared);
    let hint = source.format_hint.as_deref().filter(|value| {
        !value.is_empty()
            && value.len() <= 16
            && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
    });
    let mut setup = decode::probe_stream(reader, hint).map_err(|error| error.to_string())?;
    let mut result = decode_initial_batch(&mut setup, device_rate, device_channels)?;
    result.decoder = (!setup.finished).then_some(setup);
    Ok(result)
}

fn decode_initial_batch(
    setup: &mut decode::DecoderSetup,
    device_rate: u32,
    device_channels: u16,
) -> Result<IncrementalDecodeResult, String> {
    let source_rate = setup.sample_rate;
    let source_channels = setup.channels;

    let initial_batch_size =
        source_rate as usize * source_channels as usize * INITIAL_DECODE_SECONDS;
    let mut samples =
        decode::decode_batch(setup, initial_batch_size).map_err(|e| format!("{}", e))?;

    debug!(
        source_rate,
        source_channels,
        initial_samples = samples.len(),
        finished = setup.finished,
        "initial decode batch ready"
    );

    let mut resampler = if source_rate != device_rate {
        Some(StreamingResampler::new(
            source_channels,
            source_rate,
            device_rate,
        )?)
    } else {
        None
    };
    if let Some(streaming_resampler) = resampler.as_mut() {
        samples = streaming_resampler.process(&samples)?;
        if setup.finished && !setup.aborted {
            samples.extend(streaming_resampler.finish()?);
        }
    }

    let out_channels = if source_channels == 1 && device_channels >= 2 {
        let mut stereo = Vec::with_capacity(samples.len() * 2);
        for &s in &samples {
            stereo.push(s);
            stereo.push(s);
        }
        samples = stereo;
        2
    } else {
        source_channels
    };

    Ok(IncrementalDecodeResult {
        initial_samples: samples,
        sample_rate: device_rate,
        channels: out_channels,
        has_more: !setup.finished,
        decoder: None, // caller fills this
        source_rate,
        source_channels,
        resampler,
        aborted: setup.aborted,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn allocate_pcm_buffer(
    initial_samples: usize,
    sample_rate: u32,
    channels: u16,
) -> Result<Vec<f32>, String> {
    let capacity = (sample_rate as usize)
        .saturating_mul(channels.max(1) as usize)
        .saturating_mul(PCM_BUFFER_SECONDS)
        .max(initial_samples);
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(capacity)
        .map_err(|_| "could not reserve decoded PCM memory for this track".to_string())?;
    // Commit and initialize the ring's pages on the loader thread. Growing a
    // merely-reserved Vec from the audio callback can fault in memory and
    // create exactly the sort of intermittent deadline miss this buffer is
    // intended to prevent.
    buffer.resize(capacity, 0.0);
    Ok(buffer)
}

fn cache_ramps(meta: &TrackMeta, audio_cmd_tx: &Sender<AudioCommand>) {
    if meta.start_ramp.is_some() || meta.end_ramp.is_some() {
        let _ = audio_cmd_tx.send(AudioCommand::CacheRamps {
            rating_key: meta.rating_key,
            ramps: TrackRamps {
                start_ramp: parse_ramp(meta.start_ramp.as_deref()),
                end_ramp: parse_ramp(meta.end_ramp.as_deref()),
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_seek_is_queued_before_duration_is_published() {
        let atomics = Arc::new(SharedAtomics::new());
        assert_eq!(atomics.duration_ms.load(Ordering::Relaxed), 0);
        let (tx, rx) = bounded(2);

        handle_seek(104_000, &tx);

        match rx.recv().expect("seek command") {
            AudioCommand::SeekInBuffer { position } => assert_eq!(position, 104_000),
            _ => panic!("expected seek command"),
        }
        assert!(matches!(
            rx.recv().expect("DSP reset"),
            AudioCommand::ResetDsp
        ));
    }

    #[test]
    fn clock_snapshot_reads_the_callback_atomics_directly() {
        let atomics = SharedAtomics::new();
        atomics.set_state(EngineState::Playing);
        atomics.position_frames.store(96_000, Ordering::Relaxed);
        atomics.duration_ms.store(10_000, Ordering::Relaxed);
        atomics.active_rating_key.store(42, Ordering::Relaxed);

        assert_eq!(
            read_engine_clock(&atomics, 48_000),
            AudioEngineClock {
                state: EngineState::Playing,
                position_seconds: 2.0,
                duration_seconds: 10.0,
                active_track_id: Some(42),
            }
        );
    }

    #[test]
    fn duplicate_preload_is_ignored_until_track_becomes_active_or_fails() {
        assert!(should_ignore_duplicate_preload(42, 7, 0, 42));
        assert!(!should_ignore_duplicate_preload(42, 42, 0, 42));
        assert!(!should_ignore_duplicate_preload(42, 7, 42, 42));
        assert!(!should_ignore_duplicate_preload(42, 7, 0, 43));
    }

    #[test]
    fn pcm_allocation_is_bounded_at_a_96khz_device_rate() {
        let buffer = allocate_pcm_buffer(96_000 * 2 * INITIAL_DECODE_SECONDS, 96_000, 2)
            .expect("allocate PCM ring backing");
        assert_eq!(buffer.len(), 96_000 * 2 * PCM_BUFFER_SECONDS);
        assert!(buffer.capacity() >= buffer.len());
        assert!(buffer.capacity() < 96_000 * 2 * (PCM_BUFFER_SECONDS + 1));
    }
}
