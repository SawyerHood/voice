use std::{
    fmt,
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat, Stream, StreamConfig, StreamError,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

pub const AUDIO_LEVEL_EVENT: &str = "audio-level";
pub const AUDIO_INPUT_STREAM_ERROR_EVENT: &str = "voice://audio-input-stream-error";
const LEVEL_EVENT_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicrophoneInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedAudio {
    pub wav_bytes: Vec<u8>,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub duration_ms: u64,
    pub device_id: String,
    pub device_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioInputStreamErrorEvent {
    pub message: String,
}

struct RecordingControl {
    stop_tx: Sender<()>,
    join_handle: JoinHandle<()>,
    samples: Arc<Mutex<Vec<i16>>>,
    sample_rate_hz: u32,
    channels: u16,
    started_at: Instant,
    device_id: String,
    device_name: String,
}

#[derive(Debug)]
struct RecordingRuntime {
    sample_rate_hz: u32,
    channels: u16,
}

struct EnumeratedInputDevice {
    id: String,
    name: String,
    is_default: bool,
    sample_rate_hz: Option<u32>,
    channels: Option<u16>,
    device: Device,
}

pub struct AudioCaptureService {
    recording: Mutex<Option<RecordingControl>>,
    audio_level_bits: Arc<AtomicU32>,
}

impl fmt::Debug for AudioCaptureService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioCaptureService")
            .finish_non_exhaustive()
    }
}

impl Default for AudioCaptureService {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioCaptureService {
    pub fn new() -> Self {
        Self {
            recording: Mutex::new(None),
            audio_level_bits: Arc::new(AtomicU32::new(0.0_f32.to_bits())),
        }
    }

    pub fn list_microphones(&self) -> Result<Vec<MicrophoneInfo>, String> {
        let host = cpal::default_host();
        let devices = enumerate_input_devices(&host)?;

        Ok(devices
            .into_iter()
            .map(|device| MicrophoneInfo {
                id: device.id,
                name: device.name,
                is_default: device.is_default,
                sample_rate_hz: device.sample_rate_hz,
                channels: device.channels,
            })
            .collect())
    }

