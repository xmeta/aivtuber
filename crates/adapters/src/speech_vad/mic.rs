//! Microphone capture through cpal. Audio goes from the device callback into
//! a bounded in-process channel and nowhere else: no file, no socket.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Instant;

/// Mono samples at the device rate plus the moment the callback ran.
pub struct AudioChunk {
    pub arrived: Instant,
    pub mono: Vec<f32>,
}

pub struct Capture {
    // Dropping the stream stops capture.
    _stream: cpal::Stream,
    pub sample_rate_hz: u32,
    pub device_name: String,
    /// Chunks the callback could not queue (consumer too slow). Any change
    /// means the stream has a gap.
    pub dropped_chunks: Arc<AtomicU64>,
    /// Set by the stream error callback (device removed, backend failure).
    pub failed: Arc<AtomicBool>,
}

pub fn list_input_devices() -> Result<Vec<String>, String> {
    let host = cpal::default_host();
    let devices = host.input_devices().map_err(|e| e.to_string())?;
    Ok(devices
        .filter_map(|d| d.description().ok().map(|d| d.name().to_owned()))
        .collect())
}

/// Start capturing from the default input device, or the first device whose
/// name contains `name_contains` (case-insensitive).
pub fn start_capture(
    name_contains: Option<&str>,
    sender: SyncSender<AudioChunk>,
) -> Result<Capture, String> {
    let host = cpal::default_host();
    let device = match name_contains {
        None => host
            .default_input_device()
            .ok_or("no default input device")?,
        Some(needle) => {
            let needle = needle.to_lowercase();
            host.input_devices()
                .map_err(|e| e.to_string())?
                .find(|d| {
                    d.description()
                        .map(|d| d.name().to_lowercase().contains(&needle))
                        .unwrap_or(false)
                })
                .ok_or_else(|| format!("no input device matching {needle:?}"))?
        }
    };
    let device_name = device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "unknown".to_owned());
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let sample_rate_hz = supported.sample_rate();
    let channels = usize::from(supported.channels());
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let dropped = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicBool::new(false));
    let err_flag = failed.clone();
    let on_error = move |_error: cpal::StreamError| {
        err_flag.store(true, Ordering::Relaxed);
    };

    let stream = match format {
        SampleFormat::F32 => build::<f32>(
            &device,
            &config,
            channels,
            sender,
            dropped.clone(),
            on_error,
        ),
        SampleFormat::I16 => build::<i16>(
            &device,
            &config,
            channels,
            sender,
            dropped.clone(),
            on_error,
        ),
        SampleFormat::U16 => build::<u16>(
            &device,
            &config,
            channels,
            sender,
            dropped.clone(),
            on_error,
        ),
        SampleFormat::I32 => build::<i32>(
            &device,
            &config,
            channels,
            sender,
            dropped.clone(),
            on_error,
        ),
        other => return Err(format!("unsupported sample format {other:?}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(Capture {
        _stream: stream,
        sample_rate_hz,
        device_name,
        dropped_chunks: dropped,
        failed,
    })
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    sender: SyncSender<AudioChunk>,
    dropped: Arc<AtomicU64>,
    on_error: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _info: &cpal::InputCallbackInfo| {
                let arrived = Instant::now();
                let mono = data
                    .chunks_exact(channels.max(1))
                    .map(|frame| {
                        frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / frame.len() as f32
                    })
                    .collect();
                if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
                    sender.try_send(AudioChunk { arrived, mono })
                {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            },
            on_error,
            None,
        )
        .map_err(|e| e.to_string())
}
