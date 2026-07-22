//! High-quality stateful streaming audio resampler.
//!
//! The output rate is the current rate reported by the selected OS audio
//! device. A fixed-ratio FFT resampler provides anti-alias filtering for common
//! conversions such as 44.1 kHz to 48/96 kHz while preserving state across
//! decoder batches.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};

const RESAMPLER_CHUNK_FRAMES: usize = 4_096;

pub struct StreamingResampler {
    channels: usize,
    source_rate: u64,
    target_rate: u64,
    resampler: Fft<f32>,
    input: Vec<f32>,
    input_offset: usize,
    input_frames: u64,
    output_frames: u64,
    delay_frames: usize,
    finished: bool,
}

impl StreamingResampler {
    pub fn new(channels: u16, source_rate: u32, target_rate: u32) -> Result<Self, String> {
        let channels = usize::from(channels);
        if channels == 0 || source_rate == 0 || target_rate == 0 {
            return Err("audio resampler requires non-zero channels and sample rates".into());
        }
        let resampler = Fft::<f32>::new(
            source_rate as usize,
            target_rate as usize,
            RESAMPLER_CHUNK_FRAMES,
            channels,
            FixedSync::Input,
        )
        .map_err(|error| format!("could not create audio resampler: {error}"))?;
        let delay_frames = resampler.output_delay();

        Ok(Self {
            channels,
            source_rate: u64::from(source_rate),
            target_rate: u64::from(target_rate),
            resampler,
            input: Vec::with_capacity(RESAMPLER_CHUNK_FRAMES * channels * 2),
            input_offset: 0,
            input_frames: 0,
            output_frames: 0,
            delay_frames,
            finished: false,
        })
    }

    pub fn process(&mut self, samples: &[f32]) -> Result<Vec<f32>, String> {
        if self.finished {
            return Ok(Vec::new());
        }
        let complete_samples = samples.len() / self.channels * self.channels;
        self.input.extend_from_slice(&samples[..complete_samples]);
        self.input_frames = self
            .input_frames
            .saturating_add((complete_samples / self.channels) as u64);

        let mut output = Vec::new();
        loop {
            let needed_frames = self.resampler.input_frames_next();
            if self.available_input_frames() < needed_frames {
                break;
            }
            let raw = self.process_chunk(needed_frames, None)?;
            self.append_output(&mut output, raw, None);
        }
        self.compact_input();
        Ok(output)
    }

    pub fn finish(&mut self) -> Result<Vec<f32>, String> {
        if self.finished {
            return Ok(Vec::new());
        }

        let target_frames = self
            .input_frames
            .saturating_mul(self.target_rate)
            .saturating_add(self.source_rate - 1)
            / self.source_rate;
        let mut output = Vec::new();
        let remaining_frames = self.available_input_frames();
        if remaining_frames > 0 {
            let raw = self.process_chunk(remaining_frames, Some(remaining_frames))?;
            self.append_output(&mut output, raw, Some(target_frames));
        }

        // Flush the FFT overlap with zero-length partial chunks until all
        // source-duration output frames have been emitted.
        while self.output_frames < target_frames {
            let needed_frames = self.resampler.input_frames_next();
            let silence = vec![0.0; needed_frames * self.channels];
            let adapter = InterleavedSlice::new(&silence, self.channels, needed_frames)
                .map_err(|error| format!("could not prepare resampler flush input: {error}"))?;
            let indexing = Indexing {
                partial_len: Some(0),
                ..Indexing::default()
            };
            let raw = self
                .resampler
                .process(&adapter, Some(&indexing))
                .map_err(|error| format!("could not flush audio resampler: {error}"))?
                .take_data();
            let before = self.output_frames;
            self.append_output(&mut output, raw, Some(target_frames));
            if self.output_frames == before {
                return Err("audio resampler flush made no progress".into());
            }
        }

        self.finished = true;
        self.input.clear();
        self.input_offset = 0;
        Ok(output)
    }