    pub fn start_recording(
        &self,
        app_handle: AppHandle,
        preferred_device_id: Option<&str>,
    ) -> Result<(), String> {
        let mut recording_guard = self
            .recording
            .lock()
            .map_err(|_| "Audio capture state lock is poisoned".to_string())?;

        if recording_guard.is_some() {
            return Err("Recording is already in progress".to_string());
        }

        let host = cpal::default_host();
        let devices = enumerate_input_devices(&host)?;

        if devices.is_empty() {
            return Err("No microphone input devices are available".to_string());
        }

        let selected_device = select_input_device(devices, preferred_device_id)?;
        let selected_device_id = selected_device.id.clone();
        let selected_device_name = selected_device.name.clone();

        self.audio_level_bits
            .store(0.0_f32.to_bits(), Ordering::Relaxed);

        let samples = Arc::new(Mutex::new(Vec::<i16>::new()));
        let worker_samples = Arc::clone(&samples);
        let worker_level_bits = Arc::clone(&self.audio_level_bits);
        let worker_app_handle = app_handle.clone();
        let worker_device_id = selected_device_id.clone();

        let (ready_tx, ready_rx) = mpsc::channel::<Result<RecordingRuntime, String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let mut join_handle = Some(thread::spawn(move || {
            recording_thread_main(
                worker_device_id,
                worker_samples,
                worker_level_bits,
                worker_app_handle,
                ready_tx,
                stop_rx,
            );
        }));

        let runtime = match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(runtime)) => runtime,
            Ok(Err(err)) => {
                if let Some(handle) = join_handle.take() {
                    let _ = handle.join();
                }
                return Err(err);
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = stop_tx.send(());
                if let Some(handle) = join_handle.take() {
                    let _ = handle.join();
                }
                return Err("Timed out while starting microphone stream".to_string());
            }
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(handle) = join_handle.take() {
                    let _ = handle.join();
                }
                return Err("Microphone stream failed to initialize".to_string());
            }
        };

        let join_handle =
            join_handle.ok_or_else(|| "Microphone worker was unavailable".to_string())?;

        let _ = app_handle.emit(AUDIO_LEVEL_EVENT, 0.0_f32);

        *recording_guard = Some(RecordingControl {
            stop_tx,
            join_handle,
            samples,
            sample_rate_hz: runtime.sample_rate_hz,
            channels: runtime.channels,
            started_at: Instant::now(),
            device_id: selected_device_id,
            device_name: selected_device_name,
        });

        Ok(())
    }

    pub fn stop_recording(&self, app_handle: AppHandle) -> Result<RecordedAudio, String> {
        let control = {
            let mut recording_guard = self
                .recording
                .lock()
                .map_err(|_| "Audio capture state lock is poisoned".to_string())?;
            recording_guard
                .take()
                .ok_or_else(|| "Recording is not in progress".to_string())?
        };

        let RecordingControl {
            stop_tx,
            join_handle,
            samples,
            sample_rate_hz,
            channels,
            started_at,
            device_id,
            device_name,
        } = control;

        let _ = stop_tx.send(());
        if join_handle.join().is_err() {
            return Err("Microphone capture thread panicked while stopping".to_string());
        }

        let buffered_samples = {
            let mut sample_guard = samples
                .lock()
                .map_err(|_| "Audio sample buffer lock is poisoned".to_string())?;
            std::mem::take(&mut *sample_guard)
        };

        self.audio_level_bits
            .store(0.0_f32.to_bits(), Ordering::Relaxed);
        let _ = app_handle.emit(AUDIO_LEVEL_EVENT, 0.0_f32);

        let mut duration_ms = started_at.elapsed().as_millis() as u64;
        if duration_ms == 0 && sample_rate_hz > 0 {
            duration_ms = (buffered_samples.len() as u64 * 1000) / u64::from(sample_rate_hz);
        }

        let wav_bytes = pcm16_to_wav_bytes(&buffered_samples, sample_rate_hz, channels)?;

        Ok(RecordedAudio {
            wav_bytes,
            sample_rate_hz,
            channels,
            duration_ms,
            device_id,
            device_name,
        })
    }

    pub fn abort_recording(&self, app_handle: AppHandle) -> Result<bool, String> {
        let control = {
            let mut recording_guard = self
                .recording
                .lock()
                .map_err(|_| "Audio capture state lock is poisoned".to_string())?;
            recording_guard.take()
        };

        let Some(RecordingControl {
            stop_tx,
            join_handle,
            ..
        }) = control
        else {
            return Ok(false);
        };

        let _ = stop_tx.send(());
        let join_target = join_handle.thread().id();
        if join_target == thread::current().id() {
            drop(join_handle);
        } else if join_handle.join().is_err() {
            return Err("Microphone capture thread panicked while aborting".to_string());
        }

        self.audio_level_bits
            .store(0.0_f32.to_bits(), Ordering::Relaxed);
        let _ = app_handle.emit(AUDIO_LEVEL_EVENT, 0.0_f32);

        Ok(true)
    }

    pub fn get_audio_level(&self) -> f32 {
        f32::from_bits(self.audio_level_bits.load(Ordering::Relaxed))
    }
}

fn recording_thread_main(
    selected_device_id: String,
    samples: Arc<Mutex<Vec<i16>>>,
    audio_level_bits: Arc<AtomicU32>,
    app_handle: AppHandle,
    ready_tx: Sender<Result<RecordingRuntime, String>>,
    stop_rx: Receiver<()>,
) {
    let startup_result = start_recording_worker(
        &selected_device_id,
        Arc::clone(&samples),
        Arc::clone(&audio_level_bits),
    );

    let (stream, runtime, stream_error_rx) = match startup_result {
        Ok(started) => started,
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };

    let _ = ready_tx.send(Ok(runtime));
    let loop_exit = run_recording_loop(&stop_rx, &stream_error_rx, || {
        let level = f32::from_bits(audio_level_bits.load(Ordering::Relaxed));
        let _ = app_handle.emit(AUDIO_LEVEL_EVENT, level);
    });

    drop(stream);
    audio_level_bits.store(0.0_f32.to_bits(), Ordering::Relaxed);
    let _ = app_handle.emit(AUDIO_LEVEL_EVENT, 0.0_f32);

    if let RecordingLoopExit::StreamError(message) = loop_exit {
        let payload = AudioInputStreamErrorEvent { message };
        let _ = app_handle.emit(AUDIO_INPUT_STREAM_ERROR_EVENT, payload);
    }
}

