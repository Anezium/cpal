//! PS Vita output through a blocking SceAudioOut port on a dedicated worker.
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::traits::{DeviceTrait, HostTrait, StreamTrait};
use crate::{
    BackendSpecificError, BufferSize, BuildStreamError, Data, DefaultStreamConfigError,
    DeviceNameError, DevicesError, InputCallbackInfo, OutputCallbackInfo, OutputStreamTimestamp,
    PauseStreamError, PlayStreamError, SampleFormat, SampleRate, StreamConfig, StreamError,
    StreamInstant, SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
    SupportedStreamConfigsError,
};

const FRAMES: usize = 512;
const CHANNELS: u16 = 2;
const RATE: u32 = 48_000;
const SAMPLES: usize = FRAMES * CHANNELS as usize;
const BUFFER_SIZE: SupportedBufferSize = SupportedBufferSize::Range {
    min: FRAMES as u32,
    max: FRAMES as u32,
};

// Signatures and enum values from VitaSDK's psp2/audioout.h.
// No bindings or implementation code from other Vita ports is used.
#[link(name = "SceAudio_stub", kind = "static")]
extern "C" {
    fn sceAudioOutOpenPort(port_type: i32, len: i32, freq: i32, mode: i32) -> i32;
    fn sceAudioOutOutput(port: i32, buffer: *const std::ffi::c_void) -> i32;
    fn sceAudioOutReleasePort(port: i32) -> i32;
}
const SCE_AUDIO_OUT_PORT_TYPE_BGM: i32 = 1;
const SCE_AUDIO_OUT_MODE_STEREO: i32 = 1;

pub struct Host;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device;
pub type Devices = std::option::IntoIter<Device>;
pub type SupportedInputConfigs = std::iter::Empty<SupportedStreamConfigRange>;
pub type SupportedOutputConfigs = std::array::IntoIter<SupportedStreamConfigRange, 2>;

struct State {
    playing: AtomicBool,
    stop: AtomicBool,
    alive: AtomicBool,
}

pub struct Stream {
    state: Arc<State>,
    worker: Option<JoinHandle<()>>,
}

// Only the worker owns and uses the port. Cleanup also runs if a callback unwinds.
struct Port(i32);
impl Drop for Port {
    fn drop(&mut self) {
        unsafe {
            // NULL drains queued audio before releasing the port.
            sceAudioOutOutput(self.0, std::ptr::null());
            sceAudioOutReleasePort(self.0);
        }
    }
}
struct WorkerLifetime(Arc<State>);
impl Drop for WorkerLifetime {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::Release);
    }
}

fn backend_error(operation: &str, code: i32) -> BackendSpecificError {
    BackendSpecificError {
        description: format!("{} failed: 0x{:08x}", operation, code as u32),
    }
}

fn to_i16(sample: f32) -> i16 {
    if sample.is_finite() {
        // Rust's float-to-int cast saturates +1.0 to i16::MAX.
        (sample.clamp(-1.0, 1.0) * 32768.0) as i16
    } else {
        0
    }
}

impl Host {
    pub fn new() -> Result<Self, crate::HostUnavailable> {
        Ok(Self)
    }
}

impl HostTrait for Host {
    type Devices = Devices;
    type Device = Device;

    fn is_available() -> bool {
        true
    }

    fn devices(&self) -> Result<Devices, DevicesError> {
        Ok(Some(Device).into_iter())
    }

    fn default_input_device(&self) -> Option<Device> {
        None
    }

