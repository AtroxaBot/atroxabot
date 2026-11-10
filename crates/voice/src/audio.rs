//! Audio devices: listing them, capturing the microphone and playing to the speakers.
//!
//! The device callbacks only move samples between the device and two bounded queues, under a `try_lock`
//! that is never held for more than a copy. Everything else happens on worker threads.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, Sample, SampleFormat, SizedSample, StreamConfig};

use crate::Error;

/// Captured audio that nobody reads is dropped beyond this many seconds.
const CAPTURE_SECONDS: usize = 2;
/// Speech waiting to be played is bounded to this many seconds; the speaker thread waits for room.
const PLAYBACK_SECONDS: usize = 30;

/// An audio input or output device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The device's name; pass it as [`CallOptions::input_device`](crate::CallOptions::input_device) or
    /// [`CallOptions::output_device`](crate::CallOptions::output_device).
    pub name: String,
    /// Is this the system's default device?
    pub default: bool,
}

/// The microphones of the system's audio host. Empty when they cannot be listed.
#[must_use]
pub fn input_devices() -> Vec<Device> {
    list(true)
}

/// The speakers of the system's audio host. Empty when they cannot be listed.
#[must_use]
pub fn output_devices() -> Vec<Device> {
    list(false)
}

fn all(host: &cpal::Host, input: bool) -> Result<impl Iterator<Item = cpal::Device>, cpal::Error> {
    if input { host.input_devices() } else { host.output_devices() }
}

fn default_device(host: &cpal::Host, input: bool) -> Option<cpal::Device> {
    if input { host.default_input_device() } else { host.default_output_device() }
}

fn name(device: &cpal::Device) -> Option<String> {
    Some(device.description().ok()?.name().to_owned())
}

fn list(input: bool) -> Vec<Device> {
    let host = cpal::default_host();
    // Compared by id: a default device handle follows the system default and equals no listed device.
    let default = default_device(&host, input).and_then(|device| device.id().ok());
    let Ok(devices) = all(&host, input) else { return Vec::new() };
    devices
        .filter_map(|device| {
            let id = device.id().ok();
            Some(Device { name: name(&device)?, default: id.is_some() && id == default })
        })
        .collect()
}

/// The device with this name, or the default one for `None`.
fn find(host: &cpal::Host, wanted: Option<&str>, input: bool) -> Result<cpal::Device, Error> {
    let kind = if input { "input" } else { "output" };
    let Some(wanted) = wanted else {
        return default_device(host, input).ok_or_else(|| Error::Device(format!("there is no {kind} device")));
    };
    all(host, input)
        .map_err(|error| device_error(&error))?
        .find(|device| name(device).as_deref() == Some(wanted))
        .ok_or_else(|| Error::Device(format!("the {kind} device \"{wanted}\" was not found")))
}

fn device_error(error: &cpal::Error) -> Error {
    Error::Device(error.to_string())
}

/// Locks a mutex; a poisoned one is used as it is, since no holder leaves it half-updated.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Speech on its way to the speakers.
pub(crate) struct Playback {
    /// Mono samples at the output device's rate.
    pub(crate) queue: VecDeque<i16>,
    /// Bumped when speech is dropped, so audio of a cancelled request is not queued any more.
    pub(crate) generation: u64,
}

/// What the device callbacks share with the worker threads.
pub(crate) struct Io {
    /// Mono microphone samples at the input device's rate.
    pub(crate) capture: Mutex<VecDeque<i16>>,
    pub(crate) playback: Mutex<Playback>,
    /// RMS of the buffer that was played last, as `f32` bits.
    output_level: AtomicU32,
}

impl Io {
    pub(crate) const fn new() -> Self {
        Self {
            capture: Mutex::new(VecDeque::new()),
            playback: Mutex::new(Playback { queue: VecDeque::new(), generation: 0 }),
            output_level: AtomicU32::new(0),
        }
    }

    /// RMS of what is playing right now, on the `i16` scale.
    pub(crate) fn output_level(&self) -> f32 {
        f32::from_bits(self.output_level.load(Ordering::Relaxed))
    }
}

/// The running device streams. Dropping it stops them; it must stay on the thread that opened it.
pub(crate) struct Streams {
    _input: cpal::Stream,
    _output: cpal::Stream,
    pub(crate) input_rate: u32,
    pub(crate) output_rate: u32,
}

/// Calls `$build::<T>(..)` with the Rust type of a device sample format.
macro_rules! with_sample_type {
    ($format:expr, $build:ident($($argument:expr),*)) => {
        match $format {
            SampleFormat::I8 => $build::<i8>($($argument),*),
            SampleFormat::I16 => $build::<i16>($($argument),*),
            SampleFormat::I24 => $build::<cpal::I24>($($argument),*),
            SampleFormat::I32 => $build::<i32>($($argument),*),
            SampleFormat::I64 => $build::<i64>($($argument),*),
            SampleFormat::U8 => $build::<u8>($($argument),*),
            SampleFormat::U16 => $build::<u16>($($argument),*),
            SampleFormat::U24 => $build::<cpal::U24>($($argument),*),
            SampleFormat::U32 => $build::<u32>($($argument),*),
            SampleFormat::U64 => $build::<u64>($($argument),*),
            SampleFormat::F32 => $build::<f32>($($argument),*),
            SampleFormat::F64 => $build::<f64>($($argument),*),
            other => Err(Error::Device(format!("the sample format {other} is not supported"))),
        }
    };
}

