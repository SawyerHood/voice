use std::{
    collections::{HashMap, VecDeque},
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
use tracing::{debug, error, info, warn};

pub const AUDIO_LEVEL_EVENT: &str = "audio-level";
pub const AUDIO_INPUT_STREAM_ERROR_EVENT: &str = "voice://audio-input-stream-error";
const LEVEL_EVENT_INTERVAL: Duration = Duration::from_millis(50);
const WORKER_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

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

#[derive(Debug, Clone)]
pub struct AudioInputChunk {
    pub pcm16_mono_samples: Vec<i16>,
    pub sample_rate_hz: u32,
}

pub type AudioInputChunkCallback = Arc<dyn Fn(AudioInputChunk) + Send + Sync + 'static>;

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
    device_id: String,
    device_name: String,
}

struct EnumeratedInputDevice {
    id: String,
    name: String,
    is_default: bool,
    sample_rate_hz: Option<u32>,
    channels: Option<u16>,
    device: Device,
}

#[derive(Debug, Clone)]
struct MacosCoreAudioDeviceIdentity {
    device_id: u32,
    uid: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Clone)]
struct InputDeviceSelectionCandidate {
    id: String,
    name: String,
    is_default: bool,
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
        debug!("audio capture service initialized");
        Self {
            recording: Mutex::new(None),
            audio_level_bits: Arc::new(AtomicU32::new(0.0_f32.to_bits())),
        }
    }

    pub fn list_microphones(&self) -> Result<Vec<MicrophoneInfo>, String> {
        let host = cpal::default_host();
        let devices = enumerate_input_devices(&host)?;
        debug!(count = devices.len(), "enumerated input microphones");

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
        on_input_chunk: Option<AudioInputChunkCallback>,
    ) -> Result<(), String> {
        info!(
            preferred_device_id = ?preferred_device_id,
            "audio capture start requested"
        );
        let mut recording_guard = self
            .recording
            .lock()
            .map_err(|_| "Audio capture state lock is poisoned".to_string())?;

        if recording_guard.is_some() {
            warn!("recording start requested while already recording");
            return Err("Recording is already in progress".to_string());
        }

        self.audio_level_bits
            .store(0.0_f32.to_bits(), Ordering::Relaxed);

        let samples = Arc::new(Mutex::new(Vec::<i16>::new()));
        let worker_samples = Arc::clone(&samples);
        let worker_level_bits = Arc::clone(&self.audio_level_bits);
        let worker_app_handle = app_handle.clone();
        let worker_preferred_device_id = preferred_device_id.map(str::to_string);
        let worker_chunk_callback = on_input_chunk;

        let (ready_tx, ready_rx) = mpsc::channel::<Result<RecordingRuntime, String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let mut join_handle = Some(thread::spawn(move || {
            recording_thread_main(
                worker_preferred_device_id,
                worker_samples,
                worker_level_bits,
                worker_app_handle,
                worker_chunk_callback,
                ready_tx,
                stop_rx,
            );
        }));

        let runtime = await_worker_startup(
            &ready_rx,
            &stop_tx,
            &mut join_handle,
            WORKER_STARTUP_TIMEOUT,
        )?;

        let join_handle =
            join_handle.ok_or_else(|| "Microphone worker was unavailable".to_string())?;

        if let Err(error) = app_handle.emit(AUDIO_LEVEL_EVENT, 0.0_f32) {
            warn!(%error, "failed to emit initial audio level event");
        }

        *recording_guard = Some(RecordingControl {
            stop_tx,
            join_handle,
            samples,
            sample_rate_hz: runtime.sample_rate_hz,
            channels: runtime.channels,
            started_at: Instant::now(),
            device_id: runtime.device_id,
            device_name: runtime.device_name,
        });

        info!("audio capture started");
        Ok(())
    }

    pub fn stop_recording(&self, app_handle: AppHandle) -> Result<RecordedAudio, String> {
        info!("audio capture stop requested");
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
            error!("microphone capture thread panicked while stopping");
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
        if let Err(error) = app_handle.emit(AUDIO_LEVEL_EVENT, 0.0_f32) {
            warn!(%error, "failed to emit audio level reset event after stop");
        }

        let mut duration_ms = started_at.elapsed().as_millis() as u64;
        if duration_ms == 0 && sample_rate_hz > 0 {
            duration_ms = (buffered_samples.len() as u64 * 1000) / u64::from(sample_rate_hz);
        }

        let wav_bytes = pcm16_to_wav_bytes(&buffered_samples, sample_rate_hz, channels)?;
        info!(
            duration_ms,
            sample_rate_hz,
            channels,
            sample_count = buffered_samples.len(),
            device_id = %device_id,
            device_name = %device_name,
            "audio capture stopped"
        );

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
        warn!("aborting active audio capture");
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
            debug!("abort requested but no active recording existed");
            return Ok(false);
        };

        let _ = stop_tx.send(());
        let join_target = join_handle.thread().id();
        if join_target == thread::current().id() {
            drop(join_handle);
        } else if join_handle.join().is_err() {
            error!("microphone capture thread panicked while aborting");
            return Err("Microphone capture thread panicked while aborting".to_string());
        }

        self.audio_level_bits
            .store(0.0_f32.to_bits(), Ordering::Relaxed);
        if let Err(error) = app_handle.emit(AUDIO_LEVEL_EVENT, 0.0_f32) {
            warn!(%error, "failed to emit audio level reset event after abort");
        }

        info!("audio capture aborted");
        Ok(true)
    }

    pub fn get_audio_level(&self) -> f32 {
        f32::from_bits(self.audio_level_bits.load(Ordering::Relaxed))
    }
}

