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
// VitaSDK audioout.h permits 44.1 kHz for BGM (MAIN requires 48 kHz).
// Match the application mixer and avoid its 44.1 -> 48 kHz conversion.
const RATE: u32 = 44_100;
// Lower numbers have higher priority. Skin workers use 159 on CPU1/CPU2.
// CPU1 only: audio may preempt worker 0, but never the CPU0 game thread.
const PRIORITY: i32 = 128;
const AFFINITY: i32 = 0x20000;
// Symphonia MP3 decoding runs in rodio's callback; reserve 1 MiB, not 128 KiB.
const STACK_BYTES: usize = 1024 * 1024;
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
    fn sceAudioOutSetVolume(port: i32, channels: i32, volumes: *const i32) -> i32;
}
#[link(name = "SceLibKernel_stub", kind = "static")]
extern "C" {
    fn sceKernelGetThreadId() -> i32;
    fn sceKernelChangeThreadPriority(id: i32, priority: i32) -> i32;
    fn sceKernelChangeThreadCpuAffinityMask(id: i32, mask: i32) -> i32;
    fn sceKernelGetThreadInfo(id: i32, info: *mut ThreadInfo) -> i32;
}
const SCE_AUDIO_OUT_PORT_TYPE_BGM: i32 = 1;
const SCE_AUDIO_OUT_MODE_STEREO: i32 = 1;

// VitaSDK SceKernelThreadInfo, 0x80-byte ARM layout; verify effective policy.
#[repr(C)]
struct ThreadInfo {
    size: u32, process: i32, name: [u8; 32], attr: u32, status: u32,
    entry: u32, stack: u32, stack_size: i32, init_priority: i32, priority: i32,
    init_affinity: i32, affinity: i32, cpu: i32, last_cpu: i32,
    wait_type: u32, wait_id: i32, exit_status: i32, run_clocks: u64,
    intr_preempt: u32, preempt: u32, release: u32, changes: i32,
    notify: i32, reserved: i32,
}
const _: [(); 0x80] = [(); std::mem::size_of::<ThreadInfo>()];

// Software callback+conversion timings, excluding blocking hardware output.
// Fixed buckets avoid allocation or sorting on the audio thread. Percentiles
// are upper bounds at 0.1 ms resolution; max is exact to the recorded us.
struct Deadlines {
    late_callbacks: u64,
    underrun_estimates: u64,
    histogram: [u32; 256],
    callbacks: u32,
    max_us: u64,
}
impl Default for Deadlines {
    fn default() -> Self { Self { late_callbacks: 0, underrun_estimates: 0,
        histogram: [0; 256], callbacks: 0, max_us: 0 } }
}
impl Deadlines {
    fn observe(&mut self, gap: Option<Duration>, callback: Duration, block: Duration) {
        self.late_callbacks += u64::from(gap.is_some_and(|g| g > block + Duration::from_millis(1)));
        self.underrun_estimates += u64::from(callback > block);
        let us = callback.as_micros() as u64;
        self.histogram[(us / 100).min(255) as usize] += 1;
        self.callbacks += 1;
        self.max_us = self.max_us.max(us);
    }
    fn percentile_us(&self, percent: u32) -> u64 {
        let wanted = (self.callbacks * percent).div_ceil(100);
        let mut total = 0;
        for (bucket, count) in self.histogram.iter().enumerate() {
            total += count;
            if total >= wanted { return if bucket == 255 { self.max_us } else { (bucket as u64 + 1) * 100 }; }
        }
        self.max_us
    }
}

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