fn start_recording_worker(
    selected_device_id: &str,
    samples: Arc<Mutex<Vec<i16>>>,
    audio_level_bits: Arc<AtomicU32>,
) -> Result<(Stream, RecordingRuntime, Receiver<String>), String> {
    let host = cpal::default_host();
    let devices = enumerate_input_devices(&host)?;
    let selected_device = select_input_device(devices, Some(selected_device_id))?;

    let supported_config = selected_device
        .device
        .default_input_config()
        .map_err(|err| {
            format!(
                "Failed to read default input config for '{}': {err}",
                selected_device.name
            )
        })?;

    let stream_config: StreamConfig = supported_config.clone().into();
    let sample_format = supported_config.sample_format();
    let input_channels = usize::from(stream_config.channels);
    let sample_rate_hz = stream_config.sample_rate.0;

    if let Ok(mut sample_buffer) = samples.lock() {
        sample_buffer.clear();
        sample_buffer.reserve(usize::try_from(sample_rate_hz).unwrap_or(48_000) * 10);
    }

    let (stream_error_tx, stream_error_rx) = mpsc::channel::<String>();

    let stream = build_input_stream(
        &selected_device.device,
        &stream_config,
        sample_format,
        input_channels,
        samples,
        audio_level_bits,
        stream_error_tx,
    )?;

    stream
        .play()
        .map_err(|err| format!("Failed to start microphone stream: {err}"))?;

    Ok((
        stream,
        RecordingRuntime {
            sample_rate_hz,
            channels: 1,
        },
        stream_error_rx,
    ))
}

#[derive(Debug, PartialEq, Eq)]
enum RecordingLoopExit {
    StopRequested,
    StreamError(String),
}

fn run_recording_loop<F>(
    stop_rx: &Receiver<()>,
    stream_error_rx: &Receiver<String>,
    mut on_level_tick: F,
) -> RecordingLoopExit
where
    F: FnMut(),
{
    loop {
        match stream_error_rx.try_recv() {
            Ok(message) => return RecordingLoopExit::StreamError(message),
            Err(TryRecvError::Disconnected | TryRecvError::Empty) => {}
        }

        match stop_rx.recv_timeout(LEVEL_EVENT_INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                return RecordingLoopExit::StopRequested
            }
            Err(RecvTimeoutError::Timeout) => on_level_tick(),
        }
    }
}

fn report_stream_error(format_label: &str, stream_error_tx: &Sender<String>, err: StreamError) {
    let message = format!("Microphone stream error ({format_label}): {err}");
    let _ = stream_error_tx.send(message.clone());
    eprintln!("{message}");
}

fn enumerate_input_devices(host: &cpal::Host) -> Result<Vec<EnumeratedInputDevice>, String> {
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let mut devices = Vec::new();

    let input_devices = host
        .input_devices()
        .map_err(|err| format!("Failed to enumerate input devices: {err}"))?;

    for (index, device) in input_devices.enumerate() {
        let name = device
            .name()
            .unwrap_or_else(|_| format!("Microphone {}", index + 1));
        let id = format!("mic-{}-{}", index, slugify_device_name(&name));
        let is_default = default_name.as_ref() == Some(&name);
        let config = device.default_input_config().ok();

        devices.push(EnumeratedInputDevice {
            id,
            name,
            is_default,
            sample_rate_hz: config.as_ref().map(|c| c.sample_rate().0),
            channels: config.as_ref().map(|c| c.channels()),
            device,
        });
    }

    Ok(devices)
}

fn select_input_device(
    devices: Vec<EnumeratedInputDevice>,
    preferred_device_id: Option<&str>,
) -> Result<EnumeratedInputDevice, String> {
    if let Some(device_id) = preferred_device_id {
        return devices
            .into_iter()
            .find(|device| device.id == device_id)
            .ok_or_else(|| format!("No microphone found for id '{device_id}'"));
    }

    devices
        .into_iter()
        .min_by_key(|device| if device.is_default { 0 } else { 1 })
        .ok_or_else(|| "No microphone input devices are available".to_string())
}

fn slugify_device_name(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_dash = false;

    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }

    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed.to_string()
    }
}

