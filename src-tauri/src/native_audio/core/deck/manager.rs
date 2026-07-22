//! Deck manager — orchestrates dual decks with active/pending role swapping.
//!
//! Mirrors the TypeScript DeckManager: two reusable decks, one active (playing)
//! and one pending (preloaded with next track). Transitions swap roles.

use super::super::types::{DeckId, TrackMeta};

/// Fixed-capacity interleaved PCM ring. Its backing allocation is created off
/// the real-time thread, then reused as tracks advance so decoded audio cannot
/// grow to the size of an entire album track.
#[derive(Debug)]
pub struct PcmRing {
    storage: Vec<f32>,
    read: usize,
    write: usize,
    queued: usize,
}

impl PcmRing {
    pub fn new() -> Self {
        Self {
            storage: Vec::new(),
            read: 0,
            write: 0,
            queued: 0,
        }
    }

    pub fn from_storage(storage: Vec<f32>) -> Self {
        Self {
            storage,
            read: 0,
            write: 0,
            queued: 0,
        }
    }

    #[cfg(test)]
    pub fn from_samples(samples: Vec<f32>) -> Self {
        let queued = samples.len();
        let write = if samples.capacity() == 0 {
            0
        } else {
            queued % samples.capacity()
        };
        Self {
            storage: samples,
            read: 0,
            write,
            queued,
        }
    }

    #[cfg(test)]
    pub fn to_vec(&self) -> Vec<f32> {
        (0..self.queued)
            .filter_map(|index| self.get(index))
            .collect()
    }

    pub fn capacity(&self) -> usize {
        self.storage.capacity()
    }

    pub fn len(&self) -> usize {
        self.queued
    }

    pub fn is_empty(&self) -> bool {
        self.queued == 0
    }

    pub fn free(&self) -> usize {
        self.capacity().saturating_sub(self.queued)
    }

    pub fn get(&self, offset: usize) -> Option<f32> {
        if offset >= self.queued || self.capacity() == 0 {
            return None;
        }
        self.storage
            .get((self.read + offset) % self.capacity())
            .copied()
    }

    pub fn push(&mut self, samples: &[f32]) -> bool {
        if samples.len() > self.free() {
            return false;
        }
        let capacity = self.capacity();
        if samples.is_empty() {
            return true;
        }

        let mut copied = 0;
        while copied < samples.len() {
            let writable = (capacity - self.write).min(samples.len() - copied);
            let initialized = self.storage.len().saturating_sub(self.write).min(writable);
            if initialized > 0 {
                self.storage[self.write..self.write + initialized]
                    .copy_from_slice(&samples[copied..copied + initialized]);
                self.write = (self.write + initialized) % capacity;
                copied += initialized;
                continue;
            }

            // Until the first wrap, grow the Vec into its pre-reserved backing.
            let grow = writable.min(capacity.saturating_sub(self.storage.len()));
            debug_assert!(grow > 0);
            self.storage
                .extend_from_slice(&samples[copied..copied + grow]);
            self.write = (self.write + grow) % capacity;
            copied += grow;
        }
        self.queued += samples.len();
        true
    }

    pub fn consume(&mut self, samples: usize) -> usize {
        let consumed = samples.min(self.queued);
        if self.capacity() > 0 {
            self.read = (self.read + consumed) % self.capacity();
        }
        self.queued -= consumed;
        consumed
    }

    pub fn clear(&mut self) {
        self.read = 0;
        self.write = 0;
        self.queued = 0;
    }

    pub fn replace_storage(&mut self, storage: Vec<f32>) -> Vec<f32> {
        let old = std::mem::replace(self, Self::from_storage(storage));
        old.storage
    }
}

impl Default for PcmRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Active crossfade curve state — the mixer steps through this per-frame.
#[derive(Debug, Clone)]
pub struct FadeCurve {
    /// Gain values at each step (e.g., 400 steps for a 4s crossfade).
    pub values: Vec<f32>,
    /// Current index into `values`.
    pub index: usize,
    /// Number of audio frames between each curve step.
    /// Computed as `(duration_sec * sample_rate) / values.len()`.
    pub frames_per_step: f32,
    /// Accumulated fractional frames since last step advance.
    pub frame_accum: f32,
}

impl FadeCurve {
    pub fn new(values: Vec<f32>, total_frames: usize) -> Self {
        let frames_per_step = if values.is_empty() {
            1.0
        } else {
            total_frames as f32 / values.len() as f32
        };
        Self {
            values,
            index: 0,
            frames_per_step,
            frame_accum: 0.0,
        }
    }

    /// Get the current fade gain, or `None` if the curve is finished/empty.
    pub fn current_gain(&self) -> Option<f32> {
        self.values.get(self.index).copied()
    }