fn time_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_millis()
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
            .stack_size(STACK_BYTES)
            .spawn(move || {
                let _lifetime = WorkerLifetime(worker_state.clone());
                let id = unsafe { sceKernelGetThreadId() };
                for (operation, result) in [
                    ("sceKernelChangeThreadCpuAffinityMask", unsafe { sceKernelChangeThreadCpuAffinityMask(id, AFFINITY) }),
                    ("sceKernelChangeThreadPriority", unsafe { sceKernelChangeThreadPriority(id, PRIORITY) }),
                ] {
                    if result < 0 {
                        let _ = ready_tx.send(Err(backend_error(operation, result)));
                        return;
                    }
                }
                let mut info: ThreadInfo = unsafe { std::mem::zeroed() };
                info.size = std::mem::size_of::<ThreadInfo>() as u32;
                let info_rc = unsafe { sceKernelGetThreadInfo(id, &mut info) };
                if info_rc < 0 || info.priority != PRIORITY || info.affinity != AFFINITY
                    || info.stack_size < STACK_BYTES as i32 {
                    let _ = ready_tx.send(Err(backend_error(&format!("audio thread policy readback priority={} affinity={:#x} stack={}", info.priority, info.affinity, info.stack_size), info_rc)));
                    return;
                }
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
                let volumes = [32768i32; 2];
                let volume_rc = unsafe { sceAudioOutSetVolume(port.0, 3, volumes.as_ptr()) };
                if volume_rc < 0 {
                    let _ = ready_tx.send(Err(backend_error("sceAudioOutSetVolume", volume_rc)));
                    return;
                }
                eprintln!("[cpal/vita] time_ms={} port=BGM rate={} frames={} block_ms={:.3} priority={} affinity={:#x} stack_bytes={} volume=32768 thread_id={} info_rc={} effective_priority={} effective_affinity={:#x} effective_stack_bytes={}",
                    time_ms(), RATE, FRAMES, FRAMES as f64 * 1000.0 / RATE as f64,
                    PRIORITY, AFFINITY, STACK_BYTES, id, info_rc,
                    info.priority, info.affinity, info.stack_size);
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

                let mut previous_callback = None;
                let mut report_at = Instant::now();
                let mut deadlines = Deadlines::default();
                while !worker_state.stop.load(Ordering::Acquire) {
                    if !worker_state.playing.load(Ordering::Acquire) {
                        // unpark's token prevents a lost wakeup between the check and park.
                        previous_callback = None;
                        thread::park();
                        continue;
                    }
                    let started = Instant::now();
                    let gap = previous_callback.map(|previous| started.duration_since(previous));
                    previous_callback = Some(started);
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
                    deadlines.observe(gap, started.elapsed(), block_duration);
                    if report_at.elapsed() >= Duration::from_secs(1) {
                        eprintln!("[cpal/vita] time_ms={} late_callbacks={} underrun_estimates={} window_ms={} elapsed_ms={} callbacks={} callback_p50_us={} callback_p95_us={} callback_max_us={} percentile_resolution_us=100",
                            time_ms(), deadlines.late_callbacks, deadlines.underrun_estimates,
                            report_at.elapsed().as_millis(), origin.elapsed().as_millis(), deadlines.callbacks,
                            deadlines.percentile_us(50), deadlines.percentile_us(95), deadlines.max_us);
                        deadlines = Deadlines::default();
                        report_at = Instant::now();
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_percentiles_include_silence_and_overflow_max() {
        let mut d = Deadlines::default();
        for us in [20, 90, 101, 250, 12000, 90000] {
            d.observe(None, Duration::from_micros(us), Duration::from_micros(11610));
        }
        assert_eq!(d.percentile_us(50), 200);
        assert_eq!(d.percentile_us(95), 90000);
        assert_eq!(d.max_us, 90000);
        assert_eq!(d.callbacks, 6);
        assert_eq!(d.underrun_estimates, 2);
    }
    #[test]
    fn deadlines_count_only_late_work_and_ignore_first_callback() {
        let block = Duration::from_secs_f64(FRAMES as f64 / RATE as f64);
        let mut d = Deadlines::default();
        d.observe(None, Duration::from_millis(2), block);
        d.observe(Some(block), Duration::from_millis(3), block);
        assert_eq!((d.late_callbacks, d.underrun_estimates), (0, 0));
        d.observe(Some(block + Duration::from_millis(2)), block + Duration::from_nanos(1), block);
        assert_eq!((d.late_callbacks, d.underrun_estimates), (1, 1));
    }
    #[test]
    fn pcm_conversion_clamps_and_silences_non_finite_samples() {
        assert_eq!(to_i16(1.0), i16::MAX);
        assert_eq!(to_i16(-1.0), i16::MIN);
        assert_eq!(to_i16(2.0), i16::MAX);
        assert_eq!(to_i16(f32::NAN), 0);
        assert_eq!(to_i16(f32::INFINITY), 0);
    }
}