fn build_input_stream(
    device: &Device,
    stream_config: &StreamConfig,
    sample_format: SampleFormat,
    input_channels: usize,
    samples: Arc<Mutex<Vec<i16>>>,
    audio_level_bits: Arc<AtomicU32>,
    stream_error_tx: Sender<String>,
) -> Result<Stream, String> {
    match sample_format {
        SampleFormat::F32 => {
            let samples = Arc::clone(&samples);
            let level_bits = Arc::clone(&audio_level_bits);
            let stream_error_tx = stream_error_tx.clone();
            device
                .build_input_stream(
                    stream_config,
                    move |data: &[f32], _| {
                        process_input_frames(
                            data,
                            input_channels,
                            |sample| sample,
                            &samples,
                            &level_bits,
                        );
                    },
                    move |err| {
                        report_stream_error("f32", &stream_error_tx, err);
                    },
                    None,
                )
                .map_err(|err| format!("Failed to build f32 input stream: {err}"))
        }
        SampleFormat::I16 => {
            let samples = Arc::clone(&samples);
            let level_bits = Arc::clone(&audio_level_bits);
            let stream_error_tx = stream_error_tx.clone();
            device
                .build_input_stream(
                    stream_config,
                    move |data: &[i16], _| {
                        process_input_frames(
                            data,
                            input_channels,
                            |sample| sample as f32 / i16::MAX as f32,
                            &samples,
                            &level_bits,
                        );
                    },
                    move |err| {
                        report_stream_error("i16", &stream_error_tx, err);
                    },
                    None,
                )
                .map_err(|err| format!("Failed to build i16 input stream: {err}"))
        }
        SampleFormat::U16 => {
            let samples = Arc::clone(&samples);
            let level_bits = Arc::clone(&audio_level_bits);
            let stream_error_tx = stream_error_tx.clone();
            device
                .build_input_stream(
                    stream_config,
                    move |data: &[u16], _| {
                        process_input_frames(
                            data,
                            input_channels,
                            |sample| (sample as f32 / u16::MAX as f32) * 2.0 - 1.0,
                            &samples,
                            &level_bits,
                        );
                    },
                    move |err| {
                        report_stream_error("u16", &stream_error_tx, err);
                    },
                    None,
                )
                .map_err(|err| format!("Failed to build u16 input stream: {err}"))
        }
        _ => Err(format!(
            "Unsupported microphone sample format: {sample_format:?}"
        )),
    }
}

fn process_input_frames<T, F>(
    data: &[T],
    channels: usize,
    to_f32: F,
    samples: &Arc<Mutex<Vec<i16>>>,
    audio_level_bits: &Arc<AtomicU32>,
) where
    T: Copy,
    F: Fn(T) -> f32,
{
    if channels == 0 {
        return;
    }

    let mut peak = 0.0_f32;
    let mut sum_squares = 0.0_f64;
    let mut frame_count = 0usize;

    if let Ok(mut sample_buffer) = samples.lock() {
        sample_buffer.reserve(data.len() / channels);

        for frame in data.chunks_exact(channels) {
            let mut mixed = 0.0_f32;
            for &sample in frame {
                mixed += to_f32(sample);
            }

            let normalized = (mixed / channels as f32).clamp(-1.0, 1.0);
            sample_buffer.push(float_to_pcm16(normalized));

            let abs = normalized.abs();
            if abs > peak {
                peak = abs;
            }
            sum_squares += f64::from(normalized) * f64::from(normalized);
            frame_count += 1;
        }
    } else {
        return;
    }

    let rms = if frame_count == 0 {
        0.0
    } else {
        (sum_squares / frame_count as f64).sqrt() as f32
    };
    let level = peak.max(rms);
    audio_level_bits.store(level.to_bits(), Ordering::Relaxed);
}

fn float_to_pcm16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    if clamped <= -1.0 {
        i16::MIN
    } else if clamped >= 1.0 {
        i16::MAX
    } else {
        (clamped * i16::MAX as f32).round() as i16
    }
}