    /// Advance the curve by one audio frame. Call once per mixer frame.
    pub fn advance_frame(&mut self) {
        if self.index >= self.values.len() {
            return;
        }
        self.frame_accum += 1.0;
        while self.frame_accum >= self.frames_per_step && self.index < self.values.len() {
            self.frame_accum -= self.frames_per_step;
            self.index += 1;
        }
    }

    /// Is the curve finished (all steps consumed)?
    pub fn is_finished(&self) -> bool {
        self.values.is_empty() || self.index >= self.values.len()
    }
}

/// State of a single deck.
#[derive(Debug)]
pub struct DeckState {
    /// Decoded audio samples (interleaved f32).
    pub samples: PcmRing,
    /// Track metadata (None if deck is empty).
    pub meta: Option<TrackMeta>,
    /// Sample rate of the decoded audio.
    pub sample_rate: u32,
    /// Number of channels in the decoded audio.
    pub channels: u16,
    /// Whether the deck has finished loading.
    pub loaded: bool,
    /// Whether playback has started on this deck.
    pub has_started_playing: bool,
    /// Current fade gain (for crossfade). 1.0 = full volume, 0.0 = silent.
    pub fade_gain: f32,
    /// Per-track normalization gain (from ReplayGain dB). 1.0 = no normalization.
    pub norm_gain: f32,
    /// Active crossfade curve (if any). The mixer reads and advances this.
    pub fade_curve: Option<FadeCurve>,
    /// True once the decoder reached a clean end-of-stream.
    pub fully_decoded: bool,
    /// Absolute interleaved sample position of the ring's current read cursor.
    /// Advancing playback increments it; seeking resets it to the target.
    pub sample_offset: usize,
    /// Seek generation counter — incremented on each seek. Background decode
    /// threads stamp their `SampleBatch` with the generation at the time of
    /// decode. The audio callback rejects batches with a stale generation.
    pub generation: u64,
}

impl DeckState {
    pub fn new(_id: DeckId) -> Self {
        Self {
            samples: PcmRing::new(),
            meta: None,
            sample_rate: 44100,
            channels: 2,
            loaded: false,
            has_started_playing: false,
            fade_gain: 1.0,
            norm_gain: 1.0,
            fade_curve: None,
            fully_decoded: false,
            sample_offset: 0,
            generation: 0,
        }
    }

    /// Reset deck for reuse with a new track.
    pub fn reset(&mut self) {
        self.samples.clear();
        self.sample_offset = 0;
        self.meta = None;
        self.loaded = false;
        self.has_started_playing = false;
        self.fade_gain = 1.0;
        self.norm_gain = 1.0;
        self.fade_curve = None;
        self.fully_decoded = false;
        // generation is NOT reset — it's managed externally by seek logic
    }

    /// Current playback position in seconds (accounts for sample_offset after seek).
    pub fn position_secs(&self) -> f32 {
        if self.channels == 0 || self.sample_rate == 0 {
            return 0.0;
        }
        self.sample_offset as f32 / (self.sample_rate as f32 * self.channels as f32)
    }

    /// Total duration in seconds.
    ///
    /// Uses the metadata duration (from the play command) rather than the
    /// current buffer size, because during streaming/incremental decode the
    /// buffer is still growing. The metadata duration is the authoritative
    /// track length used for scheduler transition points and clock snapshots.
    pub fn duration_secs(&self) -> f32 {
        // Prefer metadata duration — it's known from the start
        if let Some(ref meta) = self.meta {
            if meta.duration_ms > 0 {
                return meta.duration_ms as f32 / 1000.0;
            }
        }
        // Fallback to buffer-based duration (only for tracks without metadata)
        if self.channels == 0 || self.sample_rate == 0 {
            return 0.0;
        }
        (self.sample_offset + self.samples.len()) as f32
            / (self.sample_rate as f32 * self.channels as f32)
    }

    /// Whether the deck has reached the end of the track.
    /// Only true when playback position is past the end AND the full track
    /// has been decoded. During incremental/streaming decode, the mixer may
    /// temporarily be at the end of the buffer while more samples are coming.
    pub fn is_finished(&self) -> bool {
        self.loaded && self.fully_decoded && self.samples.is_empty()
    }

    /// Milliseconds of decoded PCM available ahead of the read cursor.
    pub fn buffered_ahead_ms(&self) -> u64 {
        if self.channels == 0 || self.sample_rate == 0 {
            return 0;
        }
        let remaining = self.samples.len() as u64;
        remaining.saturating_mul(1000) / (u64::from(self.sample_rate) * u64::from(self.channels))
    }

    /// Rating key of the loaded track, or 0 if empty.
    pub fn rating_key(&self) -> i64 {
        self.meta.as_ref().map_or(0, |m| m.rating_key)
    }

