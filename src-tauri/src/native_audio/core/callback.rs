//! Audio callback state — owned exclusively by the cpal audio callback closure.
//!
//! Zero mutexes on the audio path. All communication is lock-free:
//! - Commands arrive via `crossbeam_channel::Receiver<AudioCommand>`
//! - Decoded samples arrive via per-deck `crossbeam_channel::Receiver<SampleBatch>`
//! - Position/state written to atomics
//! - Events and visualizer data sent via `crossbeam_channel::Sender`

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use tracing::debug;

use super::command::{AudioCommand, SampleBatch};
use super::crossfade::scheduler::{Scheduler, SchedulerAction, SchedulerMode};
use super::crossfade::types::{CrossfadeParams, CrossfadeSettings, TrackRamps, TransitionPlan};
use super::crossfade::{compute_skip_duck, compute_transition};
use super::deck::manager::{DeckManager, DeckState, FadeCurve};
use super::dsp::DspChain;
use super::event::EngineEvent;
use super::output::mixer;
use super::types::{DeckId, EngineState};

const MAX_SAMPLE_BATCHES_PER_DECK_CALLBACK: usize = 1;
const MAX_SCANNED_SAMPLE_BATCHES_PER_DECK_CALLBACK: usize = 16;
const REBUFFER_TARGET_MS: u64 = 2_000;
const TRANSITION_READY_MS: u64 = REBUFFER_TARGET_MS;

struct DeferredDeckLoad {
    deck: DeckId,
    generation: u64,
    meta: super::types::TrackMeta,
    sample_rate: u32,
    channels: u16,
    norm_gain: f32,
    sample_buffer: Vec<f32>,
}

/// Atomics shared between the audio callback and other threads.
/// The audio callback WRITES, other threads READ.
pub struct SharedAtomics {
    pub engine_state: Arc<AtomicU8>,
    /// Active playback position in PCM frames, independent of source/output
    /// channel-count differences.
    pub position_frames: Arc<AtomicU64>,
    pub duration_ms: Arc<AtomicU64>,
    pub active_rating_key: Arc<AtomicI64>,
    pub device_sample_rate: Arc<AtomicU32>,
    /// Which physical deck is currently active (0 = A, 1 = B).
    /// Written by the audio callback on every swap, read by the control thread
    /// to determine which deck is pending for preloads.
    pub active_deck_id: Arc<AtomicU8>,
    // Seek coordination — control task writes, bg decode reads
    pub deck_a_seek_ms: Arc<AtomicI64>,
    pub deck_b_seek_ms: Arc<AtomicI64>,
    /// PCM epochs change on both deck replacement and out-of-buffer seeks so
    /// the callback can reject samples decoded before the latest seek.
    pub deck_a_generation: Arc<AtomicU64>,
    pub deck_b_generation: Arc<AtomicU64>,
    /// Stream ownership changes only when a physical deck is replaced or
    /// stopped. Seek epochs must never cancel the HTTP download they rely on.
    pub deck_a_stream_generation: Arc<AtomicU64>,
    pub deck_b_stream_generation: Arc<AtomicU64>,
    /// Track whose initial pending-deck PCM has been prepared.
    pub preload_ready_rk: Arc<AtomicI64>,
    /// Manual play/next request waiting for an in-flight preload to become ready.
    pub activate_when_ready_rk: Arc<AtomicI64>,
    /// Rating key of the last preload that failed (stream error / truncated).
    /// Written by bg decode or control thread on error, read by PLAY handler
    /// to avoid using a broken preload.
    pub preload_error_rk: Arc<AtomicI64>,
}

impl SharedAtomics {
    pub fn new() -> Self {
        Self {
            engine_state: Arc::new(AtomicU8::new(EngineState::Stopped.to_u8())),
            position_frames: Arc::new(AtomicU64::new(0)),
            duration_ms: Arc::new(AtomicU64::new(0)),
            active_rating_key: Arc::new(AtomicI64::new(0)),
            device_sample_rate: Arc::new(AtomicU32::new(44100)),
            active_deck_id: Arc::new(AtomicU8::new(0)), // A = 0
            deck_a_seek_ms: Arc::new(AtomicI64::new(-1)),
            deck_b_seek_ms: Arc::new(AtomicI64::new(-1)),
            deck_a_generation: Arc::new(AtomicU64::new(0)),
            deck_b_generation: Arc::new(AtomicU64::new(0)),
            deck_a_stream_generation: Arc::new(AtomicU64::new(0)),
            deck_b_stream_generation: Arc::new(AtomicU64::new(0)),
            preload_ready_rk: Arc::new(AtomicI64::new(0)),
            activate_when_ready_rk: Arc::new(AtomicI64::new(0)),
            preload_error_rk: Arc::new(AtomicI64::new(0)),
        }
    }

    pub fn get_state(&self) -> EngineState {
        EngineState::from_u8(self.engine_state.load(Ordering::Relaxed))
    }

    pub fn set_state(&self, state: EngineState) {
        self.engine_state.store(state.to_u8(), Ordering::Relaxed);
    }
}

impl Default for SharedAtomics {
    fn default() -> Self {
        Self::new()
    }
}

/// All state owned exclusively by the cpal audio callback closure.
/// No Arc, no Mutex — the callback is the sole owner.
pub struct AudioCallbackState {
    // ---- Owned audio state ----
    pub deck_mgr: DeckManager,
    pub dsp_chain: DspChain,
    pub scheduler: Scheduler,
    pub crossfade_settings: CrossfadeSettings,
    pub ramp_cache: HashMap<i64, TrackRamps>,
    pub is_crossfading: bool,
    /// Rating key of the track being faded out, retained until the overlap
    /// completes so its end event and deck retirement target stay exact.
    crossfade_out_rk: i64,
    crossfade_remaining_frames: u64,
    crossfade_uses_curves: bool,
    deferred_deck_load: Option<DeferredDeckLoad>,
    deferred_transition: Option<(i64, u64, bool)>,
    /// Latest seek accepted before the first deck becomes active. Loads and
    /// rapid skip-then-seek interactions can queue commands before the
    /// asynchronous decoder has produced its initial PCM.
    pending_seek_ms: Option<usize>,
    deferred_sample_a: Option<SampleBatch>,
    deferred_sample_b: Option<SampleBatch>,
    pub normalization_enabled: bool,
    paused: bool,

    // ---- Lock-free inputs ----
    cmd_rx: Receiver<AudioCommand>,
    deck_a_rx: Receiver<SampleBatch>,
    deck_b_rx: Receiver<SampleBatch>,

    // ---- Lock-free outputs ----
    event_tx: Sender<EngineEvent>,
    vis_tx: Sender<Vec<f32>>,
    vis_recycle_tx: Sender<Vec<f32>>,
    vis_recycle_rx: Receiver<Vec<f32>>,
    retired_buffer_tx: Sender<Vec<f32>>,

    // ---- Atomics (callback writes, others read) ----
    pub atomics: Arc<SharedAtomics>,

    // ---- Device info ----
    pub device_sample_rate: u32,
    pub device_channels: u16,

    // ---- Visualizer throttle ----
    vis_enabled: bool,
    vis_frame_accum: u64,

    // ---- Duck-and-apply state ----
    duck_saved_volume: Option<f32>,
    duck_remaining_frames: u32,
}

