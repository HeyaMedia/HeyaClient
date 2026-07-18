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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use tracing::{debug, info, warn};

use self::callback::{AudioCallbackState, SharedAtomics};
use self::command::{AudioCommand, Command, SampleBatch};
use self::crossfade::types::{parse_ramp, CrossfadeSettings, TrackRamps};
use self::deck::decode;
use self::event::EngineEvent;
use self::output::resample::resample_buffer;
use self::output::{CpalOutput, OutputRequest};
use self::types::{AudioSource, DeckId, EngineState, TrackMeta};
use self::visualizer::VisualizerProcessor;

pub const PLAYBACK_GRANT_HEADER: &str = "X-Heya-Playback-Grant";
const SAMPLE_BATCH_CHANNEL_CAPACITY: usize = 8;
const INITIAL_DECODE_SECONDS: usize = 3;
const BACKGROUND_DECODE_BATCH_SECONDS: usize = 1;
const VISUALIZER_BUFFER_POOL_SIZE: usize = 8;
const VISUALIZER_BUFFER_CAPACITY_SAMPLES: usize = 64 * 1024;
const RETIRED_PCM_BUFFER_CAPACITY: usize = 8;

pub struct AudioEngine {
    cmd_tx: Sender<Command>,
    atomics: Arc<SharedAtomics>,
    event_rx: Receiver<EngineEvent>,
    running: Arc<AtomicBool>,
    stream_stop_tx: Sender<()>,
    device_sample_rate: u32,
    device_channels: u16,
    output_device_id: String,
    output_device_name: String,
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = self.cmd_tx.try_send(Command::Shutdown);
        let _ = self.stream_stop_tx.try_send(());
    }
}

impl AudioEngine {
    pub fn start(preferred_device_id: Option<&str>) -> Result<Self, String> {
        Self::start_with_output(OutputRequest::Shared, preferred_device_id)
    }

    pub fn start_exclusive(
        sample_rate: u32,
        channels: u16,
        preferred_device_id: Option<&str>,
    ) -> Result<Self, String> {
        Self::start_with_output(
            OutputRequest::Exclusive {
                sample_rate,
                channels,
            },
            preferred_device_id,
        )
    }

