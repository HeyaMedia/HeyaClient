//! Visualizer — FFT frequency analysis and time-domain sample delivery.
//!
//! The audio callback sends raw PCM chunks via a lock-free channel.
//! The visualizer task accumulates them into a rolling buffer, computes
//! FFT at ~30fps, and emits VisFrame events to JS.

pub mod fft;

use self::fft::{downmix_to_mono, FftAnalyzer, FFT_SIZE};

const TIME_DOMAIN_SAMPLES: usize = 512;

/// Processes audio samples for visualizer output.
///
/// Accumulates interleaved PCM into a rolling buffer. When enough
/// samples are present (≥ FFT_SIZE mono frames), computes FFT and
/// returns time-domain + frequency-domain data for the JS event.
pub struct VisualizerProcessor {
    /// Rolling sample buffer (interleaved, same format as audio output).
    buffer: Vec<f32>,
    /// Number of interleaved channels.
    channels: u16,
    /// Max buffer size — keeps the last N samples to bound memory.
    max_samples: usize,
    /// Cached FFT plan + scratch, reused every frame.
    analyzer: FftAnalyzer,
}

impl VisualizerProcessor {
    pub fn new(channels: u16) -> Self {
        // Keep enough for 2x FFT_SIZE mono frames (gives us headroom)
        let max_samples = FFT_SIZE * 2 * channels.max(1) as usize;
        Self {
            buffer: Vec::with_capacity(max_samples),
            channels,
            max_samples,
            analyzer: FftAnalyzer::new(),
        }
    }

    /// Push new interleaved PCM samples into the rolling buffer.
    pub fn push_samples(&mut self, samples: &[f32]) {
        self.buffer.extend_from_slice(samples);
        // Trim from the front if we've exceeded max size
        if self.buffer.len() > self.max_samples {
            let excess = self.buffer.len() - self.max_samples;
            self.buffer.drain(..excess);
        }
    }

    /// Compute FFT from the accumulated buffer.
    /// Returns `(time_domain_samples, frequency_bins)` or `None` if not enough data.
    pub fn compute(&mut self) -> Option<(Vec<f32>, Vec<f32>)> {
        let ch = self.channels.max(1) as usize;
        let min_interleaved = FFT_SIZE * ch;

        if self.buffer.len() < min_interleaved {
            return None;
        }

        // Downmix to mono
        let mono = downmix_to_mono(&self.buffer, self.channels);

        // Take the last FFT_SIZE mono samples
        let start = mono.len().saturating_sub(FFT_SIZE);
        let window = &mono[start..start + FFT_SIZE.min(mono.len() - start)];

        // Scope/VU rendering does not need the full FFT window. Keeping only
        // the newest 512 samples cuts the largest per-frame WebView payload by
        // 75% without changing the frequency-bin resolution.
        let time_domain = window[window.len().saturating_sub(TIME_DOMAIN_SAMPLES)..].to_vec();

        // Frequency-domain bins (dB)
        let bins = self.analyzer.compute(window);

        Some((time_domain, bins))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_compact_time_domain_with_full_frequency_resolution() {
        let mut processor = VisualizerProcessor::new(2);
        processor.push_samples(&vec![0.25; FFT_SIZE * 2]);

        let (time_domain, frequency_bins) = processor.compute().expect("visualizer frame");

        assert_eq!(time_domain.len(), TIME_DOMAIN_SAMPLES);
        assert_eq!(frequency_bins.len(), fft::NUM_BINS);
    }
}