    /// Parent key of the loaded track, or empty string.
    pub fn parent_key(&self) -> &str {
        self.meta.as_ref().map_or("", |m| m.parent_key.as_str())
    }
}

/// Manages two decks with active/pending role swapping.
pub struct DeckManager {
    pub deck_a: DeckState,
    pub deck_b: DeckState,
    active: DeckId,
}

impl DeckManager {
    pub fn new() -> Self {
        Self {
            deck_a: DeckState::new(DeckId::A),
            deck_b: DeckState::new(DeckId::B),
            active: DeckId::A,
        }
    }

    pub fn active_deck(&self) -> &DeckState {
        match self.active {
            DeckId::A => &self.deck_a,
            DeckId::B => &self.deck_b,
        }
    }

    pub fn active_deck_mut(&mut self) -> &mut DeckState {
        match self.active {
            DeckId::A => &mut self.deck_a,
            DeckId::B => &mut self.deck_b,
        }
    }

    pub fn pending_deck(&self) -> &DeckState {
        match self.active {
            DeckId::A => &self.deck_b,
            DeckId::B => &self.deck_a,
        }
    }

    pub fn pending_deck_mut(&mut self) -> &mut DeckState {
        match self.active {
            DeckId::A => &mut self.deck_b,
            DeckId::B => &mut self.deck_a,
        }
    }

    pub fn active_id(&self) -> DeckId {
        self.active
    }

    /// Get a mutable reference to a specific deck by ID.
    pub fn deck_mut(&mut self, id: DeckId) -> &mut DeckState {
        match id {
            DeckId::A => &mut self.deck_a,
            DeckId::B => &mut self.deck_b,
        }
    }

    /// Swap active and pending roles.
    pub fn swap_roles(&mut self) {
        self.active = self.active.other();
    }

    /// Stop both decks.
    pub fn stop_all(&mut self) {
        self.deck_a.reset();
        self.deck_b.reset();
    }

    /// Get mutable references to both decks (active first, pending second).
    /// Safe because they're always different physical decks.
    pub fn both_decks_mut(&mut self) -> (&mut DeckState, &mut DeckState) {
        match self.active {
            DeckId::A => (&mut self.deck_a, &mut self.deck_b),
            DeckId::B => (&mut self.deck_b, &mut self.deck_a),
        }
    }
}

impl Default for DeckManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deck_position_and_duration() {
        let mut deck = DeckState::new(DeckId::A);
        deck.sample_rate = 44100;
        deck.channels = 2;
        deck.samples = PcmRing::from_samples(vec![0.0; 44100 * 2]); // 1 second stereo
        deck.loaded = true;

        assert!((deck.duration_secs() - 1.0).abs() < 0.01);
        assert!((deck.position_secs() - 0.0).abs() < 0.01);

        deck.samples.consume(44100); // 0.5s into stereo buffer
        deck.sample_offset = 44100;
        assert!((deck.position_secs() - 0.5).abs() < 0.01);
    }

    #[test]
    fn swap_roles() {
        let mut dm = DeckManager::new();
        assert_eq!(dm.active_id(), DeckId::A);
        dm.swap_roles();
        assert_eq!(dm.active_id(), DeckId::B);
        dm.swap_roles();
        assert_eq!(dm.active_id(), DeckId::A);
    }

    #[test]
    fn reports_only_pcm_ahead_of_the_read_cursor() {
        let mut deck = DeckState::new(DeckId::A);
        deck.sample_rate = 48_000;
        deck.channels = 2;
        deck.samples = PcmRing::from_samples(vec![0.0; 48_000 * 2 * 3]);
        let consumed = deck.samples.consume(48_000 * 2);
        deck.sample_offset += consumed;
        assert_eq!(deck.buffered_ahead_ms(), 2_000);
    }

    #[test]
    fn reset_clears_deck() {
        let mut deck = DeckState::new(DeckId::A);
        deck.samples = PcmRing::from_samples(vec![1.0; 1000]);
        deck.sample_offset = 500;
        deck.loaded = true;
        deck.reset();
        assert!(deck.samples.is_empty());
        assert_eq!(deck.sample_offset, 0);
        assert!(!deck.loaded);
    }

    #[test]
    fn pcm_ring_reuses_consumed_capacity_after_wrapping() {
        let mut storage = Vec::with_capacity(6);
        storage.shrink_to(6);
        let mut ring = PcmRing::from_storage(storage);
        assert!(ring.push(&[1.0, 2.0, 3.0, 4.0]));
        assert_eq!(ring.consume(3), 3);
        assert!(ring.push(&[5.0, 6.0, 7.0, 8.0]));
        assert_eq!(ring.to_vec(), vec![4.0, 5.0, 6.0, 7.0, 8.0]);
        assert!(!ring.push(&[9.0, 10.0]));
    }
}