    fn start_with_output(
        output_request: OutputRequest,
        preferred_device_id: Option<&str>,
    ) -> Result<Self, String> {
        let atomics = Arc::new(SharedAtomics::new());
        let running = Arc::new(AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = bounded::<Command>(64);
        let (audio_cmd_tx, audio_cmd_rx) = bounded::<AudioCommand>(256);
        // Bound producer lead so a fast decoder cannot queue an entire track
        // for the callback to copy in one deadline.
        let (deck_a_tx, deck_a_rx) = bounded::<SampleBatch>(SAMPLE_BATCH_CHANNEL_CAPACITY);
        let (deck_b_tx, deck_b_rx) = bounded::<SampleBatch>(SAMPLE_BATCH_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = bounded::<EngineEvent>(256);
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
        let output = CpalOutput::open(
            callback,
            output_request,
            preferred_device_id,
            event_tx.clone(),
        )
        .map_err(|error| format!("failed to open audio output: {error}"))?;
        let device_sample_rate = output.sample_rate;
        let device_channels = output.channels;
        let output_device_id = output.device_id.clone();
        let output_device_name = output.device_name.clone();
        info!(
            device = %output_device_name,
            device_id = %output_device_id,
            sample_rate = device_sample_rate,
            channels = device_channels,
            "native audio engine started"
        );

        let (stream_stop_tx, stream_stop_rx) = bounded::<()>(1);
        std::thread::Builder::new()
            .name("heya-audio-output".into())
            .spawn(move || {
                let _output = output;
                let _ = stream_stop_rx.recv();
            })
            .map_err(|error| format!("failed to start audio output holder: {error}"))?;

        let running_position = running.clone();
        let atomics_position = atomics.clone();
        let atomics_for_position_thread = atomics_position.clone();
        let position_events = event_tx.clone();
        std::thread::Builder::new()
            .name("heya-audio-position".into())
            .spawn(move || {
                while running_position.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(250));
                    if atomics_for_position_thread.get_state() == EngineState::Stopped {
                        continue;
                    }
                    let frames = atomics_for_position_thread
                        .position_frames
                        .load(Ordering::Relaxed);
                    let duration_ms = atomics_for_position_thread
                        .duration_ms
                        .load(Ordering::Relaxed);
                    let divisor = u64::from(device_sample_rate);
                    if divisor == 0 {
                        continue;
                    }
                    let position_ms = ((frames as f64 / divisor as f64) * 1000.0) as u64;
                    let _ = position_events.try_send(EngineEvent::Position {
                        position_ms: position_ms.min(duration_ms),
                        duration_ms,
                    });
                }
            })
            .map_err(|error| format!("failed to start position publisher: {error}"))?;

        let running_visualizer = running.clone();
        let visualizer_events = event_tx.clone();
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
                        let _ = visualizer_events.try_send(EngineEvent::VisFrame {
                            samples: time_domain,
                            frequency_bins,
                        });
                    }
                }
            })
            .map_err(|error| format!("failed to start visualizer: {error}"))?;

        let running_control = running.clone();
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
                );
                running_control.store(false, Ordering::Release);
            })
            .map_err(|error| format!("failed to start audio control: {error}"))?;

        Ok(Self {
            cmd_tx,
            atomics: atomics_position,
            event_rx,
            running,
            stream_stop_tx,
            device_sample_rate,
            device_channels,
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
    pub fn atomics(&self) -> &Arc<SharedAtomics> {
        &self.atomics
    }
    pub fn device_sample_rate(&self) -> u32 {
        self.device_sample_rate
    }
    pub fn device_channels(&self) -> u16 {
        self.device_channels
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
) {
    let mut crossfade_settings = CrossfadeSettings::default();
    let mut normalization_enabled = true;
    // Track what's preloaded on the pending deck so we can skip re-fetching
    let mut pending_rating_key: i64 = 0;

    /// Read which deck is currently active from the shared atomic.
    /// The pending deck (for preloads/next play) is always the other one.
    fn pending_deck(atomics: &SharedAtomics) -> DeckId {
        let active = atomics.active_deck_id.load(Ordering::Relaxed);
        if active == 0 {
            DeckId::B
        } else {
            DeckId::A
        }
    }

    /// Eagerly update active_deck_id after sending TransitionToActive, so
    /// subsequent commands see the correct pending deck without waiting for
    /// the audio callback to process the swap.
    fn set_active_eagerly(atomics: &SharedAtomics, deck: DeckId) {
        let id = match deck {
            DeckId::A => 0u8,
            DeckId::B => 1u8,
        };
        atomics.active_deck_id.store(id, Ordering::Relaxed);
    }

    loop {
        match cmd_rx.recv() {
            Ok(cmd) => match cmd {
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
                        info!(rating_key = meta.rating_key, "using preloaded deck");
                        cache_ramps(&meta, &audio_cmd_tx);
                        let _ =
                            audio_cmd_tx.send(AudioCommand::TransitionToActive { user_skip: true });
                        set_active_eagerly(&atomics, pending);
                        pending_rating_key = 0;
                    } else {
                        if preload_broken {
                            warn!(
                                rating_key = meta.rating_key,
                                "preload was truncated, re-fetching"
                            );
                            atomics.preload_error_rk.store(0, Ordering::Relaxed);
                        }
                        let deck = pending;
                        handle_play(
                            &source,
                            &meta,
                            deck,
                            &audio_cmd_tx,
                            deck_tx(deck, &deck_a_tx, &deck_b_tx),
                            &event_tx,
                            &atomics,
                            &crossfade_settings,
                            normalization_enabled,
                            device_sample_rate,
                            device_channels,
                        );
                        // Eagerly reflect the upcoming deck swap
                        set_active_eagerly(&atomics, deck);
                        pending_rating_key = 0;
                    }
                }
                Command::PreloadNext { source, meta } => {
                    let deck = pending_deck(&atomics);
                    info!(
                        rating_key = meta.rating_key,
                        ?deck,
                        active_deck = atomics.active_deck_id.load(Ordering::Relaxed),
                        "PRELOAD command"
                    );
                    pending_rating_key = meta.rating_key;
                    handle_preload(
                        &source,
                        &meta,
                        deck,
                        &audio_cmd_tx,
                        deck_tx(deck, &deck_a_tx, &deck_b_tx),
                        &event_tx,
                        &atomics,
                        &crossfade_settings,
                        normalization_enabled,
                        device_sample_rate,
                        device_channels,
                    );
                }
                Command::Pause => {
                    let _ = audio_cmd_tx.send(AudioCommand::Pause);
                }
                Command::Resume => {
                    let _ = audio_cmd_tx.send(AudioCommand::Resume);
                }
                Command::Stop => {
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
                    info!("audio engine control thread shutting down");
                    break;
                }
            },
            Err(_) => {
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

// ---------------------------------------------------------------------------
// Play / Preload handlers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn handle_play(
    source: &AudioSource,
    meta: &TrackMeta,
    deck: DeckId,
    audio_cmd_tx: &Sender<AudioCommand>,
    sample_tx: &Sender<SampleBatch>,
    event_tx: &Sender<EngineEvent>,
    atomics: &Arc<SharedAtomics>,
    _xfade: &CrossfadeSettings,
    normalization_enabled: bool,
    device_rate: u32,
    device_channels: u16,
) {
    debug!(rating_key = meta.rating_key, ?deck, "play command");

    // Cache ramps
    cache_ramps(meta, audio_cmd_tx);

    // Set buffering state
    atomics.set_state(EngineState::Buffering);
    let _ = event_tx.send(EngineEvent::State {
        state: "buffering".into(),
    });

    // Increment generation to invalidate any old bg decode threads writing to this deck
    let generation = match deck {
        DeckId::A => atomics.deck_a_generation.fetch_add(1, Ordering::Relaxed) + 1,
        DeckId::B => atomics.deck_b_generation.fetch_add(1, Ordering::Relaxed) + 1,
    };

    match fetch_and_decode_incremental(source, meta.rating_key, device_rate, device_channels) {
        Ok(result) => {
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

            // Pre-compute expected samples for pre-allocation
            let expected_total =
                if meta.duration_ms > 0 && result.sample_rate > 0 && result.channels > 0 {
                    (meta.duration_ms as f64 / 1000.0
                        * result.sample_rate as f64
                        * result.channels as f64) as usize
                } else {
                    result.initial_samples.len()
                };

            let sample_buffer = match allocate_pcm_buffer(
                expected_total,
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

            // Tell audio callback to swap pending → active
            let _ = audio_cmd_tx.send(AudioCommand::TransitionToActive { user_skip: true });

            // Continue decoding in background
            if let Some(decoder) = result.decoder {
                spawn_background_decode(
                    decoder,
                    meta.rating_key,
                    source_rate,
                    source_channels,
                    deck,
                    generation,
                    sample_tx.clone(),
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
    audio_cmd_tx: &Sender<AudioCommand>,
    sample_tx: &Sender<SampleBatch>,
    event_tx: &Sender<EngineEvent>,
    atomics: &Arc<SharedAtomics>,
    _xfade: &CrossfadeSettings,
    normalization_enabled: bool,
    device_rate: u32,
    device_channels: u16,
) {
    debug!(rating_key = meta.rating_key, ?deck, "preload command");

    cache_ramps(meta, audio_cmd_tx);

    // Increment generation to invalidate any old bg decode threads writing to this deck
    let generation = match deck {
        DeckId::A => atomics.deck_a_generation.fetch_add(1, Ordering::Relaxed) + 1,
        DeckId::B => atomics.deck_b_generation.fetch_add(1, Ordering::Relaxed) + 1,
    };

    match fetch_and_decode_incremental(source, meta.rating_key, device_rate, device_channels) {
        Ok(result) => {
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

            let expected_total =
                if meta.duration_ms > 0 && result.sample_rate > 0 && result.channels > 0 {
                    (meta.duration_ms as f64 / 1000.0
                        * result.sample_rate as f64
                        * result.channels as f64) as usize
                } else {
                    result.initial_samples.len()
                };

            let sample_buffer = match allocate_pcm_buffer(
                expected_total,
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
                meta: meta.clone(),
                sample_rate: result.sample_rate,
                channels: result.channels,
                norm_gain,
                sample_buffer,
            });

            let fully_decoded = !has_more && !result.aborted;

            let _ = sample_tx.send(SampleBatch {
                rating_key: meta.rating_key,
                generation,
                samples: result.initial_samples,
                fully_decoded,
            });

            if result.aborted {
                warn!(rating_key = meta.rating_key, "preload stream was truncated");
                atomics
                    .preload_error_rk
                    .store(meta.rating_key, Ordering::Relaxed);
            }

            debug!(rating_key = meta.rating_key, "preload initial batch ready");
            // NOTE: No TransitionToActive here — the audio callback's scheduler handles it

            if let Some(decoder) = result.decoder {
                spawn_background_decode(
                    decoder,
                    meta.rating_key,
                    source_rate,
                    source_channels,
                    deck,
                    generation,
                    sample_tx.clone(),
                    atomics.clone(),
                    device_rate,
                    device_channels,
                );
            }
        }
        Err(e) => {
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
    rating_key: i64,
    source_rate: u32,
    source_channels: u16,
    deck: DeckId,
    generation: u64,
    sample_tx: Sender<SampleBatch>,
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

            loop {
                // Check for seek request
                let seek_ms = seek_signal.load(Ordering::Relaxed);
                if seek_ms >= 0 {
                    seek_signal.store(-1, Ordering::Relaxed);
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
                            // Update generation — new batches get the new generation
                            current_gen = gen_signal.load(Ordering::Relaxed);
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
                        }
                    }
                }

                match decode::decode_batch(&mut decoder, batch_size) {
                    Ok(batch) if !batch.is_empty() => {
                        let mut samples = batch;

                        if source_rate != device_sample_rate {
                            if let Some(resampled) = resample_buffer(
                                &samples,
                                source_channels,
                                source_rate,
                                device_sample_rate,
                            ) {
                                samples = resampled;
                            }
                        }

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
                            atomics
                                .preload_error_rk
                                .store(rating_key, Ordering::Relaxed);
                        } else {
                            debug!(rating_key, "bg decode: complete");
                            let _ = sample_tx.send(SampleBatch {
                                rating_key,
                                generation: current_gen,
                                samples: Vec::new(),
                                fully_decoded: true,
                            });
                        }
                        return;
                    }
                    Err(e) => {
                        warn!(rating_key, error = %e, "bg decode: error");
                        atomics
                            .preload_error_rk
                            .store(rating_key, Ordering::Relaxed);
                        return;
                    }
                }
            }
        })
        .ok();
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
    aborted: bool,
}

/// Start a credential-scoped HTTP download and decode enough PCM to begin.
/// Redirects are rejected so the fixed grant can never cross origins.
fn fetch_and_decode_incremental(
    source: &AudioSource,
    rating_key: i64,
    device_rate: u32,
    device_channels: u16,
) -> Result<IncrementalDecodeResult, String> {
    use self::deck::streaming::{SharedBuffer, StreamingReader};

    let shared = SharedBuffer::new(None);
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
            let client = match reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(15))
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
                writer.set_content_length(length);
                let _ = response_ready_tx.send(Ok(length));
            }
            let mut chunk = vec![0_u8; 128 * 1024];
            let mut received = 0_u64;
            loop {
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
                        writer.push(&chunk[..count]);
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
    log::info!(
        "native audio response ready track={} content_length={}",
        rating_key,
        content_length,
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

    if source_rate != device_rate {
        if let Some(resampled) =
            resample_buffer(&samples, source_channels, source_rate, device_rate)
        {
            samples = resampled;
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
        aborted: setup.aborted,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn allocate_pcm_buffer(
    expected_samples: usize,
    initial_samples: usize,
    sample_rate: u32,
    channels: u16,
) -> Result<Vec<f32>, String> {
    let headroom =
        sample_rate as usize * channels.max(1) as usize * BACKGROUND_DECODE_BATCH_SECONDS;
    let capacity = expected_samples
        .max(initial_samples)
        .saturating_add(headroom);
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(capacity)
        .map_err(|_| "could not reserve decoded PCM memory for this track".to_string())?;
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
}
