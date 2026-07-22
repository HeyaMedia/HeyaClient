//! Stateful streaming audio resampler.
//!
//! Decoder output arrives in independent batches. Resampling each batch from
//! scratch loses the fractional source position at every boundary and repeats
//! the final source frame, which can produce an audible tick for common
//! conversions such as 44.1 kHz to 48 kHz. This resampler keeps the previous
//! frame and an exact rational output position for the lifetime of a track.

/// Linear streaming resampler for interleaved PCM.
///
/// Linear interpolation is intentionally retained for now, but unlike the old
/// batch helper this type is continuous across decoder batches and produces an
/// exact, deterministic number of output frames at end-of-stream.
pub struct StreamingResampler {
    channels: usize,
    source_rate: u64,
    target_rate: u64,
    input_frames: u64,
    output_frames: u64,
    previous_frame: Vec<f32>,
    finished: bool,
}

impl StreamingResampler {
    pub fn new(channels: u16, source_rate: u32, target_rate: u32) -> Self {
        Self {
            channels: usize::from(channels),
            source_rate: u64::from(source_rate),
            target_rate: u64::from(target_rate),
            input_frames: 0,
            output_frames: 0,
            previous_frame: Vec::new(),
            finished: false,
        }
    }

    /// Resample another decoder batch. The final source frame is retained until
    /// the following batch so interpolation can cross the boundary correctly.
    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        self.process_inner(samples, false)
    }

    /// Flush the final fractional frame after the decoder reaches clean EOF.
    pub fn finish(&mut self) -> Vec<f32> {
        self.process_inner(&[], true)
    }

    fn process_inner(&mut self, samples: &[f32], end_of_stream: bool) -> Vec<f32> {
        if self.finished || self.channels == 0 || self.source_rate == 0 || self.target_rate == 0 {
            return Vec::new();
        }

        let batch_frames = samples.len() / self.channels;
        let batch_start = self.input_frames;
        self.input_frames = self.input_frames.saturating_add(batch_frames as u64);

        if self.input_frames == 0 {
            if end_of_stream {
                self.finished = true;
            }
            return Vec::new();
        }

        let final_output_frames = end_of_stream.then(|| {
            self.input_frames
                .saturating_mul(self.target_rate)
                .saturating_add(self.source_rate - 1)
                / self.source_rate
        });
        let estimated_frames = if let Some(total) = final_output_frames {
            total.saturating_sub(self.output_frames)
        } else {
            (batch_frames as u64)
                .saturating_mul(self.target_rate)
                .saturating_add(self.source_rate - 1)
                / self.source_rate
        };
        let mut output = Vec::with_capacity(
            usize::try_from(estimated_frames)
                .unwrap_or(0)
                .saturating_mul(self.channels),
        );

        loop {
            if final_output_frames.is_some_and(|total| self.output_frames >= total) {
                break;
            }

            let source_position = self.output_frames.saturating_mul(self.source_rate);
            let frame_a = source_position / self.target_rate;
            let remainder = source_position % self.target_rate;

            // Until EOF, retain the newest frame so the next decoder batch can
            // provide the right-hand interpolation sample.
            if !end_of_stream && frame_a.saturating_add(1) >= self.input_frames {
                break;
            }

            let last_frame = self.input_frames - 1;
            let frame_b = frame_a.saturating_add(1).min(last_frame);
            let fraction = remainder as f32 / self.target_rate as f32;

            for channel in 0..self.channels {
                let a = self.sample_at(samples, batch_start, frame_a.min(last_frame), channel);
                let b = self.sample_at(samples, batch_start, frame_b, channel);
                output.push(a + (b - a) * fraction);
            }
            self.output_frames = self.output_frames.saturating_add(1);
        }

        if batch_frames > 0 {
            let start = (batch_frames - 1) * self.channels;
            self.previous_frame.clear();
            self.previous_frame
                .extend_from_slice(&samples[start..start + self.channels]);
        }
        if end_of_stream {
            self.finished = true;
        }

        output
    }

    fn sample_at(&self, samples: &[f32], batch_start: u64, frame: u64, channel: usize) -> f32 {
        if frame < batch_start {
            return self.previous_frame.get(channel).copied().unwrap_or(0.0);
        }

        let local_frame = usize::try_from(frame - batch_start).unwrap_or(usize::MAX);
        samples
            .get(
                local_frame
                    .saturating_mul(self.channels)
                    .saturating_add(channel),
            )
            .copied()
            .or_else(|| self.previous_frame.get(channel).copied())
            .unwrap_or(0.0)
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

    fn resample_in_chunks(samples: &[f32], chunk_frames: usize) -> Vec<f32> {
        let mut resampler = StreamingResampler::new(2, 44_100, 48_000);
        let mut output = Vec::new();
        for chunk in samples.chunks(chunk_frames * 2) {
            output.extend(resampler.process(chunk));
        }
        output.extend(resampler.finish());
        output
    }

    #[test]
    fn resample_44100_to_48000_has_exact_duration() {
        let samples = stereo_sine(44_100, 44_100.0);
        let output = resample_in_chunks(&samples, 44_100);
        assert_eq!(output.len() / 2, 48_000);
    }

    #[test]
    fn decoder_batch_boundaries_do_not_change_output() {
        let samples = stereo_sine(44_100 * 3, 44_100.0);
        let whole = resample_in_chunks(&samples, samples.len() / 2);
        let one_second_batches = resample_in_chunks(&samples, 44_100);
        let uneven_batches = resample_in_chunks(&samples, 7_919);

        assert_eq!(whole, one_second_batches);
        assert_eq!(whole, uneven_batches);
    }

    #[test]
    fn finish_is_idempotent() {
        let mut resampler = StreamingResampler::new(1, 48_000, 44_100);
        let input = vec![0.5; 48_000];
        let mut output = resampler.process(&input);
        output.extend(resampler.finish());

        assert_eq!(output.len(), 44_100);
        assert!(output
            .iter()
            .all(|sample| (*sample - 0.5).abs() < f32::EPSILON));
        assert!(resampler.finish().is_empty());
    }
}
