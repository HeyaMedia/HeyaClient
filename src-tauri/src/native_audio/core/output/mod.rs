//! Audio output — cpal device stream.

pub mod mixer;
pub mod resample;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig, I24, U24};
use crossbeam_channel::Sender;
use std::str::FromStr;
use tracing::{debug, error};

use super::callback::AudioCallbackState;
use super::event::EngineEvent;

const CONVERSION_SCRATCH_SAMPLES: usize = 64 * 1024;

/// Holds the cpal output stream and related state.
pub struct CpalOutput {
    stream: Option<Stream>,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: String,
    pub device_id: String,
    pub device_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutputDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutputDevicesSnapshot {
    pub devices: Vec<AudioOutputDeviceInfo>,
    pub active_device_id: Option<String>,
    pub follows_system_default: bool,
}

// SAFETY: CpalOutput is only accessed from a single thread (the thread that
// creates it). The cpal Stream contains raw pointers which prevent auto-Send,
// but we ensure it's never sent across threads after creation.
unsafe impl Send for CpalOutput {}

impl CpalOutput {
    /// Open the default audio output device and create a stream.
    ///
    /// The `AudioCallbackState` is moved into the cpal closure — it owns all
    /// audio state directly. Zero mutexes on the audio path.
    pub fn open(
        mut cb_state: AudioCallbackState,
        preferred_device_id: Option<&str>,
        event_tx: Sender<EngineEvent>,
    ) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = resolve_output_device(&host, preferred_device_id)?;
        let device_id = device
            .id()
            .map_err(|error| format!("could not identify output device: {error}"))?
            .to_string();
        let device_name = device
            .description()
            .map(|description| description.name().to_string())
            .unwrap_or_else(|_| device.to_string());
        let config = device
            .default_output_config()
            .map_err(|e| format!("default output config: {e}"))?;
        let (sample_rate, channels, sample_format) = (
            config.sample_rate(),
            config.channels(),
            config.sample_format(),
        );

        debug!(
            device = %device_name,
            device_id = %device_id,
            sample_rate,
            channels,
            sample_format = %sample_format,
            "opening audio output"
        );

        // Configure callback state with device params
        cb_state.device_sample_rate = sample_rate;
        cb_state.device_channels = channels;
        cb_state.dsp_chain.set_sample_rate(sample_rate);
        cb_state
            .atomics
            .device_sample_rate
            .store(sample_rate, std::sync::atomic::Ordering::Relaxed);

        let stream_config = StreamConfig {
            channels,
            sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let stream = match sample_format {
            SampleFormat::F32 => build_f32_stream(&device, stream_config, cb_state, event_tx),
            SampleFormat::F64 => {
                build_converting_stream::<f64>(&device, stream_config, cb_state, event_tx)
            }
            SampleFormat::I8 => {
                build_converting_stream::<i8>(&device, stream_config, cb_state, event_tx)
            }
            SampleFormat::I16 => {
                build_converting_stream::<i16>(&device, stream_config, cb_state, event_tx)
            }
            SampleFormat::I24 => {
                build_converting_stream::<I24>(&device, stream_config, cb_state, event_tx)
            }
            SampleFormat::I32 => {
                build_converting_stream::<i32>(&device, stream_config, cb_state, event_tx)
            }
            SampleFormat::I64 => {
                build_converting_stream::<i64>(&device, stream_config, cb_state, event_tx)
            }
            SampleFormat::U8 => {
                build_converting_stream::<u8>(&device, stream_config, cb_state, event_tx)
            }
            SampleFormat::U16 => {
                build_converting_stream::<u16>(&device, stream_config, cb_state, event_tx)
            }
            SampleFormat::U24 => {
                build_converting_stream::<U24>(&device, stream_config, cb_state, event_tx)
            }
            SampleFormat::U32 => {
                build_converting_stream::<u32>(&device, stream_config, cb_state, event_tx)
            }
            SampleFormat::U64 => {
                build_converting_stream::<u64>(&device, stream_config, cb_state, event_tx)
            }
            unsupported => Err(format!("unsupported output sample format {unsupported}")),
        }?;

        stream.play().map_err(|e| format!("stream play: {}", e))?;

        Ok(Self {
            stream: Some(stream),
            sample_rate,
            channels,
            sample_format: sample_format.to_string(),
            device_id,
            device_name,
        })
    }