fn pcm16_to_wav_bytes(
    samples: &[i16],
    sample_rate_hz: u32,
    channels: u16,
) -> Result<Vec<u8>, String> {
    let bytes_per_sample = 2u16;
    let block_align = channels
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| "WAV header block alignment overflow".to_string())?;
    let byte_rate = sample_rate_hz
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| "WAV header byte rate overflow".to_string())?;

    let sample_count_u32 = u32::try_from(samples.len())
        .map_err(|_| "Audio clip is too long to encode as standard WAV".to_string())?;
    let data_size = sample_count_u32
        .checked_mul(u32::from(bytes_per_sample))
        .ok_or_else(|| "WAV data size overflow".to_string())?;
    let riff_chunk_size = 36u32
        .checked_add(data_size)
        .ok_or_else(|| "WAV RIFF chunk overflow".to_string())?;

    let mut wav_bytes = Vec::with_capacity(44 + usize::try_from(data_size).unwrap_or(0));
    wav_bytes.extend_from_slice(b"RIFF");
    wav_bytes.extend_from_slice(&riff_chunk_size.to_le_bytes());
    wav_bytes.extend_from_slice(b"WAVE");
    wav_bytes.extend_from_slice(b"fmt ");
    wav_bytes.extend_from_slice(&16u32.to_le_bytes());
    wav_bytes.extend_from_slice(&1u16.to_le_bytes());
    wav_bytes.extend_from_slice(&channels.to_le_bytes());
    wav_bytes.extend_from_slice(&sample_rate_hz.to_le_bytes());
    wav_bytes.extend_from_slice(&byte_rate.to_le_bytes());
    wav_bytes.extend_from_slice(&block_align.to_le_bytes());
    wav_bytes.extend_from_slice(&16u16.to_le_bytes());
    wav_bytes.extend_from_slice(b"data");
    wav_bytes.extend_from_slice(&data_size.to_le_bytes());

    for sample in samples {
        wav_bytes.extend_from_slice(&sample.to_le_bytes());
    }

    Ok(wav_bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::{
        float_to_pcm16, pcm16_to_wav_bytes, run_recording_loop, slugify_device_name,
        RecordingLoopExit,
    };

    #[test]
    fn slugify_device_name_normalizes_ascii() {
        assert_eq!(
            slugify_device_name("MacBook Pro Microphone"),
            "macbook-pro-microphone"
        );
        assert_eq!(slugify_device_name("USB-C 🎤 Input"), "usb-c-input");
        assert_eq!(slugify_device_name("___"), "unnamed");
    }

    #[test]
    fn float_to_pcm16_clamps_and_scales() {
        assert_eq!(float_to_pcm16(-1.5), i16::MIN);
        assert_eq!(float_to_pcm16(-1.0), i16::MIN);
        assert_eq!(float_to_pcm16(0.0), 0);
        assert_eq!(float_to_pcm16(1.0), i16::MAX);
        assert_eq!(float_to_pcm16(1.5), i16::MAX);
    }

    #[test]
    fn pcm16_to_wav_bytes_writes_valid_header_and_data() {
        let samples = [0i16, 1024i16, -1024i16];
        let wav = pcm16_to_wav_bytes(&samples, 16_000, 1).expect("expected wav bytes");

        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");

        let riff_chunk_size = u32::from_le_bytes([wav[4], wav[5], wav[6], wav[7]]);
        let data_size = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]);

        assert_eq!(riff_chunk_size, 36 + data_size);
        assert_eq!(data_size, (samples.len() * 2) as u32);
        assert_eq!(wav.len(), 44 + samples.len() * 2);

        let encoded_first = i16::from_le_bytes([wav[44], wav[45]]);
        let encoded_second = i16::from_le_bytes([wav[46], wav[47]]);
        let encoded_third = i16::from_le_bytes([wav[48], wav[49]]);

        assert_eq!(encoded_first, samples[0]);
        assert_eq!(encoded_second, samples[1]);
        assert_eq!(encoded_third, samples[2]);
    }

    #[test]
    fn recording_loop_returns_stream_error_when_callback_reports_error() {
        let (_stop_tx, stop_rx) = mpsc::channel::<()>();
        let (stream_error_tx, stream_error_rx) = mpsc::channel::<String>();
        stream_error_tx
            .send("stream disconnected".to_string())
            .expect("stream error should send");

        let exit = run_recording_loop(&stop_rx, &stream_error_rx, || {});

        assert_eq!(
            exit,
            RecordingLoopExit::StreamError("stream disconnected".to_string())
        );
    }

    #[test]
    fn recording_loop_stops_when_stop_signal_received() {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (_stream_error_tx, stream_error_rx) = mpsc::channel::<String>();
        stop_tx.send(()).expect("stop signal should send");

        let exit = run_recording_loop(&stop_rx, &stream_error_rx, || {});

        assert_eq!(exit, RecordingLoopExit::StopRequested);
    }
}
