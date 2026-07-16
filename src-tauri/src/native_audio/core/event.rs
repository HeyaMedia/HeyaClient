//! Events emitted from the engine back to the JS frontend via Tauri events.

use serde::Serialize;

/// Events the engine emits to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EngineEvent {
    Position {
        position_ms: u64,
        duration_ms: u64,
    },
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
    VisFrame {
        /// Time-domain samples (interleaved f32, but sent as Vec<f32> for serde).
        samples: Vec<f32>,
        /// FFT frequency bins in dB.
        frequency_bins: Vec<f32>,
    },
}