    /// Stop the OS render callback without tearing the stream down. The
    /// the stream can resume without rebuilding the decoder or callback.
    pub fn pause(&self) -> Result<(), String> {
        match &self.stream {
            Some(stream) => stream
                .pause()
                .map_err(|error| format!("stream pause: {error}")),
            None => Ok(()),
        }
    }

    pub fn resume(&self) -> Result<(), String> {
        match &self.stream {
            Some(stream) => stream
                .play()
                .map_err(|error| format!("stream play: {error}")),
            None => Ok(()),
        }
    }
}

pub fn enumerate_output_devices(
    preferred_device_id: Option<&str>,
) -> Result<AudioOutputDevicesSnapshot, String> {
    let host = cpal::default_host();
    let default_device_id = host
        .default_output_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let mut devices = host
        .output_devices()
        .map_err(|error| format!("could not enumerate output devices: {error}"))?
        .filter_map(|device| {
            let id = device.id().ok()?.to_string();
            let name = device
                .description()
                .map(|description| description.name().to_string())
                .unwrap_or_else(|_| device.to_string());
            Some(AudioOutputDeviceInfo {
                is_default: default_device_id.as_deref() == Some(id.as_str()),
                id,
                name,
            })
        })
        .collect::<Vec<_>>();
    devices.sort_by(|left, right| {
        right
            .is_default
            .cmp(&left.is_default)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.id.cmp(&right.id))
    });
    let preferred_is_available = preferred_device_id
        .is_some_and(|preferred| devices.iter().any(|device| device.id == preferred));
    let active_device_id = if preferred_is_available {
        preferred_device_id.map(str::to_owned)
    } else {
        default_device_id.or_else(|| devices.first().map(|device| device.id.clone()))
    };
    Ok(AudioOutputDevicesSnapshot {
        devices,
        active_device_id,
        // A remembered device may have been unplugged between launches. In
        // that case the effective active output is the current system default.
        follows_system_default: !preferred_is_available,
    })
}

pub fn validate_output_device(device_id: &str) -> Result<(), String> {
    let host = cpal::default_host();
    resolve_output_device(&host, Some(device_id)).map(|_| ())
}

fn resolve_output_device(
    host: &cpal::Host,
    preferred_device_id: Option<&str>,
) -> Result<Device, String> {
    let Some(preferred) = preferred_device_id else {
        return host
            .default_output_device()
            .ok_or_else(|| "no default output device".into());
    };
    let id = cpal::DeviceId::from_str(preferred)
        .map_err(|error| format!("output device identifier is invalid: {error}"))?;
    let device = host
        .device_by_id(&id)
        .ok_or_else(|| "the selected output device is unavailable".to_string())?;
    if !device.supports_output() {
        return Err("the selected device does not support audio output".into());
    }
    Ok(device)
}

fn build_f32_stream(
    device: &Device,
    config: StreamConfig,
    mut callback: AudioCallbackState,
    event_tx: Sender<EngineEvent>,
) -> Result<Stream, String> {
    device
        .build_output_stream(
            config,
            move |data: &mut [f32], _| callback.process_callback(data),
            stream_error_handler(event_tx),
            None,
        )
        .map_err(|error| format!("build output stream: {error}"))
}

fn build_converting_stream<T>(
    device: &Device,
    config: StreamConfig,
    mut callback: AudioCallbackState,
    event_tx: Sender<EngineEvent>,
) -> Result<Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    // Allocate before the stream starts. Resizing a Vec from inside the device
    // callback can miss a deadline and produce an audible underrun.
    let mut scratch = vec![0.0_f32; CONVERSION_SCRATCH_SAMPLES];
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                if data.len() > scratch.len() {
                    for output in data.iter_mut() {
                        *output = T::from_sample(0.0);
                    }
                    return;
                }
                let frame = &mut scratch[..data.len()];
                frame.fill(0.0);
                callback.process_callback(frame);
                for (output, sample) in data.iter_mut().zip(frame) {
                    *output = T::from_sample(*sample);
                }
            },
            stream_error_handler(event_tx),
            None,
        )
        .map_err(|error| format!("build output stream: {error}"))
}

fn stream_error_handler(event_tx: Sender<EngineEvent>) -> impl FnMut(cpal::Error) + Send + 'static {
    move |stream_error| {
        error!("cpal stream error: {stream_error}");
        let _ = event_tx.try_send(EngineEvent::Error {
            message: format!("the audio output stopped: {stream_error}"),
        });
    }
}

impl Drop for CpalOutput {
    fn drop(&mut self) {
        self.stream.take();
    }
}
