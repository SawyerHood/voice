# Performance Tuning Notes

Date: 2026-02-21
Branch: `voice-perf-tuning`

## What changed

### 1) Latency reduction
- **Audio start path now avoids duplicate device enumeration** in `src-tauri/src/audio_capture_service/mod.rs`.
  - Before: the selected microphone was resolved once in `start_recording` and then resolved again inside the worker thread.
  - After: resolution happens once in the worker, and resolved metadata is returned to the control path.
  - Why: reduces hotkey-to-recording startup work and avoids duplicate device/config calls.
- **Pipeline stop path now attempts insertion immediately after transcription** in `src-tauri/src/voice_pipeline/mod.rs`.
  - The insertion call is made before transcript event emission, while still preserving transcript emission even when insertion fails.
  - Why: prioritizes transcript delivery into the target app as soon as transcription returns.

### 2) Long dictation stability
- **WAV encoding for large recordings is more efficient** in `src-tauri/src/audio_capture_service/mod.rs`.
  - On little-endian targets, PCM samples are appended in one bulk byte copy instead of per-sample writes.
  - Why: lowers CPU overhead and memory churn during large WAV generation.
- **OpenAI uploads no longer deep-clone audio bytes per retry** in `src-tauri/src/transcription/openai.rs`.
  - Switched retry payload handling from `Vec<u8>::clone()` to shared `bytes::Bytes` buffers for multipart uploads.
  - Why: avoids repeated large allocations/copies for long recordings with retries.
- **OpenAI default request timeout increased from 30s to 180s**.
  - Why: longer audio uploads + processing are less likely to fail prematurely.

### 3) General performance
- **Audio level event emissions are quantized/deduplicated** in `src-tauri/src/audio_capture_service/mod.rs`.
  - Emission is rounded to 1% and only emitted when the value changes.
  - Why: reduces backend->frontend event traffic for long recordings.
- **Frontend audio-level updates are gated and quantized** in `src/App.tsx`.
  - Non-zero level updates are ignored unless app status is `listening`, and updates are quantized to 1%.
  - Why: reduces avoidable React renders from high-frequency meter updates.

## Tests and validation

- `cargo build --manifest-path src-tauri/Cargo.toml` ✅
- `cargo test --manifest-path src-tauri/Cargo.toml` ✅
- `pnpm test` ✅
- `pnpm build` ✅

## Additional test coverage added

- `audio_capture_service::tests::audio_level_quantization_clamps_and_rounds`
- `voice_pipeline::tests::hotkey_stop_insertion_failure_emits_transcript_and_sets_error`