    fn process_chunk(
        &mut self,
        consumed_frames: usize,
        partial_len: Option<usize>,
    ) -> Result<Vec<f32>, String> {
        let start = self.input_offset;
        let end = start.saturating_add(consumed_frames.saturating_mul(self.channels));
        let input = self
            .input
            .get(start..end)
            .ok_or_else(|| "audio resampler input accounting was inconsistent".to_string())?;
        let adapter = InterleavedSlice::new(input, self.channels, consumed_frames)
            .map_err(|error| format!("could not prepare resampler input: {error}"))?;
        let indexing = partial_len.map(|frames| Indexing {
            partial_len: Some(frames),
            ..Indexing::default()
        });
        let output = self
            .resampler
            .process(&adapter, indexing.as_ref())
            .map_err(|error| format!("could not resample audio: {error}"))?
            .take_data();
        self.input_offset = end;
        Ok(output)
    }

    fn append_output(&mut self, target: &mut Vec<f32>, raw: Vec<f32>, cap: Option<u64>) {
        let raw_frames = raw.len() / self.channels;
        let skipped_frames = self.delay_frames.min(raw_frames);
        self.delay_frames -= skipped_frames;
        let available_frames = raw_frames.saturating_sub(skipped_frames);
        let accepted_frames = cap.map_or(available_frames, |limit| {
            available_frames.min(limit.saturating_sub(self.output_frames) as usize)
        });
        let start = skipped_frames * self.channels;
        let end = start + accepted_frames * self.channels;
        target.extend_from_slice(&raw[start..end]);
        self.output_frames = self.output_frames.saturating_add(accepted_frames as u64);
    }

    fn available_input_frames(&self) -> usize {
        self.input.len().saturating_sub(self.input_offset) / self.channels
    }

    fn compact_input(&mut self) {
        if self.input_offset == 0 {
            return;
        }
        if self.input_offset >= self.input.len() {
            self.input.clear();
            self.input_offset = 0;
        } else if self.input_offset >= RESAMPLER_CHUNK_FRAMES * self.channels {
            self.input.drain(..self.input_offset);
            self.input_offset = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo_sine(frames: usize, sample_rate: f32) -> Vec<f32> {
        let mut samples = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            let time = frame as f32 / sample_rate;
            let sample = (2.0 * std::f32::consts::PI * 440.0 * time).sin();
            samples.extend_from_slice(&[sample, sample]);
        }
        samples
    }

    fn resample_in_chunks(samples: &[f32], chunk_frames: usize, target_rate: u32) -> Vec<f32> {
        let mut resampler = StreamingResampler::new(2, 44_100, target_rate).unwrap();
        let mut output = Vec::new();
        for chunk in samples.chunks(chunk_frames * 2) {
            output.extend(resampler.process(chunk).unwrap());
        }
        output.extend(resampler.finish().unwrap());
        output
    }

    #[test]
    fn resample_44100_to_48000_has_exact_duration() {
        let samples = stereo_sine(44_100, 44_100.0);
        let output = resample_in_chunks(&samples, 44_100, 48_000);
        assert_eq!(output.len() / 2, 48_000);
    }

    #[test]
    fn resample_44100_to_96000_has_exact_duration() {
        let samples = stereo_sine(44_100, 44_100.0);
        let output = resample_in_chunks(&samples, 7_919, 96_000);
        assert_eq!(output.len() / 2, 96_000);
    }

    #[test]
    fn decoder_batch_boundaries_do_not_change_output() {
        let samples = stereo_sine(44_100 * 3, 44_100.0);
        let whole = resample_in_chunks(&samples, samples.len() / 2, 96_000);
        let one_second_batches = resample_in_chunks(&samples, 44_100, 96_000);
        let uneven_batches = resample_in_chunks(&samples, 7_919, 96_000);

        assert_eq!(whole, one_second_batches);
        assert_eq!(whole, uneven_batches);
    }

    #[test]
    fn finish_is_idempotent() {
        let mut resampler = StreamingResampler::new(1, 48_000, 44_100).unwrap();
        let input = vec![0.5; 48_000];
        let mut output = resampler.process(&input).unwrap();
        output.extend(resampler.finish().unwrap());

        assert_eq!(output.len(), 44_100);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(resampler.finish().unwrap().is_empty());
    }
}
