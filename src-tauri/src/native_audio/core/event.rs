//! Events emitted from the engine back to the JS frontend via Tauri events.

use serde::Serialize;

/// Events the engine emits to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EngineEvent {
    State {
        state: String,
    },
    /// The active decoded PCM queue ran dry before the source finished.
    BufferUnderrun,
    TrackStarted {
        rating_key: i64,
        duration_ms: u64,
    },
    TrackEnded {
        rating_key: i64,
    },
    Format {
        rating_key: i64,
        source_sample_rate: u32,
        source_channels: u16,
        output_sample_rate: u32,
        output_channels: u16,
    },
    Error {
        message: String,
    },
    /// A pending deck failed to prepare. Active playback remains authoritative
    /// and the next explicit Play command can cold-load the track.
    PreloadError {
        rating_key: i64,
        message: String,
    },
    VisFrame {
        /// Compact mono time-domain samples for scope/VU rendering.
        samples: Vec<f32>,
        /// FFT frequency bins in dB.
        frequency_bins: Vec<f32>,
    },
}