fn await_worker_startup(
    ready_rx: &Receiver<Result<RecordingRuntime, String>>,
    stop_tx: &Sender<()>,
    join_handle: &mut Option<JoinHandle<()>>,
    timeout: Duration,
) -> Result<RecordingRuntime, String> {
    match ready_rx.recv_timeout(timeout) {
        Ok(Ok(runtime)) => Ok(runtime),
        Ok(Err(err)) => {
            if let Some(handle) = join_handle.take() {
                let _ = handle.join();
            }
            error!(error = %err, "microphone worker failed to initialize");
            Err(err)
        }
        Err(RecvTimeoutError::Timeout) => {
            let _ = stop_tx.send(());
            if join_handle.take().is_some() {
                warn!("detaching microphone worker after startup timeout");
            }
            error!("microphone worker timed out while starting");
            Err("Timed out while starting microphone stream".to_string())
        }
        Err(RecvTimeoutError::Disconnected) => {
            if let Some(handle) = join_handle.take() {
                let _ = handle.join();
            }
            error!("microphone worker disconnected during startup");
            Err("Microphone stream failed to initialize".to_string())
        }
    }
}

fn recording_thread_main(
    preferred_device_id: Option<String>,
    samples: Arc<Mutex<Vec<i16>>>,
    audio_level_bits: Arc<AtomicU32>,
    app_handle: AppHandle,
    on_input_chunk: Option<AudioInputChunkCallback>,
    ready_tx: Sender<Result<RecordingRuntime, String>>,
    stop_rx: Receiver<()>,
) {
    debug!(
        preferred_device_id = ?preferred_device_id.as_deref(),
        "microphone worker thread started"
    );
    let startup_result = start_recording_worker(
        preferred_device_id.as_deref(),
        Arc::clone(&samples),
        Arc::clone(&audio_level_bits),
        on_input_chunk,
    );

    let (stream, runtime, stream_error_rx) = match startup_result {
        Ok(started) => started,
        Err(err) => {
            error!(
                preferred_device_id = ?preferred_device_id.as_deref(),
                error = %err,
                "microphone worker startup failed"
            );
            let _ = ready_tx.send(Err(err));
            return;
        }
    };

    let _ = ready_tx.send(Ok(runtime));
    let mut last_emitted_level: Option<f32> = None;
    let loop_exit = run_recording_loop(&stop_rx, &stream_error_rx, || {
        let level =
            quantize_audio_level_for_emit(f32::from_bits(audio_level_bits.load(Ordering::Relaxed)));
        if last_emitted_level.is_some_and(|last| (last - level).abs() < f32::EPSILON) {
            return;
        }
        last_emitted_level = Some(level);
        let _ = app_handle.emit(AUDIO_LEVEL_EVENT, level);
    });

    drop(stream);
    audio_level_bits.store(0.0_f32.to_bits(), Ordering::Relaxed);
    if let Err(error) = app_handle.emit(AUDIO_LEVEL_EVENT, 0.0_f32) {
        warn!(%error, "failed to emit audio level reset from worker thread");
    }

    if let RecordingLoopExit::StreamError(message) = loop_exit {
        error!(message = %message, "microphone worker exited due to stream error");
        let payload = AudioInputStreamErrorEvent { message };
        if let Err(error) = app_handle.emit(AUDIO_INPUT_STREAM_ERROR_EVENT, payload) {
            warn!(%error, "failed to emit audio stream error event");
        }
    } else {
        debug!("microphone worker exited after stop request");
    }
}

