//! Lightweight Meier-style headphone crossfeed.
//!
//! A low-passed, ~0.3 ms delayed copy of each stereo channel is mixed into
//! the opposite ear. Direct + bleed gains sum to one so presets do not create
//! an obvious level jump. Mono and multichannel sources are left untouched.

use super::traits::DspBlock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossfeedPreset {
    Subtle,
    Natural,
    Strong,
}

impl CrossfeedPreset {
    fn parameters(self) -> (f32, f32) {
        match self {
            Self::Subtle => (0.15, 500.0),
            Self::Natural => (0.30, 700.0),
            Self::Strong => (0.45, 900.0),
        }
    }
}

pub struct Crossfeed {
    enabled: bool,
    preset: CrossfeedPreset,
    sample_rate: u32,
    low_l: f32,
    low_r: f32,
    delay_l: Vec<f32>,
    delay_r: Vec<f32>,
    delay_index: usize,
}

impl Crossfeed {
    pub fn new(sample_rate: u32) -> Self {
        let mut crossfeed = Self {
            enabled: false,
            preset: CrossfeedPreset::Natural,
            sample_rate,
            low_l: 0.0,
            low_r: 0.0,
            delay_l: Vec::new(),
            delay_r: Vec::new(),
            delay_index: 0,
        };
        crossfeed.rebuild_delay();
        crossfeed
    }

    pub fn set_preset(&mut self, preset: CrossfeedPreset) {
        self.preset = preset;
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        if self.sample_rate == sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.rebuild_delay();
    }

    pub fn reset(&mut self) {
        self.low_l = 0.0;
        self.low_r = 0.0;
        self.delay_l.fill(0.0);
        self.delay_r.fill(0.0);
        self.delay_index = 0;
    }

    fn rebuild_delay(&mut self) {
        let frames = ((self.sample_rate as f64 * 0.0003).round() as usize).max(1);
        self.delay_l = vec![0.0; frames];
        self.delay_r = vec![0.0; frames];
        self.reset();
    }
}

impl DspBlock for Crossfeed {
    fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.reset();
        }
    }

    fn process(&mut self, samples: &mut [f32], sample_rate: u32, channels: u16) {
        if !self.enabled || channels != 2 {
            return;
        }
        if sample_rate != self.sample_rate {
            self.set_sample_rate(sample_rate);
        }

        let (bleed, cutoff) = self.preset.parameters();
        let direct = 1.0 - bleed;
        let alpha = 1.0 - (-2.0 * std::f32::consts::PI * cutoff / sample_rate as f32).exp();

        for frame in samples.chunks_exact_mut(2) {
            let left = frame[0];
            let right = frame[1];
            self.low_l += alpha * (left - self.low_l);
            self.low_r += alpha * (right - self.low_r);

            let delayed_l = self.delay_l[self.delay_index];
            let delayed_r = self.delay_r[self.delay_index];
            self.delay_l[self.delay_index] = self.low_l;
            self.delay_r[self.delay_index] = self.low_r;
            self.delay_index = (self.delay_index + 1) % self.delay_l.len();

            frame[0] = direct * left + bleed * delayed_r;
            frame[1] = direct * right + bleed * delayed_l;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_crossfeed_is_passthrough() {
        let mut block = Crossfeed::new(48_000);
        let mut samples = vec![1.0, 0.0, 0.5, -0.5];
        let original = samples.clone();
        block.process(&mut samples, 48_000, 2);
        assert_eq!(samples, original);
    }

    #[test]
    fn enabled_crossfeed_bleeds_left_into_right() {
        let mut block = Crossfeed::new(48_000);
        block.set_enabled(true);
        let mut samples = vec![0.0; 256 * 2];
        for frame in samples.chunks_exact_mut(2) {
            frame[0] = 1.0;
        }
        block.process(&mut samples, 48_000, 2);
        assert!(samples
            .chunks_exact(2)
            .skip(32)
            .any(|frame| frame[1] > 0.01));
    }

    #[test]
    fn mono_is_untouched() {
        let mut block = Crossfeed::new(44_100);
        block.set_enabled(true);
        let mut samples = vec![1.0, -0.5, 0.25];
        let original = samples.clone();
        block.process(&mut samples, 44_100, 1);
        assert_eq!(samples, original);
    }
}