/// Opens and starts the microphone and the speakers in their native formats.
///
/// `on_lost` is called (from a device thread) when a stream stops working.
pub(crate) fn open(
    io: &Arc<Io>,
    input: Option<&str>,
    output: Option<&str>,
    on_lost: impl Fn(String) + Send + Clone + 'static,
) -> Result<Streams, Error> {
    let host = cpal::default_host();
    let input = find(&host, input, true)?;
    let output = find(&host, output, false)?;
    let input_config = input.default_input_config().map_err(|error| device_error(&error))?;
    let output_config = output.default_output_config().map_err(|error| device_error(&error))?;
    let (input_rate, output_rate) = (input_config.sample_rate(), output_config.sample_rate());
    // The callbacks never grow the queues: what is reserved here is their bound.
    lock(&io.capture).reserve_exact(input_rate as usize * CAPTURE_SECONDS);
    lock(&io.playback).queue.reserve_exact(output_rate as usize * PLAYBACK_SECONDS);

    let input_stream = with_sample_type!(
        input_config.sample_format(),
        input_stream(&input, input_config.config(), Arc::clone(io), error_callback(on_lost.clone()))
    )?;
    let output_stream = with_sample_type!(
        output_config.sample_format(),
        output_stream(&output, output_config.config(), Arc::clone(io), error_callback(on_lost))
    )?;
    input_stream.play().map_err(|error| device_error(&error))?;
    output_stream.play().map_err(|error| device_error(&error))?;
    Ok(Streams { _input: input_stream, _output: output_stream, input_rate, output_rate })
}

fn error_callback(on_lost: impl Fn(String) + Send + 'static) -> impl FnMut(cpal::Error) + Send + 'static {
    move |error| match error.kind() {
        // A glitch, a denied thread priority or a new route: the stream goes on.
        ErrorKind::Xrun | ErrorKind::RealtimeDenied | ErrorKind::DeviceChanged => {}
        _ => on_lost(error.to_string()),
    }
}

/// Microphone → mono `i16` in [`Io::capture`]. Samples that do not fit are dropped.
fn input_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    io: Arc<Io>,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, Error>
where
    T: SizedSample,
    i16: FromSample<T>,
{
    let channels = usize::from(config.channels).max(1);
    let callback = move |data: &[T], _: &cpal::InputCallbackInfo| {
        let Ok(mut queue) = io.capture.try_lock() else { return };
        for frame in data.chunks_exact(channels) {
            if queue.len() >= queue.capacity() {
                break;
            }
            let sum: i32 = frame.iter().map(|&sample| i32::from(i16::from_sample(sample))).sum();
            queue.push_back((sum / channels as i32) as i16);
        }
    };
    device.build_input_stream(config, callback, on_error, None).map_err(|error| device_error(&error))
}

/// [`Io::playback`] → every channel of the speakers; silence when there is nothing to play.
fn output_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    io: Arc<Io>,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, Error>
where
    T: SizedSample + FromSample<i16>,
{
    let channels = usize::from(config.channels).max(1);
    let callback = move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
        let mut playback = io.playback.try_lock().ok();
        let (mut energy, mut frames) = (0.0f32, 0u32);
        for frame in data.chunks_exact_mut(channels) {
            let sample = playback.as_mut().and_then(|playback| playback.queue.pop_front()).unwrap_or(0);
            frame.fill(T::from_sample(sample));
            energy += f32::from(sample) * f32::from(sample);
            frames += 1;
        }
        let level = if frames == 0 { 0.0 } else { (energy / frames as f32).sqrt() };
        io.output_level.store(level.to_bits(), Ordering::Relaxed);
    };
    device.build_output_stream(config, callback, on_error, None).map_err(|error| device_error(&error))
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;

    /// The values depend on the room (a muted microphone gives zeros); the sample count does not.
    #[test]
    #[ignore = "needs a microphone and speakers"]
    fn live_microphone_delivers_samples_at_its_rate() {
        let named = |devices: Vec<Device>| devices.into_iter().find(|device| device.default).map(|device| device.name);
        let (input, output) = (named(input_devices()), named(output_devices()));
        assert!(input.is_some() && output.is_some(), "no default devices");
        for (input, output) in [(None, None), (input.as_deref(), output.as_deref())] {
            let io = Arc::new(Io::new());
            let streams = open(&io, input, output, |reason| panic!("device lost: {reason}")).unwrap();
            thread::sleep(Duration::from_millis(600));
            let captured = lock(&io.capture).len();
            let expected = streams.input_rate as usize * 6 / 10;
            drop(streams);
            assert!(captured > expected / 2 && captured < expected * 3 / 2, "{captured} samples, expected {expected}");
        }
    }
}