impl AudioCallbackState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cmd_rx: Receiver<AudioCommand>,
        deck_a_rx: Receiver<SampleBatch>,
        deck_b_rx: Receiver<SampleBatch>,
        event_tx: Sender<EngineEvent>,
        vis_tx: Sender<Vec<f32>>,
        vis_recycle_tx: Sender<Vec<f32>>,
        vis_recycle_rx: Receiver<Vec<f32>>,
        retired_buffer_tx: Sender<Vec<f32>>,
        atomics: Arc<SharedAtomics>,
    ) -> Self {
        Self {
            deck_mgr: DeckManager::new(),
            dsp_chain: DspChain::new(44100),
            scheduler: Scheduler::new(),
            crossfade_settings: CrossfadeSettings::default(),
            ramp_cache: HashMap::new(),
            is_crossfading: false,
            crossfade_out_rk: 0,
            crossfade_remaining_frames: 0,
            crossfade_uses_curves: false,
            deferred_deck_load: None,
            deferred_transition: None,
            pending_seek_ms: None,
            deferred_sample_a: None,
            deferred_sample_b: None,
            normalization_enabled: true,
            paused: false,

            cmd_rx,
            deck_a_rx,
            deck_b_rx,

            event_tx,
            vis_tx,
            vis_recycle_tx,
            vis_recycle_rx,
            retired_buffer_tx,

            atomics,

            device_sample_rate: 44100,
            device_channels: 2,

            vis_enabled: false,
            vis_frame_accum: 0,

            duck_saved_volume: None,
            duck_remaining_frames: 0,
        }
    }

    // -----------------------------------------------------------------------
    // Main callback — called by cpal every ~10ms
    // -----------------------------------------------------------------------

    pub fn process_callback(&mut self, data: &mut [f32]) {
        // 1. Process commands first (lock-free)
        //    LoadDeck must be processed before draining samples so the deck
        //    knows its rating_key and generation — otherwise initial batches
        //    that arrive in the same callback tick are rejected as stale.
        self.process_commands();

        // 2. Drain decoded samples into deck buffers (lock-free)
        self.drain_sample_batches();

        // 3. Check buffering → playing resume
        self.check_buffering_resume();

        // 4. Zero output buffer
        data.fill(0.0);

        // 5. Mix decks (direct access, no lock)
        let rendered_audio = !self.paused && self.state() == EngineState::Playing;
        if rendered_audio {
            self.render_audio(data);
        }
        let output_frames = data.len() / usize::from(self.device_channels.max(1));
        if self.is_crossfading && rendered_audio {
            self.crossfade_remaining_frames = self
                .crossfade_remaining_frames
                .saturating_sub(output_frames as u64);
        }

        // 6. Process DSP chain (direct access, no lock). Skipped while idle:
        //    the buffer is already zeroed and running EQ/limiter over silence
        //    keeps the limiter's log10 path hot for nothing.
        if rendered_audio {
            self.dsp_chain
                .process(data, self.device_sample_rate, self.device_channels);
        }

        // 7. Update position atomics
        self.update_position_atomics();

        // 8. Tick scheduler (sample-accurate)
        self.tick_scheduler();

        // 9. Check crossfade completion (replaces sleep+lock pattern)
        self.check_crossfade_complete();

        // 10. Handle duck-and-apply countdown
        self.tick_duck(output_frames as u32);

        // 11. Send visualizer data (~30fps) — silence carries nothing worth
        //     an FFT + webview event
        if rendered_audio {
            self.maybe_send_vis_frame(data);
        }
    }

    /// Render one device callback. A gapless transition is allowed to occur
    /// inside the output buffer, so the first frame after the outgoing PCM ends
    /// comes from the incoming deck instead of being zero-filled until the next
    /// device callback.
    fn render_audio(&mut self, data: &mut [f32]) {
        let output_channels = usize::from(self.device_channels.max(1));
        let output_frames = data.len() / output_channels;
        let split_frame = self.gapless_split_frame(output_frames);

        if let Some(split_frame) = split_frame {
            let split_sample = split_frame.saturating_mul(output_channels).min(data.len());
            if split_sample > 0 {
                let (active, pending) = self.deck_mgr.both_decks_mut();
                mixer::mix_decks(
                    active,
                    pending,
                    &mut data[..split_sample],
                    self.device_channels,
                    false,
                );
            }

            if self.handle_gapless_transition() && split_sample < data.len() {
                let (active, pending) = self.deck_mgr.both_decks_mut();
                mixer::mix_decks(
                    active,
                    pending,
                    &mut data[split_sample..],
                    self.device_channels,
                    false,
                );
            }
            return;
        }

        let (active, pending) = self.deck_mgr.both_decks_mut();
        mixer::mix_decks(
            active,
            pending,
            data,
            self.device_channels,
            self.is_crossfading,
        );
    }

    fn gapless_split_frame(&self, output_frames: usize) -> Option<usize> {
        if self.is_crossfading || self.scheduler.mode() != SchedulerMode::Gapless {
            return None;
        }
        let active = self.deck_mgr.active_deck();
        if !active.loaded || !active.fully_decoded || active.channels == 0 {
            return None;
        }
        if !self.pending_ready_for_transition(TRANSITION_READY_MS) {
            return None;
        }

        let remaining_frames = active.samples.len() / usize::from(active.channels);
        (remaining_frames <= output_frames).then_some(remaining_frames)
    }

    // -----------------------------------------------------------------------
    // Sample batch draining
    // -----------------------------------------------------------------------

    fn drain_sample_batches(&mut self) {
        let deferred = self.deferred_deck_load.as_ref().map(|load| load.deck);
        if deferred != Some(DeckId::A) {
            self.drain_deck_channel(DeckId::A);
        }
        if deferred != Some(DeckId::B) {
            self.drain_deck_channel(DeckId::B);
        }
    }

    fn drain_deck_channel(&mut self, deck_id: DeckId) {
        let mut accepted_batches = 0;
        let mut scanned_batches = 0;
        while accepted_batches < MAX_SAMPLE_BATCHES_PER_DECK_CALLBACK
            && scanned_batches < MAX_SCANNED_SAMPLE_BATCHES_PER_DECK_CALLBACK
        {
            let deferred = match deck_id {
                DeckId::A => self.deferred_sample_a.take(),
                DeckId::B => self.deferred_sample_b.take(),
            };
            let batch = deferred.or_else(|| match deck_id {
                DeckId::A => self.deck_a_rx.try_recv().ok(),
                DeckId::B => self.deck_b_rx.try_recv().ok(),
            });
            let Some(batch) = batch else {
                break;
            };
            scanned_batches += 1;
            let deck = self.deck_mgr.deck_mut(deck_id);

            // Reject stale batches (wrong track or old seek generation)
            if deck.rating_key() != batch.rating_key || deck.generation != batch.generation {
                let _ = self.retired_buffer_tx.try_send(batch.samples);
                continue;
            }

            if batch.samples.len() > deck.samples.free() {
                match deck_id {
                    DeckId::A => self.deferred_sample_a = Some(batch),
                    DeckId::B => self.deferred_sample_b = Some(batch),
                }
                break;
            }

            if deck.sample_offset > 0 && deck.samples.is_empty() {
                log::info!(
                    "native audio accepted first post-seek PCM batch track={} deck={:?} generation={} samples={}",
                    batch.rating_key,
                    deck_id,
                    batch.generation,
                    batch.samples.len(),
                );
            }
            let pushed = deck.samples.push(&batch.samples);
            debug_assert!(pushed);
            if batch.fully_decoded {
                deck.fully_decoded = true;
            }
            let _ = self.retired_buffer_tx.try_send(batch.samples);
            accepted_batches += 1;
        }
    }

    // -----------------------------------------------------------------------
    // Command processing
    // -----------------------------------------------------------------------

    fn process_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            self.handle_command(cmd);
        }
    }

    fn handle_command(&mut self, cmd: AudioCommand) {
        match cmd {
            AudioCommand::LoadDeck {
                deck,
                generation,
                meta,
                sample_rate,
                channels,
                norm_gain,
                sample_buffer,
            } => {
                if self.is_crossfading && deck == self.deck_mgr.active_id().other() {
                    let replacement = DeferredDeckLoad {
                        deck,
                        generation,
                        meta,
                        sample_rate,
                        channels,
                        norm_gain,
                        sample_buffer,
                    };
                    if let Some(retired) = self.deferred_deck_load.replace(replacement) {
                        let _ = self.retired_buffer_tx.try_send(retired.sample_buffer);
                    }
                } else {
                    self.handle_load_deck(
                        deck,
                        generation,
                        meta,
                        sample_rate,
                        channels,
                        norm_gain,
                        sample_buffer,
                    );
                    self.compute_and_set_schedule();
                }
            }
            AudioCommand::Pause => {
                self.paused = true;
                self.set_state(EngineState::Paused);
                let _ = self.event_tx.try_send(EngineEvent::State {
                    state: "paused".into(),
                });
            }
            AudioCommand::Resume => {
                self.paused = false;
                self.set_state(EngineState::Playing);
                let _ = self.event_tx.try_send(EngineEvent::State {
                    state: "playing".into(),
                });
            }
            AudioCommand::Stop => {
                self.scheduler.reset();
                self.deck_mgr.stop_all();
                self.discard_queued_samples();
                self.is_crossfading = false;
                self.crossfade_remaining_frames = 0;
                self.crossfade_uses_curves = false;
                if let Some(retired) = self.deferred_deck_load.take() {
                    let _ = self.retired_buffer_tx.try_send(retired.sample_buffer);
                }
                self.deferred_transition = None;
                self.pending_seek_ms = None;
                self.paused = false;
                self.set_state(EngineState::Stopped);
                let _ = self.event_tx.try_send(EngineEvent::State {
                    state: "stopped".into(),
                });
            }
            AudioCommand::TransitionToActive {
                user_skip,
                rating_key,
                generation,
            } => {
                let pending_id = self.deck_mgr.active_id().other();
                let current_generation = match pending_id {
                    DeckId::A => self.atomics.deck_a_generation.load(Ordering::Acquire),
                    DeckId::B => self.atomics.deck_b_generation.load(Ordering::Acquire),
                };
                let pending = self.deck_mgr.pending_deck();
                if current_generation == generation
                    && pending.rating_key() == rating_key
                    && pending.generation == generation
                {
                    self.handle_transition_to_active(user_skip);
                } else if self.deferred_deck_load.as_ref().is_some_and(|load| {
                    load.meta.rating_key == rating_key && load.generation == generation
                }) {
                    self.deferred_transition = Some((rating_key, generation, user_skip));
                } else {
                    debug!(rating_key, generation, "discarding stale deck transition");
                }
            }
            AudioCommand::SeekInBuffer { position } => {
                if self.deck_mgr.active_deck().loaded {
                    self.seek_active_deck(position);
                } else {
                    // Play/load is asynchronous. Keep only the latest target;
                    // it is applied after TransitionToActive installs the deck.
                    self.pending_seek_ms = Some(position);
                }
                self.dsp_chain.reset();
            }
            AudioCommand::SetVolume(gain) => {
                self.dsp_chain.set_volume(gain);
            }
            AudioCommand::SetPreampGain(db) => {
                self.dsp_chain.set_preamp_db(db);
            }
            AudioCommand::SetEq(gains) => {
                self.dsp_chain.set_eq_gains(&gains);
            }
            AudioCommand::SetEqEnabled(enabled) => {
                self.dsp_chain.set_eq_enabled(enabled);
            }
            AudioCommand::SetEqPostgain(db) => {
                self.dsp_chain.set_postgain_db(db);
            }
            AudioCommand::SetLimiterEnabled(enabled) => {
                self.dsp_chain.set_limiter_enabled(enabled);
            }
            AudioCommand::SetCrossfeed { enabled, preset } => {
                self.dsp_chain.set_crossfeed(enabled, preset);
            }
            AudioCommand::SetCrossfeedBeforeEq(before) => {
                self.dsp_chain.set_crossfeed_before_eq(before);
            }
            AudioCommand::ResetDsp => {
                self.dsp_chain.reset();
            }
            AudioCommand::SetNormalization(enabled) => {
                self.normalization_enabled = enabled;
                // Apply the change to the audible and already-preloaded decks.
                for deck in [&mut self.deck_mgr.deck_a, &mut self.deck_mgr.deck_b] {
                    if let Some(ref meta) = deck.meta {
                        deck.norm_gain = if enabled {
                            meta.gain_db.map_or(1.0, |db| 10.0_f32.powf(db / 20.0))
                        } else {
                            1.0
                        };
                    }
                }
                if let Some(load) = self.deferred_deck_load.as_mut() {
                    load.norm_gain = if enabled {
                        load.meta.gain_db.map_or(1.0, |db| 10.0_f32.powf(db / 20.0))
                    } else {
                        1.0
                    };
                }
            }
            AudioCommand::UpdateTrackAnalysis {
                rating_key,
                gain_db,
                intro_end_ms,
                outro_start_ms,
                fade_start_ms,
                silence_start_ms,
            } => {
                for deck in [&mut self.deck_mgr.deck_a, &mut self.deck_mgr.deck_b] {
                    if deck.rating_key() != rating_key {
                        continue;
                    }
                    if let Some(meta) = deck.meta.as_mut() {
                        meta.gain_db = gain_db;
                        meta.intro_end_ms = intro_end_ms;
                        meta.outro_start_ms = outro_start_ms;
                        meta.fade_start_ms = fade_start_ms;
                        meta.silence_start_ms = silence_start_ms;
                    }
                    deck.norm_gain = if self.normalization_enabled {
                        gain_db.map_or(1.0, |db| 10.0_f32.powf(db / 20.0))
                    } else {
                        1.0
                    };
                }
                if let Some(load) = self
                    .deferred_deck_load
                    .as_mut()
                    .filter(|load| load.meta.rating_key == rating_key)
                {
                    load.meta.gain_db = gain_db;
                    load.meta.intro_end_ms = intro_end_ms;
                    load.meta.outro_start_ms = outro_start_ms;
                    load.meta.fade_start_ms = fade_start_ms;
                    load.meta.silence_start_ms = silence_start_ms;
                    load.norm_gain = if self.normalization_enabled {
                        gain_db.map_or(1.0, |db| 10.0_f32.powf(db / 20.0))
                    } else {
                        1.0
                    };
                }
                self.compute_and_set_schedule();
            }
            AudioCommand::UpdateCrossfadeSettings(settings) => {
                self.crossfade_settings = settings;
                // Recompute schedule if both decks loaded
                self.compute_and_set_schedule();
            }
            AudioCommand::CacheRamps { rating_key, ramps } => {
                self.ramp_cache.insert(rating_key, ramps);
                // Keep cache bounded
                if self.ramp_cache.len() > 100 {
                    if let Some(&key) = self.ramp_cache.keys().next() {
                        self.ramp_cache.remove(&key);
                    }
                }
                self.compute_and_set_schedule();
            }
            AudioCommand::SetVisualizerEnabled(enabled) => {
                self.vis_enabled = enabled;
            }
            AudioCommand::DuckAndApply { duck_ms } => {
                // Save current volume, set to 0, schedule restore
                let current = self.dsp_chain.volume.gain();
                self.duck_saved_volume = Some(current);
                self.dsp_chain.set_volume(0.0);
                let frames = (duck_ms as f32 / 1000.0 * self.device_sample_rate as f32) as u32;
                self.duck_remaining_frames = frames;
            }
        }
    }

    fn discard_queued_samples(&mut self) {
        for batch in [self.deferred_sample_a.take(), self.deferred_sample_b.take()]
            .into_iter()
            .flatten()
        {
            let _ = self.retired_buffer_tx.try_send(batch.samples);
        }
        while let Ok(batch) = self.deck_a_rx.try_recv() {
            let _ = self.retired_buffer_tx.try_send(batch.samples);
        }
        while let Ok(batch) = self.deck_b_rx.try_recv() {
            let _ = self.retired_buffer_tx.try_send(batch.samples);
        }
    }

    // -----------------------------------------------------------------------
    // Deck loading
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn handle_load_deck(
        &mut self,
        deck: DeckId,
        generation: u64,
        meta: super::types::TrackMeta,
        sample_rate: u32,
        channels: u16,
        norm_gain: f32,
        sample_buffer: Vec<f32>,
    ) {
        let current_generation = match deck {
            DeckId::A => self.atomics.deck_a_generation.load(Ordering::Acquire),
            DeckId::B => self.atomics.deck_b_generation.load(Ordering::Acquire),
        };
        if current_generation != generation {
            let _ = self.retired_buffer_tx.try_send(sample_buffer);
            return;
        }
        let d = self.deck_mgr.deck_mut(deck);
        d.reset();
        let retired = if d.samples.capacity() >= sample_buffer.capacity() {
            sample_buffer
        } else {
            d.samples.replace_storage(sample_buffer)
        };
        // Deallocating a previous track-sized PCM allocation can take longer
        // than an audio deadline. Hand it to a reclaimer thread instead.
        let _ = self.retired_buffer_tx.try_send(retired);
        d.sample_rate = sample_rate;
        d.channels = channels;
        d.meta = Some(meta);
        d.loaded = true;
        d.norm_gain = norm_gain;
        // Inherit the current generation from atomics for new batches
        d.generation = generation;
    }

    fn install_deferred_deck_load(&mut self) {
        let Some(load) = self.deferred_deck_load.take() else {
            return;
        };
        self.handle_load_deck(
            load.deck,
            load.generation,
            load.meta,
            load.sample_rate,
            load.channels,
            load.norm_gain,
            load.sample_buffer,
        );
        if let Some((rating_key, generation, user_skip)) = self.deferred_transition.take() {
            let pending_id = self.deck_mgr.active_id().other();
            let current_generation = match pending_id {
                DeckId::A => self.atomics.deck_a_generation.load(Ordering::Acquire),
                DeckId::B => self.atomics.deck_b_generation.load(Ordering::Acquire),
            };
            let pending = self.deck_mgr.pending_deck();
            if current_generation == generation
                && pending.rating_key() == rating_key
                && pending.generation == generation
            {
                self.handle_transition_to_active(user_skip);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Transition (swap pending → active)
    // -----------------------------------------------------------------------

    fn handle_transition_to_active(&mut self, user_skip: bool) {
        self.scheduler.reset();

        let has_active =
            self.deck_mgr.active_deck().loaded && self.deck_mgr.active_deck().has_started_playing;

        let should_xfade = has_active
            && self.deck_mgr.pending_deck().loaded
            && !self
                .deck_mgr
                .pending_deck()
                .meta
                .as_ref()
                .is_none_or(|m| m.skip_crossfade)
            && self.effective_window() > 0
            && !super::crossfade::album_aware::should_suppress_crossfade(
                self.deck_mgr.active_deck().parent_key(),
                self.deck_mgr.pending_deck().parent_key(),
                self.crossfade_settings.same_album_crossfade,
            );

        if should_xfade && user_skip {
            // Short duck crossfade for user skip
            let plan = compute_skip_duck(500);
            self.deck_mgr.pending_deck_mut().has_started_playing = true;
            let new_rk = self.deck_mgr.pending_deck().rating_key();
            let new_dur = self
                .deck_mgr
                .pending_deck()
                .meta
                .as_ref()
                .map_or(0, |m| m.duration_ms);

            // Remember which track is fading out so crossfade cleanup
            // doesn't accidentally reset a newly preloaded deck.
            self.crossfade_out_rk = self.deck_mgr.active_deck().rating_key();
            self.swap_decks();
            self.is_crossfading = true;
            self.begin_crossfade(&plan);

            let (active, old) = self.deck_mgr.both_decks_mut();
            apply_fade_start(active, old, &plan);

            let transition_state = if self.paused {
                EngineState::Paused
            } else {
                EngineState::Playing
            };
            self.set_state(transition_state);
            let _ = self.event_tx.try_send(EngineEvent::TrackStarted {
                rating_key: new_rk,
                duration_ms: new_dur,
            });
            let _ = self.event_tx.try_send(EngineEvent::State {
                state: transition_state.as_str().into(),
            });
        } else {
            // Hard transition
            self.deck_mgr.active_deck_mut().reset();
            self.swap_decks();

            let active = self.deck_mgr.active_deck_mut();
            active.has_started_playing = true;
            active.fade_gain = 1.0;
            let new_rk = active.rating_key();
            let new_dur = active.meta.as_ref().map_or(0, |m| m.duration_ms);

            self.is_crossfading = false;
            self.crossfade_remaining_frames = 0;
            self.crossfade_uses_curves = false;
            let transition_state = if self.paused {
                EngineState::Paused
            } else {
                EngineState::Playing
            };
            self.set_state(transition_state);
            let _ = self.event_tx.try_send(EngineEvent::TrackStarted {
                rating_key: new_rk,
                duration_ms: new_dur,
            });
            let _ = self.event_tx.try_send(EngineEvent::State {
                state: transition_state.as_str().into(),
            });
        }

        self.compute_and_set_schedule();
        if let Some(position_ms) = self.pending_seek_ms.take() {
            self.seek_active_deck(position_ms);
        }
    }

    fn seek_active_deck(&mut self, position_ms: usize) {
        let active = self.deck_mgr.active_deck_mut();
        if !active.loaded || active.channels == 0 || active.sample_rate == 0 {
            self.pending_seek_ms = Some(position_ms);
            return;
        }

        let target_sample = (position_ms as f32 / 1000.0
            * active.sample_rate as f32
            * active.channels as f32) as usize;
        let buffer_end = active.sample_offset + active.samples.len();
        let in_buffer = target_sample >= active.sample_offset && target_sample <= buffer_end;

        if in_buffer {
            let consumed = active.samples.consume(target_sample - active.sample_offset);
            active.sample_offset = active.sample_offset.saturating_add(consumed);
        } else {
            // Outside the bounded decoded window — ask the persistent decoder
            // worker to seek and refill it. PCM generation advances to reject
            // already-queued batches; stream ownership deliberately does not.
            let out_ch = active.channels as usize;
            let new_offset = (position_ms as f64 / 1000.0
                * self.device_sample_rate as f64
                * out_ch as f64) as usize;
            let new_gen = active.generation + 1;
            active.samples.clear();
            active.sample_offset = new_offset;
            active.fully_decoded = false;
            active.generation = new_gen;

            let active_id = self.deck_mgr.active_id();
            match active_id {
                DeckId::A => {
                    self.atomics
                        .deck_a_seek_ms
                        .store(position_ms as i64, Ordering::Relaxed);
                    self.atomics
                        .deck_a_generation
                        .store(new_gen, Ordering::Release);
                }
                DeckId::B => {
                    self.atomics
                        .deck_b_seek_ms
                        .store(position_ms as i64, Ordering::Relaxed);
                    self.atomics
                        .deck_b_generation
                        .store(new_gen, Ordering::Release);
                }
            }

            self.set_state(EngineState::Buffering);
            let _ = self.event_tx.try_send(EngineEvent::State {
                state: "buffering".into(),
            });
        }

        self.scheduler.reset();
        self.compute_and_set_schedule();
    }

    // -----------------------------------------------------------------------
    // Scheduler
    // -----------------------------------------------------------------------

    fn tick_scheduler(&mut self) {
        if self.state() != EngineState::Playing {
            return;
        }

        let active = self.deck_mgr.active_deck();
        if !active.loaded {
            return;
        }

        let pos = active.position_secs();
        let dur = active.duration_secs();

        if let Some(action) = self.scheduler.check(pos, dur) {
            let transitioned = match action {
                SchedulerAction::TransitionPoint => self.handle_crossfade_transition(),
                // Metadata duration is only a hint. Exact gapless handoff is
                // performed from decoded PCM EOF inside `render_audio`.
                SchedulerAction::GaplessPoint => {
                    self.deck_mgr.active_deck().is_finished() && self.handle_gapless_transition()
                }
            };
            if !transitioned {
                self.scheduler.retry();
            }
        } else if active.is_finished() && !self.is_crossfading {
            // Metadata duration can differ slightly from the decoded PCM
            // length. If the exact scheduled point was missed, preserve the
            // prepared next track instead of stopping at the end of this one.
            if self.deck_mgr.pending_deck().loaded {
                if !self.handle_gapless_transition() {
                    self.scheduler.retry();
                }
            } else {
                let rk = active.rating_key();
                let _ = self
                    .event_tx
                    .try_send(EngineEvent::TrackEnded { rating_key: rk });
                self.set_state(EngineState::Stopped);
                let _ = self.event_tx.try_send(EngineEvent::State {
                    state: "stopped".into(),
                });
            }
        } else if active.loaded
            && !active.fully_decoded
            && active.samples.is_empty()
            && !self.is_crossfading
        {
            // Active deck ran out of samples but the track isn't fully decoded.
            // This happens when a streaming download was truncated (HTTP error).
            // Enter buffering state — if more data arrives it will resume,
            // otherwise the JS side can detect the stall.
            self.set_state(EngineState::Buffering);
            let _ = self.event_tx.try_send(EngineEvent::BufferUnderrun);
        }
    }

    fn handle_crossfade_transition(&mut self) -> bool {
        debug!("crossfade transition triggered by scheduler");

        if !self.deck_mgr.pending_deck().loaded {
            debug!("crossfade aborted: pending deck not loaded");
            return false;
        }

        let plan = {
            let params = self.build_crossfade_params();
            compute_transition(&params)
        };

        if let Some(plan) = plan {
            let required_ms = TRANSITION_READY_MS.max((plan.duration_sec * 1000.0).ceil() as u64);
            if !self.pending_ready_for_transition(required_ms) {
                debug!(
                    buffered_ms = self.deck_mgr.pending_deck().buffered_ahead_ms(),
                    required_ms, "crossfade deferred: pending deck is still buffering"
                );
                return false;
            }
            debug!(
                duration_ms = (plan.duration_sec * 1000.0) as u32,
                "crossfade transition"
            );

            self.deck_mgr.pending_deck_mut().has_started_playing = true;
            let new_rk = self.deck_mgr.pending_deck().rating_key();
            let new_dur = self
                .deck_mgr
                .pending_deck()
                .meta
                .as_ref()
                .map_or(0, |m| m.duration_ms);

            self.crossfade_out_rk = self.deck_mgr.active_deck().rating_key();
            self.swap_decks();
            self.is_crossfading = true;
            self.begin_crossfade(&plan);

            let (active, old) = self.deck_mgr.both_decks_mut();
            apply_fade_start(active, old, &plan);

            let _ = self.event_tx.try_send(EngineEvent::TrackStarted {
                rating_key: new_rk,
                duration_ms: new_dur,
            });

            self.compute_and_set_schedule();
            true
        } else {
            false
        }
    }

    fn handle_gapless_transition(&mut self) -> bool {
        if !self.pending_ready_for_transition(TRANSITION_READY_MS) {
            debug!(
                buffered_ms = self.deck_mgr.pending_deck().buffered_ahead_ms(),
                "gapless transition deferred: pending deck is still buffering"
            );
            return false;
        }

        let old_rk = self.deck_mgr.active_deck().rating_key();
        self.deck_mgr.pending_deck_mut().has_started_playing = true;
        let new_rk = self.deck_mgr.pending_deck().rating_key();
        let new_dur = self
            .deck_mgr
            .pending_deck()
            .meta
            .as_ref()
            .map_or(0, |m| m.duration_ms);

        self.swap_decks();
        self.deck_mgr.pending_deck_mut().reset();

        let _ = self.event_tx.try_send(EngineEvent::TrackStarted {
            rating_key: new_rk,
            duration_ms: new_dur,
        });
        let _ = self
            .event_tx
            .try_send(EngineEvent::TrackEnded { rating_key: old_rk });

        self.compute_and_set_schedule();
        true
    }

    fn pending_ready_for_transition(&self, required_ms: u64) -> bool {
        let pending = self.deck_mgr.pending_deck();
        pending.loaded
            && !pending.samples.is_empty()
            && (pending.fully_decoded || pending.buffered_ahead_ms() >= required_ms)
    }

    // -----------------------------------------------------------------------
    // Crossfade completion (frame-accurate, replaces sleep+lock)
    // -----------------------------------------------------------------------

    fn check_crossfade_complete(&mut self) {
        if !self.is_crossfading {
            return;
        }

        // Timed/boundary fades complete when their curves do. MixRamp has no
        // curves, so retain the outgoing deck for the plan's explicit overlap.
        let active_done = self
            .deck_mgr
            .active_deck()
            .fade_curve
            .as_ref()
            .is_none_or(|c| c.is_finished());
        let pending_done = self
            .deck_mgr
            .pending_deck()
            .fade_curve
            .as_ref()
            .is_none_or(|c| c.is_finished());

        let complete = if self.crossfade_uses_curves {
            active_done && pending_done
        } else {
            self.crossfade_remaining_frames == 0
        };

        if complete {
            self.is_crossfading = false;

            let pending_rk = self.deck_mgr.pending_deck().rating_key();

            // The outgoing deck normally remains pending until this point.
            // Keep the identity check defensive in case a future transition
            // path changes deck roles before cleanup.
            if pending_rk == self.crossfade_out_rk || pending_rk == 0 {
                if self.crossfade_out_rk != 0 {
                    let _ = self.event_tx.try_send(EngineEvent::TrackEnded {
                        rating_key: self.crossfade_out_rk,
                    });
                }
                self.deck_mgr.pending_deck_mut().reset();
            }

            self.crossfade_out_rk = 0;
            self.crossfade_remaining_frames = 0;
            self.crossfade_uses_curves = false;
            self.deck_mgr.active_deck_mut().fade_gain = 1.0;
            self.deck_mgr.active_deck_mut().fade_curve = None;
            self.install_deferred_deck_load();
            self.compute_and_set_schedule();
        }
    }

    // -----------------------------------------------------------------------
    // Buffering resume
    // -----------------------------------------------------------------------

    fn check_buffering_resume(&mut self) {
        if self.state() != EngineState::Buffering {
            return;
        }
        let active = self.deck_mgr.active_deck();
        let enough_margin =
            active.fully_decoded || active.buffered_ahead_ms() >= REBUFFER_TARGET_MS;
        if active.loaded && !active.samples.is_empty() && enough_margin {
            self.set_state(EngineState::Playing);
            let _ = self.event_tx.try_send(EngineEvent::State {
                state: "playing".into(),
            });
        }
    }

    // -----------------------------------------------------------------------
    // Position atomics
    // -----------------------------------------------------------------------

    fn update_position_atomics(&self) {
        let active = self.deck_mgr.active_deck();
        if active.loaded {
            let pos = active.sample_offset as u64 / u64::from(active.channels.max(1));
            self.atomics.position_frames.store(pos, Ordering::Relaxed);
            let dur = active.meta.as_ref().map_or(0, |m| m.duration_ms);
            self.atomics.duration_ms.store(dur, Ordering::Relaxed);
            self.atomics
                .active_rating_key
                .store(active.rating_key(), Ordering::Relaxed);
        }
    }

    // -----------------------------------------------------------------------
    // Duck-and-apply
    // -----------------------------------------------------------------------

    fn tick_duck(&mut self, processed_frames: u32) {
        if self.duck_remaining_frames > 0 {
            self.duck_remaining_frames =
                self.duck_remaining_frames.saturating_sub(processed_frames);
            if self.duck_remaining_frames == 0 {
                if let Some(vol) = self.duck_saved_volume.take() {
                    self.dsp_chain.set_volume(vol);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Visualizer
    // -----------------------------------------------------------------------

    fn maybe_send_vis_frame(&mut self, data: &[f32]) {
        if !self.vis_enabled {
            return;
        }
        self.vis_frame_accum += data.len() as u64;
        // 30 Hz is fluid enough for the UI and halves FFT work plus the
        // serialized Rust -> WebView transport versus the old 60 Hz stream.
        let frames_per_vis = (self.device_sample_rate as u64 * self.device_channels as u64) / 30;
        if frames_per_vis == 0 || self.vis_frame_accum < frames_per_vis {
            return;
        }
        self.vis_frame_accum = 0;
        let Ok(mut frame) = self.vis_recycle_rx.try_recv() else {
            return;
        };
        frame.clear();
        let start = data.len().saturating_sub(frame.capacity());
        frame.extend_from_slice(&data[start..]);
        if let Err(error) = self.vis_tx.try_send(frame) {
            let mut frame = error.into_inner();
            frame.clear();
            let _ = self.vis_recycle_tx.try_send(frame);
        }
    }

    // -----------------------------------------------------------------------
    // Deck swap helper
    // -----------------------------------------------------------------------

    /// Swap active/pending deck roles and update ALL shared atomics immediately
    /// so the control thread always has a consistent view of the active deck.
    fn swap_decks(&mut self) {
        let old_rk = self.deck_mgr.active_deck().rating_key();
        self.deck_mgr.swap_roles();
        let active = self.deck_mgr.active_deck();
        let new_rk = active.rating_key();
        let id = match self.deck_mgr.active_id() {
            DeckId::A => 0u8,
            DeckId::B => 1u8,
        };
        debug!(old_rk, new_rk, new_active_deck = id, "SWAP decks");
        self.atomics.active_deck_id.store(id, Ordering::Relaxed);
        self.atomics
            .active_rating_key
            .store(new_rk, Ordering::Relaxed);
        if active.loaded {
            let pos = active.sample_offset as u64 / u64::from(active.channels.max(1));
            self.atomics.position_frames.store(pos, Ordering::Relaxed);
            let dur = active.meta.as_ref().map_or(0, |m| m.duration_ms);
            self.atomics.duration_ms.store(dur, Ordering::Relaxed);
        }
    }

    // -----------------------------------------------------------------------
    // Crossfade helpers
    // -----------------------------------------------------------------------

    fn effective_window(&self) -> u32 {
        if self.crossfade_settings.smart_crossfade {
            self.crossfade_settings.smart_crossfade_max_ms
        } else {
            self.crossfade_settings.crossfade_window_ms
        }
    }

    fn begin_crossfade(&mut self, plan: &TransitionPlan) {
        self.crossfade_remaining_frames =
            (plan.duration_sec * self.device_sample_rate as f32).ceil() as u64;
        self.crossfade_uses_curves = plan.fade_in_curve.is_some() || plan.fade_out_curve.is_some();
    }

    fn compute_and_set_schedule(&mut self) {
        self.scheduler.reset();
        // During an overlap the physical pending deck is the outgoing track,
        // not the next queue item. Scheduling against it would create a
        // backwards transition. Completion installs N+2 and re-arms then.
        if self.is_crossfading {
            return;
        }

        let active = self.deck_mgr.active_deck();
        let pending = self.deck_mgr.pending_deck();

        if !active.loaded || !pending.loaded {
            return;
        }

        let window = self.effective_window();
        let suppress = super::crossfade::album_aware::should_suppress_crossfade(
            active.parent_key(),
            pending.parent_key(),
            self.crossfade_settings.same_album_crossfade,
        );

        if window == 0 || suppress || pending.meta.as_ref().is_some_and(|m| m.skip_crossfade) {
            self.scheduler.set_mode(SchedulerMode::Gapless);
            self.scheduler.reset();
        } else {
            let params = self.build_crossfade_params();
            if let Some(plan) = compute_transition(&params) {
                self.scheduler.set_mode(SchedulerMode::Crossfade);
                self.scheduler.set_transition_point(plan.start_time_sec);
                debug!(
                    trigger_sec = format!("{:.2}", plan.start_time_sec),
                    duration_sec = format!("{:.2}", plan.duration_sec),
                    track_duration = format!("{:.2}", active.duration_secs()),
                    "scheduled crossfade"
                );
            } else {
                self.scheduler.set_mode(SchedulerMode::Gapless);
                self.scheduler.reset();
            }
        }
    }

    fn build_crossfade_params(&self) -> CrossfadeParams {
        let active = self.deck_mgr.active_deck();
        let pending = self.deck_mgr.pending_deck();
        let out_ramps = self.ramp_cache.get(&active.rating_key());
        let in_ramps = self.ramp_cache.get(&pending.rating_key());

        CrossfadeParams {
            out_duration_sec: active.duration_secs(),
            out_parent_key: active.parent_key().to_string(),
            in_parent_key: pending.parent_key().to_string(),
            out_end_ramp: out_ramps.map(|r| r.end_ramp.clone()),
            in_start_ramp: in_ramps.map(|r| r.start_ramp.clone()),
            out_outro_start_ms: active.meta.as_ref().and_then(|meta| meta.outro_start_ms),
            out_fade_start_ms: active.meta.as_ref().and_then(|meta| meta.fade_start_ms),
            out_silence_start_ms: active.meta.as_ref().and_then(|meta| meta.silence_start_ms),
            crossfade_window_ms: self.crossfade_settings.crossfade_window_ms,
            smart_crossfade_max_ms: self.crossfade_settings.smart_crossfade_max_ms,
            mixramp_db: self.crossfade_settings.mixramp_db,
            smart_crossfade_enabled: self.crossfade_settings.smart_crossfade,
            same_album_crossfade: self.crossfade_settings.same_album_crossfade,
        }
    }

    // -----------------------------------------------------------------------
    // State helpers
    // -----------------------------------------------------------------------

    fn state(&self) -> EngineState {
        self.atomics.get_state()
    }

    fn set_state(&self, state: EngineState) {
        self.atomics.set_state(state);
    }
}

/// Install fade curves on both decks for a transition plan.
fn apply_fade_start(new_active: &mut DeckState, old_active: &mut DeckState, plan: &TransitionPlan) {
    let total_frames = (plan.duration_sec * new_active.sample_rate as f32) as usize;

    if let (Some(fade_in), Some(fade_out)) = (&plan.fade_in_curve, &plan.fade_out_curve) {
        new_active.fade_gain = 0.0;
        new_active.fade_curve = Some(FadeCurve::new(fade_in.clone(), total_frames));

        old_active.fade_gain = 1.0;
        old_active.fade_curve = Some(FadeCurve::new(fade_out.clone(), total_frames));

        debug!(
            steps = fade_in.len(),
            total_frames,
            duration_sec = plan.duration_sec,
            "crossfade curves installed"
        );
    } else {
        // MixRamp: both at full volume during overlap
        new_active.fade_gain = 1.0;
        new_active.fade_curve = None;
        old_active.fade_gain = 1.0;
        old_active.fade_curve = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{bounded, unbounded};

    fn callback() -> (AudioCallbackState, Receiver<EngineEvent>) {
        let (callback, event_rx, _, _) = callback_with_deck_senders();
        (callback, event_rx)
    }

    fn callback_with_deck_senders() -> (
        AudioCallbackState,
        Receiver<EngineEvent>,
        Sender<SampleBatch>,
        Sender<SampleBatch>,
    ) {
        let (_cmd_tx, cmd_rx) = unbounded();
        let (deck_a_tx, deck_a_rx) = unbounded();
        let (deck_b_tx, deck_b_rx) = unbounded();
        let (event_tx, event_rx) = unbounded();
        let (vis_tx, _vis_rx) = bounded(1);
        let (vis_recycle_tx, vis_recycle_rx) = bounded(1);
        let (retired_buffer_tx, _retired_buffer_rx) = unbounded();
        let callback = AudioCallbackState::new(
            cmd_rx,
            deck_a_rx,
            deck_b_rx,
            event_tx,
            vis_tx,
            vis_recycle_tx,
            vis_recycle_rx,
            retired_buffer_tx,
            Arc::new(SharedAtomics::new()),
        );
        (callback, event_rx, deck_a_tx, deck_b_tx)
    }

    fn meta(
        rating_key: i64,
        duration_ms: u64,
        parent_key: &str,
        gain_db: f32,
    ) -> super::super::types::TrackMeta {
        super::super::types::TrackMeta {
            rating_key,
            duration_ms,
            parent_key: parent_key.into(),
            gain_db: Some(gain_db),
            skip_crossfade: false,
            start_ramp: None,
            end_ramp: None,
            intro_end_ms: None,
            outro_start_ms: None,
            fade_start_ms: None,
            silence_start_ms: None,
        }
    }

    fn load(
        callback: &mut AudioCallbackState,
        deck: DeckId,
        track: super::super::types::TrackMeta,
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
    ) {
        let sample_buffer = Vec::with_capacity(samples.len().max(1_024));
        callback.handle_command(AudioCommand::LoadDeck {
            deck,
            generation: 0,
            meta: track,
            sample_rate,
            channels,
            norm_gain: 1.0,
            sample_buffer,
        });
        assert!(callback.deck_mgr.deck_mut(deck).samples.push(&samples));
    }

    #[test]
    fn loading_pending_deck_arms_crossfade_schedule() {
        let (mut callback, _) = callback();
        callback.crossfade_settings = CrossfadeSettings {
            crossfade_window_ms: 4_000,
            smart_crossfade: false,
            ..CrossfadeSettings::default()
        };
        load(
            &mut callback,
            DeckId::B,
            meta(1, 10_000, "album:1", 0.0),
            10,
            2,
            vec![0.0; 200],
        );
        callback.handle_transition_to_active(true);
        load(
            &mut callback,
            DeckId::A,
            meta(2, 10_000, "album:2", 0.0),
            10,
            2,
            vec![0.0; 200],
        );

        assert!(callback.scheduler.check(5.99, 10.0).is_none());
        assert_eq!(
            callback.scheduler.check(6.0, 10.0),
            Some(SchedulerAction::TransitionPoint)
        );
    }

    #[test]
    fn stale_loader_cannot_swap_a_superseded_pending_deck() {
        let (mut callback, _) = callback();
        load(
            &mut callback,
            DeckId::B,
            meta(2, 1_000, "album:2", 0.0),
            10,
            2,
            vec![0.5; 20],
        );
        callback
            .atomics
            .deck_b_generation
            .store(1, Ordering::Release);

        callback.handle_command(AudioCommand::TransitionToActive {
            user_skip: true,
            rating_key: 2,
            generation: 0,
        });

        assert_eq!(callback.deck_mgr.active_id(), DeckId::A);
        assert_eq!(callback.deck_mgr.active_deck().rating_key(), 0);
    }

    #[test]
    fn rapid_out_of_buffer_seeks_keep_stream_owned_and_publish_latest_position() {
        let (mut callback, _) = callback();
        callback.device_sample_rate = 10;
        callback.device_channels = 2;
        load(
            &mut callback,
            DeckId::B,
            meta(2, 100_000, "album:2", 0.0),
            10,
            2,
            vec![0.5; 20],
        );
        callback.handle_transition_to_active(true);

        callback.handle_command(AudioCommand::SeekInBuffer { position: 50_000 });
        callback.handle_command(AudioCommand::SeekInBuffer { position: 25_000 });

        assert_eq!(callback.deck_mgr.active_deck().generation, 2);
        assert_eq!(
            callback.atomics.deck_b_generation.load(Ordering::Acquire),
            2
        );
        assert_eq!(
            callback
                .atomics
                .deck_b_stream_generation
                .load(Ordering::Acquire),
            0,
            "seeking must not relinquish the current download"
        );
        assert_eq!(
            callback.atomics.deck_b_seek_ms.load(Ordering::Acquire),
            25_000,
            "the decoder should coalesce rapid seeks onto the newest target"
        );
    }

    #[test]
    fn seek_before_first_pcm_is_applied_when_the_deck_becomes_active() {
        let (mut callback, _) = callback();
        callback.device_sample_rate = 10;
        callback.device_channels = 2;

        callback.handle_command(AudioCommand::SeekInBuffer { position: 50_000 });
        assert_eq!(callback.pending_seek_ms, Some(50_000));

        load(
            &mut callback,
            DeckId::B,
            meta(2, 100_000, "album:2", 0.0),
            10,
            2,
            vec![0.5; 20],
        );
        callback.handle_transition_to_active(true);

        assert_eq!(callback.pending_seek_ms, None);
        assert_eq!(callback.deck_mgr.active_deck().generation, 1);
        assert!(callback.deck_mgr.active_deck().samples.is_empty());
        assert_eq!(
            callback.atomics.deck_b_seek_ms.load(Ordering::Acquire),
            50_000
        );
        assert_eq!(callback.state(), EngineState::Buffering);
    }

    #[test]
    fn normalization_toggle_updates_active_and_preloaded_decks() {
        let (mut callback, _) = callback();
        load(
            &mut callback,
            DeckId::A,
            meta(1, 10_000, "album:1", -6.0),
            10,
            2,
            Vec::new(),
        );
        load(
            &mut callback,
            DeckId::B,
            meta(2, 10_000, "album:2", -12.0),
            10,
            2,
            Vec::new(),
        );

        callback.handle_command(AudioCommand::SetNormalization(false));
        assert_eq!(callback.deck_mgr.deck_a.norm_gain, 1.0);
        assert_eq!(callback.deck_mgr.deck_b.norm_gain, 1.0);

        callback.handle_command(AudioCommand::SetNormalization(true));
        assert!((callback.deck_mgr.deck_a.norm_gain - 10.0_f32.powf(-6.0 / 20.0)).abs() < 1e-6);
        assert!((callback.deck_mgr.deck_b.norm_gain - 10.0_f32.powf(-12.0 / 20.0)).abs() < 1e-6);

        callback.handle_command(AudioCommand::UpdateTrackAnalysis {
            rating_key: 1,
            gain_db: Some(-3.0),
            intro_end_ms: None,
            outro_start_ms: Some(8_000),
            fade_start_ms: Some(9_000),
            silence_start_ms: Some(9_800),
        });
        assert!((callback.deck_mgr.deck_a.norm_gain - 10.0_f32.powf(-3.0 / 20.0)).abs() < 1e-6);
        assert_eq!(
            callback.deck_mgr.deck_a.meta.as_ref().unwrap().gain_db,
            Some(-3.0)
        );
        assert!((callback.deck_mgr.deck_b.norm_gain - 10.0_f32.powf(-12.0 / 20.0)).abs() < 1e-6);
        assert!(callback.scheduler.check(8.99, 10.0).is_none());
        assert_eq!(
            callback.scheduler.check(9.0, 10.0),
            Some(SchedulerAction::TransitionPoint)
        );
    }

    #[test]
    fn next_preload_waits_until_outgoing_crossfade_deck_is_retired() {
        let (mut callback, _) = callback();
        callback.device_sample_rate = 10;
        callback.device_channels = 2;
        load(
            &mut callback,
            DeckId::B,
            meta(1, 100_000, "album:1", 0.0),
            10,
            2,
            vec![0.25; 2_000],
        );
        callback.handle_transition_to_active(true);
        load(
            &mut callback,
            DeckId::A,
            meta(2, 100_000, "album:2", 0.0),
            10,
            2,
            vec![0.25; 2_000],
        );
        callback.handle_transition_to_active(true);
        assert!(callback.is_crossfading);

        load(
            &mut callback,
            DeckId::B,
            meta(3, 100_000, "album:3", 0.0),
            10,
            2,
            Vec::new(),
        );
        assert_eq!(callback.deck_mgr.pending_deck().rating_key(), 1);
        callback.handle_command(AudioCommand::UpdateTrackAnalysis {
            rating_key: 3,
            gain_db: Some(-6.0),
            intro_end_ms: None,
            outro_start_ms: Some(90_000),
            fade_start_ms: Some(95_000),
            silence_start_ms: Some(99_000),
        });

        callback.process_callback(&mut [0.0; 8]);
        assert!(callback.is_crossfading);
        assert_eq!(callback.deck_mgr.pending_deck().rating_key(), 1);

        callback.process_callback(&mut [0.0; 4]);
        assert!(!callback.is_crossfading);
        assert_eq!(callback.deck_mgr.pending_deck().rating_key(), 3);
        assert!(
            (callback.deck_mgr.pending_deck().norm_gain - 10.0_f32.powf(-6.0 / 20.0)).abs() < 1e-6
        );
        assert_eq!(
            callback
                .deck_mgr
                .pending_deck()
                .meta
                .as_ref()
                .unwrap()
                .fade_start_ms,
            Some(95_000)
        );
    }

    #[test]
    fn published_position_uses_source_frames_not_output_channel_count() {
        let (mut callback, _) = callback();
        callback.device_sample_rate = 10;
        callback.device_channels = 2;
        load(
            &mut callback,
            DeckId::B,
            meta(1, 1_000, "album:1", 0.0),
            10,
            6,
            vec![0.0; 60],
        );
        callback.handle_transition_to_active(true);

        callback.process_callback(&mut [0.0; 4]);

        assert_eq!(callback.atomics.position_frames.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn decoded_end_uses_preloaded_track_when_metadata_duration_runs_long() {
        let (mut callback, event_rx) = callback();
        callback.device_sample_rate = 10;
        callback.device_channels = 2;
        load(
            &mut callback,
            DeckId::B,
            meta(1, 1_100, "album:1", 0.0),
            10,
            2,
            vec![0.0; 20],
        );
        callback.handle_transition_to_active(true);
        callback.deck_mgr.active_deck_mut().fully_decoded = true;
        load(
            &mut callback,
            DeckId::A,
            meta(2, 1_000, "album:2", 0.0),
            10,
            2,
            vec![0.0; 20],
        );
        callback.deck_mgr.pending_deck_mut().fully_decoded = true;
        callback.handle_command(AudioCommand::UpdateCrossfadeSettings(CrossfadeSettings {
            crossfade_window_ms: 0,
            smart_crossfade: false,
            smart_crossfade_max_ms: 0,
            ..CrossfadeSettings::default()
        }));

        callback.process_callback(&mut [0.0; 20]);

        assert_eq!(callback.deck_mgr.active_deck().rating_key(), 2);
        assert_eq!(callback.state(), EngineState::Playing);
        assert!(event_rx
            .try_iter()
            .any(|event| matches!(event, EngineEvent::TrackEnded { rating_key: 1 })));
    }

    #[test]
    fn gapless_transition_waits_for_pending_pcm_then_retries() {
        let (mut callback, _) = callback();
        callback.device_sample_rate = 10;
        callback.device_channels = 2;
        load(
            &mut callback,
            DeckId::B,
            meta(1, 1_000, "album:1", 0.0),
            10,
            2,
            vec![0.0; 20],
        );
        callback.handle_transition_to_active(true);
        callback.deck_mgr.active_deck_mut().fully_decoded = true;
        load(
            &mut callback,
            DeckId::A,
            meta(2, 1_000, "album:1", 0.0),
            10,
            2,
            Vec::new(),
        );
        callback.handle_command(AudioCommand::UpdateCrossfadeSettings(CrossfadeSettings {
            crossfade_window_ms: 0,
            smart_crossfade: false,
            smart_crossfade_max_ms: 0,
            ..CrossfadeSettings::default()
        }));
        let active = callback.deck_mgr.active_deck_mut();
        let consumed = active.samples.consume(20);
        active.sample_offset += consumed;

        callback.tick_scheduler();
        assert_eq!(callback.deck_mgr.active_deck().rating_key(), 1);

        assert!(callback
            .deck_mgr
            .pending_deck_mut()
            .samples
            .push(&[0.0; 20]));
        callback.deck_mgr.pending_deck_mut().fully_decoded = true;
        callback.tick_scheduler();
        assert_eq!(callback.deck_mgr.active_deck().rating_key(), 2);
    }

    #[test]
    fn stale_batches_do_not_delay_current_generation_pcm() {
        let (mut callback, _, deck_a_tx, _) = callback_with_deck_senders();
        load(
            &mut callback,
            DeckId::A,
            meta(2, 1_000, "album:1", 0.0),
            10,
            2,
            Vec::new(),
        );
        deck_a_tx
            .send(SampleBatch {
                rating_key: 1,
                generation: 0,
                samples: vec![0.25; 20],
                fully_decoded: false,
            })
            .unwrap();
        deck_a_tx
            .send(SampleBatch {
                rating_key: 2,
                generation: 0,
                samples: vec![0.5; 20],
                fully_decoded: false,
            })
            .unwrap();

        callback.drain_deck_channel(DeckId::A);

        assert_eq!(callback.deck_mgr.deck_a.samples.to_vec(), vec![0.5; 20]);
    }

    #[test]
    fn gapless_handoff_fills_the_same_device_buffer() {
        let (mut callback, event_rx) = callback();
        callback.device_sample_rate = 10;
        callback.device_channels = 2;
        load(
            &mut callback,
            DeckId::B,
            meta(1, 400, "album:1", 0.0),
            10,
            2,
            vec![0.25; 8],
        );
        callback.handle_transition_to_active(true);
        callback.deck_mgr.active_deck_mut().fully_decoded = true;
        load(
            &mut callback,
            DeckId::A,
            meta(2, 1_000, "album:1", 0.0),
            10,
            2,
            vec![0.75; 20],
        );
        callback.deck_mgr.pending_deck_mut().fully_decoded = true;
        callback.handle_command(AudioCommand::UpdateCrossfadeSettings(CrossfadeSettings {
            crossfade_window_ms: 4_000,
            same_album_crossfade: false,
            ..CrossfadeSettings::default()
        }));

        let mut output = vec![0.0; 12]; // Six frames: four outgoing, two incoming.
        callback.render_audio(&mut output);

        assert_eq!(&output[..8], &[0.25; 8]);
        assert_eq!(&output[8..], &[0.75; 4]);
        assert_eq!(callback.deck_mgr.active_deck().rating_key(), 2);
        assert_eq!(callback.deck_mgr.active_deck().sample_offset, 4);
        assert!(event_rx
            .try_iter()
            .any(|event| matches!(event, EngineEvent::TrackEnded { rating_key: 1 })));
    }
}