    fn default_output_device(&self) -> Option<Device> {
        Some(Device)
    }
}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    fn name(&self) -> Result<String, DeviceNameError> {
        Ok("PS Vita BGM output".to_owned())
    }

    fn supported_input_configs(
        &self,
    ) -> Result<SupportedInputConfigs, SupportedStreamConfigsError> {
        Ok(std::iter::empty())
    }

    fn supported_output_configs(
        &self,
    ) -> Result<SupportedOutputConfigs, SupportedStreamConfigsError> {
        Ok([SampleFormat::F32, SampleFormat::I16]
            .map(|sample_format| SupportedStreamConfigRange {
                channels: CHANNELS,
                min_sample_rate: SampleRate(RATE),
                max_sample_rate: SampleRate(RATE),
                buffer_size: BUFFER_SIZE,
                sample_format,
            })
            .into_iter())
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        Err(DefaultStreamConfigError::StreamTypeNotSupported)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        Ok(SupportedStreamConfig::new(
            CHANNELS,
            SampleRate(RATE),
            BUFFER_SIZE,
            SampleFormat::F32,
        ))
    }

    fn build_input_stream_raw<D, E>(
        &self,
        _config: &StreamConfig,
        _sample_format: SampleFormat,
        _data_callback: D,
        _error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Stream, BuildStreamError>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        Err(BuildStreamError::StreamConfigNotSupported)
    }

    fn build_output_stream_raw<D, E>(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
        mut data_callback: D,
        mut error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Stream, BuildStreamError>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        if config.channels != CHANNELS
            || config.sample_rate != SampleRate(RATE)
            || !matches!(sample_format, SampleFormat::F32 | SampleFormat::I16)
            || !matches!(
                config.buffer_size,
                BufferSize::Default | BufferSize::Fixed(512)
            )
        {
            return Err(BuildStreamError::StreamConfigNotSupported);
        }

        let state = Arc::new(State {
            playing: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(true),
        });
        let worker_state = state.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("cpal-vita-audio".to_owned())
            .stack_size(128 * 1024)
            .spawn(move || {
                let _lifetime = WorkerLifetime(worker_state.clone());
                let handle = unsafe {
                    sceAudioOutOpenPort(
                        SCE_AUDIO_OUT_PORT_TYPE_BGM,
                        FRAMES as i32,
                        RATE as i32,
                        SCE_AUDIO_OUT_MODE_STEREO,
                    )
                };
                if handle < 0 {
                    let _ = ready_tx.send(Err(backend_error("sceAudioOutOpenPort", handle)));
                    return;
                }
                let port = Port(handle);
                let mut pcm = vec![0i16; SAMPLES];
                let mut floats = vec![
                    0f32;
                    if sample_format == SampleFormat::F32 {
                        SAMPLES
                    } else {
                        0
                    }
                ];
                let origin = Instant::now();
                let block_duration = Duration::from_secs_f64(FRAMES as f64 / RATE as f64);
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }

                while !worker_state.stop.load(Ordering::Acquire) {
                    if !worker_state.playing.load(Ordering::Acquire) {
                        // unpark's token prevents a lost wakeup between the check and park.
                        thread::park();
                        continue;
                    }
                    let elapsed = origin.elapsed();
                    let callback =
                        StreamInstant::new(elapsed.as_secs() as i64, elapsed.subsec_nanos());
                    let info = OutputCallbackInfo {
                        timestamp: OutputStreamTimestamp {
                            callback,
                            // Estimate only: no hardware playback clock is exposed here.
                            playback: callback.add(block_duration).unwrap_or(callback),
                        },
                    };
                    if sample_format == SampleFormat::F32 {
                        floats.fill(0.0);
                        // The pointer, sample count and format describe the live, exclusive Vec.
                        let mut data = unsafe {
                            Data::from_parts(floats.as_mut_ptr().cast(), SAMPLES, SampleFormat::F32)
                        };
                        data_callback(&mut data, &info);
                        for (dst, src) in pcm.iter_mut().zip(&floats) {
                            *dst = to_i16(*src);
                        }
                    } else {
                        pcm.fill(0);
                        let mut data = unsafe {
                            Data::from_parts(pcm.as_mut_ptr().cast(), SAMPLES, SampleFormat::I16)
                        };
                        data_callback(&mut data, &info);
                    }
                    if worker_state.stop.load(Ordering::Acquire)
                        || !worker_state.playing.load(Ordering::Acquire)
                    {
                        continue;
                    }
                    // Blocking output is the clock. The buffer lives until this call returns.
                    let result = unsafe { sceAudioOutOutput(port.0, pcm.as_ptr().cast()) };
                    if result < 0 {
                        worker_state.alive.store(false, Ordering::Release);
                        error_callback(backend_error("sceAudioOutOutput", result).into());
                        break;
                    }
                }
            })
            .map_err(|err| BackendSpecificError {
                description: format!("could not spawn Vita audio worker: {}", err),
            })?;
        let stream = Stream {
            state,
            worker: Some(worker),
        };
        // Propagate port-open failure synchronously so rodio can use its normal fallback.
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(stream),
            Ok(Err(err)) => Err(err.into()),
            Err(err) => Err(BackendSpecificError {
                description: format!("Vita audio worker failed during startup: {}", err),
            }
            .into()),
        }
    }
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), PlayStreamError> {
        if !self.state.alive.load(Ordering::Acquire) {
            return Err(PlayStreamError::DeviceNotAvailable);
        }
        self.state.playing.store(true, Ordering::Release);
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
        Ok(())
    }

    fn pause(&self) -> Result<(), PauseStreamError> {
        if !self.state.alive.load(Ordering::Acquire) {
            return Err(PauseStreamError::DeviceNotAvailable);
        }
        self.state.playing.store(false, Ordering::Release);
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            // A callback may arrange to drop its own backend stream: never join ourselves.
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}