fn start_recording_worker(
    preferred_device_id: Option<&str>,
    samples: Arc<Mutex<Vec<i16>>>,
    audio_level_bits: Arc<AtomicU32>,
    on_input_chunk: Option<AudioInputChunkCallback>,
) -> Result<(Stream, RecordingRuntime, Receiver<String>), String> {
    let host = cpal::default_host();
    let default_input_device_name = host.default_input_device().and_then(|d| d.name().ok());
    let devices = enumerate_input_devices(&host)?;
    if devices.is_empty() {
        return Err("No microphone input devices are available".to_string());
    }

    let selected_device = select_input_device(
        devices,
        preferred_device_id,
        default_input_device_name.as_deref(),
    )?;
    info!(
        device_id = %selected_device.id,
        device_name = %selected_device.name,
        "starting recording worker for selected device"
    );

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
        sample_rate_hz,
        samples,
        audio_level_bits,
        on_input_chunk,
        stream_error_tx,
    )?;

    stream
        .play()
        .map_err(|err| format!("Failed to start microphone stream: {err}"))?;
    info!(
        sample_rate_hz,
        channels = stream_config.channels,
        "microphone stream playback started"
    );

    Ok((
        stream,
        RecordingRuntime {
            sample_rate_hz,
            channels: 1,
            device_id: selected_device.id,
            device_name: selected_device.name,
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
    debug!("entering recording loop");
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
    error!(%message, "microphone stream callback error");
}

fn enumerate_input_devices(host: &cpal::Host) -> Result<Vec<EnumeratedInputDevice>, String> {
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let default_coreaudio_device_id = macos_default_input_device_id();
    let mut macos_identities_by_name =
        build_macos_identity_lookup_by_name(macos_coreaudio_device_identities_in_global_order());
    let mut devices = Vec::new();
    let mut id_occurrences: HashMap<String, usize> = HashMap::new();

    let all_devices = host
        .devices()
        .map_err(|err| format!("Failed to enumerate input devices: {err}"))?;

    for device in all_devices {
        let supports_input = device
            .supported_input_configs()
            .map(|mut configs| configs.next().is_some())
            .unwrap_or(false);
        if !supports_input {
            continue;
        }

        let name = device
            .name()
            .unwrap_or_else(|_| format!("Microphone {}", devices.len() + 1));
        let coreaudio_identity =
            take_macos_identity_by_device_name(&mut macos_identities_by_name, &name);
        let coreaudio_device_id = coreaudio_identity
            .as_ref()
            .map(|identity| identity.device_id);
        let id = ensure_unique_device_id(
            build_microphone_device_id(
                &name,
                coreaudio_identity
                    .as_ref()
                    .and_then(|identity| identity.uid.as_deref()),
                coreaudio_device_id,
            ),
            &mut id_occurrences,
        );
        let is_default = match (default_coreaudio_device_id, coreaudio_device_id) {
            (Some(default_id), Some(device_id)) => default_id == device_id,
            _ => default_name.as_ref() == Some(&name),
        };
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

    debug!(count = devices.len(), "input device enumeration complete");
    Ok(devices)
}

fn select_input_device(
    mut devices: Vec<EnumeratedInputDevice>,
    preferred_device_id: Option<&str>,
    default_device_name: Option<&str>,
) -> Result<EnumeratedInputDevice, String> {
    let candidates = devices
        .iter()
        .map(|device| InputDeviceSelectionCandidate {
            id: device.id.clone(),
            name: device.name.clone(),
            is_default: device.is_default,
        })
        .collect::<Vec<_>>();
    let selected_index =
        select_input_device_index(&candidates, preferred_device_id, default_device_name)?;
    let selected = devices.swap_remove(selected_index);
    debug!(
        device_id = %selected.id,
        device_name = %selected.name,
        is_default = selected.is_default,
        "selected microphone device"
    );
    Ok(selected)
}

fn select_input_device_index(
    devices: &[InputDeviceSelectionCandidate],
    preferred_device_id: Option<&str>,
    default_device_name: Option<&str>,
) -> Result<usize, String> {
    if let Some(device_id) = preferred_device_id {
        if let Some(index) = devices.iter().position(|device| device.id == device_id) {
            debug!(device_id, "selected preferred input device by id");
            return Ok(index);
        }

        if let Some(legacy_slug) = legacy_device_slug(device_id) {
            if let Some(index) = devices
                .iter()
                .position(|device| slugify_device_name(&device.name) == legacy_slug)
            {
                warn!(
                    device_id,
                    "resolved microphone using legacy device id fallback"
                );
                return Ok(index);
            }
        }

        let default_index = resolve_default_input_device_index(devices, default_device_name)
            .ok_or_else(|| "No microphone input devices are available".to_string())?;
        let fallback_device = &devices[default_index];
        warn!(
            preferred_device_id = device_id,
            fallback_device_id = %fallback_device.id,
            fallback_device_name = %fallback_device.name,
            "preferred microphone was unavailable; falling back to system default input device"
        );
        return Ok(default_index);
    }

    resolve_default_input_device_index(devices, default_device_name)
        .ok_or_else(|| "No microphone input devices are available".to_string())
}

fn resolve_default_input_device_index(
    devices: &[InputDeviceSelectionCandidate],
    default_device_name: Option<&str>,
) -> Option<usize> {
    if let Some(default_name) = default_device_name {
        if let Some((index, _)) = devices
            .iter()
            .enumerate()
            .filter(|(_, device)| device.name == default_name)
            .min_by_key(|(_, device)| if device.is_default { 0 } else { 1 })
        {
            return Some(index);
        }

        warn!(
            default_device_name = default_name,
            "cpal default input device name was not found in enumerated microphones; using fallback selection"
        );
    }

    devices
        .iter()
        .enumerate()
        .min_by_key(|(_, device)| if device.is_default { 0 } else { 1 })
        .map(|(index, _)| index)
}

fn build_macos_identity_lookup_by_name(
    identities: Vec<MacosCoreAudioDeviceIdentity>,
) -> HashMap<String, VecDeque<MacosCoreAudioDeviceIdentity>> {
    let mut identities_by_name: HashMap<String, VecDeque<MacosCoreAudioDeviceIdentity>> =
        HashMap::new();
    for identity in identities {
        let Some(name_key) = normalized_device_name_lookup_key(identity.name.as_deref()) else {
            continue;
        };
        identities_by_name
            .entry(name_key)
            .or_default()
            .push_back(identity);
    }
    identities_by_name
}

fn take_macos_identity_by_device_name(
    identities_by_name: &mut HashMap<String, VecDeque<MacosCoreAudioDeviceIdentity>>,
    device_name: &str,
) -> Option<MacosCoreAudioDeviceIdentity> {
    let lookup_key = normalized_device_name_lookup_key(Some(device_name))?;
    let identities = identities_by_name.get_mut(&lookup_key)?;
    let identity = identities.pop_front();
    if identities.is_empty() {
        identities_by_name.remove(&lookup_key);
    }
    identity
}

fn normalized_device_name_lookup_key(name: Option<&str>) -> Option<String> {
    let normalized = name?.trim();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized.to_lowercase())
    }
}

fn build_microphone_device_id(
    name: &str,
    coreaudio_uid: Option<&str>,
    coreaudio_device_id: Option<u32>,
) -> String {
    if let Some(uid) = coreaudio_uid.map(str::trim).filter(|uid| !uid.is_empty()) {
        return format!("coreaudio:{uid}");
    }

    if let Some(device_id) = coreaudio_device_id {
        return format!("coreaudio-device-id:{device_id}");
    }

    format!("name:{}", slugify_device_name(name))
}

fn ensure_unique_device_id(
    candidate_id: String,
    id_occurrences: &mut HashMap<String, usize>,
) -> String {
    let entry = id_occurrences.entry(candidate_id.clone()).or_insert(0);
    *entry += 1;

    if *entry == 1 {
        candidate_id
    } else {
        format!("{candidate_id}#{}", *entry)
    }
}

fn legacy_device_slug(device_id: &str) -> Option<&str> {
    if let Some(slug) = device_id.strip_prefix("name:") {
        return (!slug.is_empty()).then_some(slug);
    }

    if let Some(slug) = device_id.strip_prefix("mic-name-") {
        return (!slug.is_empty()).then_some(slug);
    }

    let legacy = device_id.strip_prefix("mic-")?;
    let (index, slug) = legacy.split_once('-')?;
    if !index.is_empty() && index.chars().all(|ch| ch.is_ascii_digit()) && !slug.is_empty() {
        Some(slug)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn macos_coreaudio_device_identities_in_global_order() -> Vec<MacosCoreAudioDeviceIdentity> {
    match macos_collect_coreaudio_device_identities_in_global_order() {
        Ok(identities) => identities,
        Err(error) => {
            warn!(%error, "failed to collect CoreAudio device identifiers");
            Vec::new()
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_coreaudio_device_identities_in_global_order() -> Vec<MacosCoreAudioDeviceIdentity> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn macos_default_input_device_id() -> Option<u32> {
    match macos_collect_default_input_device_id() {
        Ok(device_id) => device_id,
        Err(error) => {
            warn!(%error, "failed to resolve CoreAudio default input device");
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_default_input_device_id() -> Option<u32> {
    None
}

#[cfg(target_os = "macos")]
fn macos_collect_coreaudio_device_identities_in_global_order(
) -> Result<Vec<MacosCoreAudioDeviceIdentity>, String> {
    use core_foundation_sys::string::{
        kCFStringEncodingUTF8, CFStringGetCString, CFStringGetCStringPtr, CFStringRef,
    };
    use coreaudio::sys::{
        kAudioDevicePropertyDeviceUID, kAudioDevicePropertyStreams, kAudioHardwareNoError,
        kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMaster, kAudioObjectPropertyName,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectSystemObject,
        AudioDeviceID, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
        AudioObjectPropertyAddress,
    };
    use std::{ffi::CStr, mem, os::raw::c_char, ptr::null};

    fn device_ids_global_order() -> Result<Vec<AudioDeviceID>, String> {
        let property_address = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMaster,
        };

        let mut data_size = 0u32;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(
                kAudioObjectSystemObject,
                &property_address as *const _,
                0,
                null(),
                &mut data_size as *mut _,
            )
        };
        if status != kAudioHardwareNoError as i32 {
            return Err(format!(
                "AudioObjectGetPropertyDataSize(kAudioHardwarePropertyDevices) failed with status {status}"
            ));
        }

        let device_count = data_size as usize / mem::size_of::<AudioDeviceID>();
        let mut device_ids = vec![0 as AudioDeviceID; device_count];

        let status = unsafe {
            AudioObjectGetPropertyData(
                kAudioObjectSystemObject,
                &property_address as *const _,
                0,
                null(),
                &mut data_size as *mut _,
                device_ids.as_mut_ptr() as *mut _,
            )
        };
        if status != kAudioHardwareNoError as i32 {
            return Err(format!(
                "AudioObjectGetPropertyData(kAudioHardwarePropertyDevices) failed with status {status}"
            ));
        }

        Ok(device_ids)
    }

    fn read_device_string_property(
        device_id: AudioDeviceID,
        selector: u32,
        scope: u32,
    ) -> Option<String> {
        let property_address = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMaster,
        };
        let mut cf_string: CFStringRef = null();
        let mut data_size = mem::size_of::<CFStringRef>() as u32;

        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &property_address as *const _,
                0,
                null(),
                &mut data_size as *mut _,
                &mut cf_string as *mut _ as *mut _,
            )
        };
        if status != kAudioHardwareNoError as i32 || cf_string.is_null() {
            return None;
        }

        let c_string_ptr = unsafe { CFStringGetCStringPtr(cf_string, kCFStringEncodingUTF8) };
        if !c_string_ptr.is_null() {
            let value = unsafe { CStr::from_ptr(c_string_ptr as *const c_char) };
            return Some(value.to_string_lossy().into_owned());
        }

        let mut buffer = [0i8; 512];
        let copied = unsafe {
            CFStringGetCString(
                cf_string,
                buffer.as_mut_ptr(),
                buffer.len() as isize,
                kCFStringEncodingUTF8,
            )
        };
        if copied == 0 {
            return None;
        }

        let value = unsafe { CStr::from_ptr(buffer.as_ptr()) };
        Some(value.to_string_lossy().into_owned())
    }

    fn device_supports_input(device_id: AudioDeviceID) -> bool {
        let property_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyStreams,
            mScope: kAudioObjectPropertyScopeInput,
            mElement: kAudioObjectPropertyElementMaster,
        };
        let mut data_size = 0u32;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(
                device_id,
                &property_address as *const _,
                0,
                null(),
                &mut data_size as *mut _,
            )
        };

        status == kAudioHardwareNoError as i32 && data_size > 0
    }

    let mut identities = Vec::new();
    for device_id in device_ids_global_order()? {
        if !device_supports_input(device_id) {
            continue;
        }

        let uid = read_device_string_property(
            device_id,
            kAudioDevicePropertyDeviceUID,
            kAudioObjectPropertyScopeGlobal,
        )
        .map(|uid| uid.trim().to_string())
        .filter(|uid| !uid.is_empty());
        let name = read_device_string_property(
            device_id,
            kAudioObjectPropertyName,
            kAudioObjectPropertyScopeGlobal,
        )
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());

        identities.push(MacosCoreAudioDeviceIdentity {
            device_id: device_id as u32,
            uid,
            name,
        });
    }

    Ok(identities)
}

#[cfg(target_os = "macos")]
fn macos_collect_default_input_device_id() -> Result<Option<u32>, String> {
    use coreaudio::sys::{
        kAudioHardwareNoError, kAudioHardwarePropertyDefaultInputDevice,
        kAudioObjectPropertyElementMaster, kAudioObjectPropertyScopeGlobal,
        kAudioObjectSystemObject, AudioDeviceID, AudioObjectGetPropertyData,
        AudioObjectPropertyAddress,
    };
    use std::{mem, ptr::null};

    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultInputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };
    let mut device_id: AudioDeviceID = 0 as AudioDeviceID;
    let mut data_size = mem::size_of::<AudioDeviceID>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &property_address as *const _,
            0,
            null(),
            &mut data_size as *mut _,
            &mut device_id as *mut _ as *mut _,
        )
    };
    if status != kAudioHardwareNoError as i32 {
        return Err(format!(
            "AudioObjectGetPropertyData(kAudioHardwarePropertyDefaultInputDevice) failed with status {status}"
        ));
    }

    if device_id == 0 {
        Ok(None)
    } else {
        Ok(Some(device_id as u32))
    }
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
    sample_rate_hz: u32,
    samples: Arc<Mutex<Vec<i16>>>,
    audio_level_bits: Arc<AtomicU32>,
    on_input_chunk: Option<AudioInputChunkCallback>,
    stream_error_tx: Sender<String>,
) -> Result<Stream, String> {
    match sample_format {
        SampleFormat::F32 => {
            let samples = Arc::clone(&samples);
            let level_bits = Arc::clone(&audio_level_bits);
            let on_input_chunk = on_input_chunk.clone();
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
                            sample_rate_hz,
                            on_input_chunk.as_ref(),
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
            let on_input_chunk = on_input_chunk.clone();
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
                            sample_rate_hz,
                            on_input_chunk.as_ref(),
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
            let on_input_chunk = on_input_chunk.clone();
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
                            sample_rate_hz,
                            on_input_chunk.as_ref(),
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
    sample_rate_hz: u32,
    on_input_chunk: Option<&AudioInputChunkCallback>,
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
    let should_emit_chunks = on_input_chunk.is_some();
    let mut mono_chunk = if should_emit_chunks {
        Some(Vec::with_capacity(data.len() / channels))
    } else {
        None
    };

    if let Ok(mut sample_buffer) = samples.lock() {
        sample_buffer.reserve(data.len() / channels);

        for frame in data.chunks_exact(channels) {
            let mut mixed = 0.0_f32;
            for &sample in frame {
                mixed += to_f32(sample);
            }

            let normalized = (mixed / channels as f32).clamp(-1.0, 1.0);
            let mono_pcm16 = float_to_pcm16(normalized);
            sample_buffer.push(mono_pcm16);
            if let Some(chunk) = mono_chunk.as_mut() {
                chunk.push(mono_pcm16);
            }

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

    if let (Some(callback), Some(pcm16_mono_samples)) = (on_input_chunk, mono_chunk) {
        if !pcm16_mono_samples.is_empty() {
            callback(AudioInputChunk {
                pcm16_mono_samples,
                sample_rate_hz,
            });
        }
    }
}

fn quantize_audio_level_for_emit(level: f32) -> f32 {
    let clamped = level.clamp(0.0, 1.0);
    (clamped * 100.0).round() / 100.0
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

    #[cfg(target_endian = "little")]
    {
        let sample_byte_len = samples
            .len()
            .checked_mul(std::mem::size_of::<i16>())
            .ok_or_else(|| "WAV data size overflow".to_string())?;
        // SAFETY: `i16` is a plain old data type and `samples` remains alive for the
        // duration of this conversion.
        let sample_bytes =
            unsafe { std::slice::from_raw_parts(samples.as_ptr() as *const u8, sample_byte_len) };
        wav_bytes.extend_from_slice(sample_bytes);
    }

    #[cfg(not(target_endian = "little"))]
    {
        for sample in samples {
            wav_bytes.extend_from_slice(&sample.to_le_bytes());
        }
    }

    Ok(wav_bytes)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use super::{
        await_worker_startup, build_macos_identity_lookup_by_name, build_microphone_device_id,
        ensure_unique_device_id, float_to_pcm16, legacy_device_slug, pcm16_to_wav_bytes,
        quantize_audio_level_for_emit, run_recording_loop, select_input_device_index,
        slugify_device_name, take_macos_identity_by_device_name, InputDeviceSelectionCandidate,
        MacosCoreAudioDeviceIdentity, RecordingLoopExit, RecordingRuntime,
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
    fn microphone_id_uses_coreaudio_uid_when_available() {
        assert_eq!(
            build_microphone_device_id(
                "MacBook Pro Microphone",
                Some("BuiltInMicDeviceUID"),
                Some(23),
            ),
            "coreaudio:BuiltInMicDeviceUID"
        );
    }

    #[test]
    fn microphone_id_falls_back_to_coreaudio_device_id_when_uid_missing() {
        assert_eq!(
            build_microphone_device_id("USB-C 🎤 Input", None, Some(41)),
            "coreaudio-device-id:41"
        );
        assert_eq!(
            build_microphone_device_id("USB-C 🎤 Input", Some("   "), Some(42)),
            "coreaudio-device-id:42"
        );
    }

    #[test]
    fn microphone_id_falls_back_to_name_when_coreaudio_identity_missing() {
        assert_eq!(
            build_microphone_device_id("USB-C 🎤 Input", None, None),
            "name:usb-c-input"
        );
        assert_eq!(
            build_microphone_device_id("USB-C 🎤 Input", Some("   "), None),
            "name:usb-c-input"
        );
    }

    #[test]
    fn microphone_id_distinguishes_duplicate_names_using_coreaudio_identity() {
        let first_uid_id = build_microphone_device_id("USB Mic", Some("uid-A"), Some(10));
        let second_uid_id = build_microphone_device_id("USB Mic", Some("uid-B"), Some(11));
        let first_device_id = build_microphone_device_id("USB Mic", None, Some(10));
        let second_device_id = build_microphone_device_id("USB Mic", None, Some(11));

        assert_ne!(first_uid_id, second_uid_id);
        assert_ne!(first_device_id, second_device_id);
        assert_eq!(first_device_id, "coreaudio-device-id:10");
        assert_eq!(second_device_id, "coreaudio-device-id:11");
        assert!(first_uid_id.starts_with("coreaudio:"));
        assert!(second_uid_id.starts_with("coreaudio:"));
        assert!(!first_uid_id.contains("usb-mic"));
        assert!(!second_uid_id.contains("usb-mic"));
        assert!(!first_device_id.contains("usb-mic"));
        assert!(!second_device_id.contains("usb-mic"));
    }

    #[test]
    fn macos_identity_matching_uses_device_name_not_global_position() {
        let identities = vec![
            MacosCoreAudioDeviceIdentity {
                device_id: 101,
                uid: Some("uid-built-in".to_string()),
                name: Some("Built-in Mic".to_string()),
            },
            MacosCoreAudioDeviceIdentity {
                device_id: 102,
                uid: Some("uid-usb".to_string()),
                name: Some("USB Mic".to_string()),
            },
        ];

        let mut lookup = build_macos_identity_lookup_by_name(identities);
        let usb = take_macos_identity_by_device_name(&mut lookup, "USB Mic")
            .expect("USB identity should be found by name");
        let built_in = take_macos_identity_by_device_name(&mut lookup, "Built-in Mic")
            .expect("Built-in identity should be found by name");

        assert_eq!(usb.uid.as_deref(), Some("uid-usb"));
        assert_eq!(built_in.uid.as_deref(), Some("uid-built-in"));
    }

    #[test]
    fn macos_identity_matching_consumes_duplicate_names_in_order() {
        let identities = vec![
            MacosCoreAudioDeviceIdentity {
                device_id: 201,
                uid: Some("uid-a".to_string()),
                name: Some("USB Mic".to_string()),
            },
            MacosCoreAudioDeviceIdentity {
                device_id: 202,
                uid: Some("uid-b".to_string()),
                name: Some("USB Mic".to_string()),
            },
        ];

        let mut lookup = build_macos_identity_lookup_by_name(identities);
        let first = take_macos_identity_by_device_name(&mut lookup, "USB Mic")
            .expect("first duplicate should be available");
        let second = take_macos_identity_by_device_name(&mut lookup, "USB Mic")
            .expect("second duplicate should be available");

        assert_eq!(first.uid.as_deref(), Some("uid-a"));
        assert_eq!(second.uid.as_deref(), Some("uid-b"));
    }

    #[test]
    fn ensure_unique_device_id_suffixes_duplicate_candidates() {
        let mut id_occurrences = HashMap::new();

        let first = ensure_unique_device_id("name:mic".to_string(), &mut id_occurrences);
        let second = ensure_unique_device_id("name:mic".to_string(), &mut id_occurrences);
        let third = ensure_unique_device_id("name:mic".to_string(), &mut id_occurrences);

        assert_eq!(first, "name:mic");
        assert_eq!(second, "name:mic#2");
        assert_eq!(third, "name:mic#3");
    }

    #[test]
    fn legacy_device_slug_accepts_previous_id_formats() {
        assert_eq!(
            legacy_device_slug("mic-0-macbook-pro-microphone"),
            Some("macbook-pro-microphone")
        );
        assert_eq!(
            legacy_device_slug("mic-name-macbook-pro-microphone"),
            Some("macbook-pro-microphone")
        );
        assert_eq!(
            legacy_device_slug("name:macbook-pro-microphone"),
            Some("macbook-pro-microphone")
        );
        assert_eq!(legacy_device_slug("coreaudio:BuiltInMicDeviceUID"), None);
    }

    #[test]
    fn selection_falls_back_to_default_when_preferred_device_is_missing() {
        let devices = vec![
            InputDeviceSelectionCandidate {
                id: "coreaudio:built-in".to_string(),
                name: "Built-in Mic".to_string(),
                is_default: true,
            },
            InputDeviceSelectionCandidate {
                id: "coreaudio:usb".to_string(),
                name: "USB Mic".to_string(),
                is_default: false,
            },
        ];

        let selected = select_input_device_index(
            &devices,
            Some("coreaudio:does-not-exist"),
            Some("Built-in Mic"),
        )
        .expect("selection should fall back to default input");

        assert_eq!(selected, 0);
    }

    #[test]
    fn selection_prefers_cpal_default_name_when_default_flags_are_unset() {
        let devices = vec![
            InputDeviceSelectionCandidate {
                id: "name:mic-a".to_string(),
                name: "Mic A".to_string(),
                is_default: false,
            },
            InputDeviceSelectionCandidate {
                id: "name:mic-b".to_string(),
                name: "Mic B".to_string(),
                is_default: false,
            },
        ];

        let selected = select_input_device_index(&devices, None, Some("Mic B"))
            .expect("selection should use cpal default device name");

        assert_eq!(selected, 1);
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
    fn audio_level_quantization_clamps_and_rounds() {
        assert_eq!(quantize_audio_level_for_emit(-0.2), 0.0);
        assert_eq!(quantize_audio_level_for_emit(0.004), 0.0);
        assert_eq!(quantize_audio_level_for_emit(0.005), 0.01);
        assert_eq!(quantize_audio_level_for_emit(0.456), 0.46);
        assert_eq!(quantize_audio_level_for_emit(1.6), 1.0);
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

    #[test]
    fn startup_timeout_returns_promptly_without_waiting_for_worker_join() {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<RecordingRuntime, String>>();
        let (stop_tx, _stop_rx) = mpsc::channel::<()>();
        let mut join_handle = Some(thread::spawn(move || {
            let _keep_ready_sender_alive = ready_tx;
            thread::sleep(Duration::from_millis(750));
        }));

        let started_at = Instant::now();
        let result = await_worker_startup(
            &ready_rx,
            &stop_tx,
            &mut join_handle,
            Duration::from_millis(25),
        );

        let elapsed = started_at.elapsed();
        assert!(
            matches!(result, Err(message) if message == "Timed out while starting microphone stream")
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "startup timeout should return quickly without waiting for worker thread; elapsed: {elapsed:?}"
        );
        assert!(
            join_handle.is_none(),
            "worker handle should be detached on timeout"
        );
    }
}
