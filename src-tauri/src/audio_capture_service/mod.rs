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

        let (ready_tx, ready_rx) = mpsc::channel::<Result<RecordingRuntime, String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let mut join_handle = Some(thread::spawn(move || {
            recording_thread_main(
                worker_preferred_device_id,
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
                error!(error = %err, "microphone worker failed to initialize");
                return Err(err);
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = stop_tx.send(());
                if let Some(handle) = join_handle.take() {
                    let _ = handle.join();
                }
                error!("microphone worker timed out while starting");
                return Err("Timed out while starting microphone stream".to_string());
            }
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(handle) = join_handle.take() {
                    let _ = handle.join();
                }
                error!("microphone worker disconnected during startup");
                return Err("Microphone stream failed to initialize".to_string());
            }
        };

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

fn recording_thread_main(
    preferred_device_id: Option<String>,
    samples: Arc<Mutex<Vec<i16>>>,
    audio_level_bits: Arc<AtomicU32>,
    app_handle: AppHandle,
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
) -> Result<(Stream, RecordingRuntime, Receiver<String>), String> {
    let host = cpal::default_host();
    let devices = enumerate_input_devices(&host)?;
    if devices.is_empty() {
        return Err("No microphone input devices are available".to_string());
    }

    let selected_device = select_input_device(devices, preferred_device_id)?;
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
        samples,
        audio_level_bits,
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
    let mut devices = Vec::new();
    let mut macos_uids_by_name = macos_input_device_uids_by_name();
    let mut id_occurrences: HashMap<String, usize> = HashMap::new();

    let input_devices = host
        .input_devices()
        .map_err(|err| format!("Failed to enumerate input devices: {err}"))?;

    for device in input_devices {
        let name = device
            .name()
            .unwrap_or_else(|_| format!("Microphone {}", devices.len() + 1));
        let coreaudio_uid = take_macos_uid_for_device_name(&mut macos_uids_by_name, &name);
        let id = ensure_unique_device_id(
            build_microphone_device_id(&name, coreaudio_uid.as_deref()),
            &mut id_occurrences,
        );
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

    debug!(count = devices.len(), "input device enumeration complete");
    Ok(devices)
}

fn select_input_device(
    mut devices: Vec<EnumeratedInputDevice>,
    preferred_device_id: Option<&str>,
) -> Result<EnumeratedInputDevice, String> {
    if let Some(device_id) = preferred_device_id {
        if let Some(index) = devices.iter().position(|device| device.id == device_id) {
            debug!(device_id, "selected preferred input device by id");
            return Ok(devices.swap_remove(index));
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
                return Ok(devices.swap_remove(index));
            }
        }

        return Err(format!("No microphone found for id '{device_id}'"));
    }

    let selected = devices
        .into_iter()
        .min_by_key(|device| if device.is_default { 0 } else { 1 })
        .ok_or_else(|| "No microphone input devices are available".to_string())?;
    debug!(
        device_id = %selected.id,
        device_name = %selected.name,
        is_default = selected.is_default,
        "selected microphone device"
    );
    Ok(selected)
}

fn build_microphone_device_id(name: &str, coreaudio_uid: Option<&str>) -> String {
    if let Some(uid) = coreaudio_uid.map(str::trim).filter(|uid| !uid.is_empty()) {
        return format!("coreaudio:{uid}");
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

fn take_macos_uid_for_device_name(
    macos_uids_by_name: &mut HashMap<String, VecDeque<String>>,
    name: &str,
) -> Option<String> {
    let uid = macos_uids_by_name
        .get_mut(name)
        .and_then(VecDeque::pop_front);

    if macos_uids_by_name.get(name).is_some_and(VecDeque::is_empty) {
        macos_uids_by_name.remove(name);
    }

    uid
}

#[cfg(target_os = "macos")]
fn macos_input_device_uids_by_name() -> HashMap<String, VecDeque<String>> {
    match macos_collect_input_device_name_uid_pairs() {
        Ok(pairs) => {
            let mut uids_by_name: HashMap<String, VecDeque<String>> = HashMap::new();
            for (name, uid) in pairs {
                uids_by_name.entry(name).or_default().push_back(uid);
            }
            uids_by_name
        }
        Err(error) => {
            warn!(%error, "failed to collect CoreAudio input device UIDs");
            HashMap::new()
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_input_device_uids_by_name() -> HashMap<String, VecDeque<String>> {
    HashMap::new()
}

#[cfg(target_os = "macos")]
fn macos_collect_input_device_name_uid_pairs() -> Result<Vec<(String, String)>, String> {
    use core_foundation_sys::string::{
        kCFStringEncodingUTF8, CFStringGetCString, CFStringGetCStringPtr, CFStringRef,
    };
    use coreaudio::sys::{
        kAudioDevicePropertyDeviceNameCFString, kAudioDevicePropertyDeviceUID,
        kAudioDevicePropertyScopeOutput, kAudioHardwareNoError, kAudioHardwarePropertyDevices,
        kAudioObjectPropertyElementMaster, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyScopeInput, kAudioObjectSystemObject, AudioDeviceID,
        AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectPropertyAddress,
    };
    use std::{ffi::CStr, mem, os::raw::c_char, ptr::null};

    fn input_device_ids() -> Result<Vec<AudioDeviceID>, String> {
        let property_address = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeInput,
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

    let mut pairs = Vec::new();
    for device_id in input_device_ids()? {
        let name = read_device_string_property(
            device_id,
            kAudioDevicePropertyDeviceNameCFString,
            kAudioDevicePropertyScopeOutput,
        );
        let uid = read_device_string_property(
            device_id,
            kAudioDevicePropertyDeviceUID,
            kAudioObjectPropertyScopeGlobal,
        );

        if let (Some(name), Some(uid)) = (name, uid) {
            let trimmed_uid = uid.trim();
            if !trimmed_uid.is_empty() {
                pairs.push((name, trimmed_uid.to_string()));
            }
        }
    }

    Ok(pairs)
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
    use std::{collections::HashMap, sync::mpsc};

    use super::{
        build_microphone_device_id, ensure_unique_device_id, float_to_pcm16, legacy_device_slug,
        pcm16_to_wav_bytes, quantize_audio_level_for_emit, run_recording_loop, slugify_device_name,
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
    fn microphone_id_uses_coreaudio_uid_when_available() {
        assert_eq!(
            build_microphone_device_id("MacBook Pro Microphone", Some("BuiltInMicDeviceUID")),
            "coreaudio:BuiltInMicDeviceUID"
        );
    }

    #[test]
    fn microphone_id_falls_back_to_name_when_uid_missing() {
        assert_eq!(
            build_microphone_device_id("USB-C 🎤 Input", None),
            "name:usb-c-input"
        );
        assert_eq!(
            build_microphone_device_id("USB-C 🎤 Input", Some("   ")),
            "name:usb-c-input"
        );
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
}
