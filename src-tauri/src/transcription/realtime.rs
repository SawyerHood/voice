use std::{path::PathBuf, time::Duration};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, client::IntoClientRequest, http::HeaderValue, Message},
};
use tracing::{debug, info, warn};

#[cfg(not(test))]
use crate::api_key_store::ApiKeyStore;

use super::{
    normalize_transcript_text, TranscriptionError, TranscriptionOptions, TranscriptionResult,
};

const DEFAULT_OPENAI_REALTIME_ENDPOINT: &str = "wss://api.openai.com/v1/realtime";
const DEFAULT_OPENAI_REALTIME_MODEL: &str = "gpt-realtime";
const DEFAULT_OPENAI_TRANSCRIPTION_MODEL: &str = "gpt-4o-mini-transcribe";
const OPENAI_REALTIME_BETA_HEADER_VALUE: &str = "realtime=v1";
const DEFAULT_COMMIT_TIMEOUT_SECS: u64 = 20;
const REALTIME_OUTPUT_SAMPLE_RATE_HZ: u32 = 24_000;
const EVENT_SESSION_CREATED: &str = "session.created";
const EVENT_SESSION_UPDATED: &str = "session.updated";
const EVENT_SPEECH_STARTED: &str = "input_audio_buffer.speech_started";
const EVENT_SPEECH_STOPPED: &str = "input_audio_buffer.speech_stopped";
const EVENT_DELTA: &str = "conversation.item.input_audio_transcription.delta";
const EVENT_COMPLETED: &str = "conversation.item.input_audio_transcription.completed";
const EVENT_FALLBACK_DELTA: &str = "transcript.text.delta";
const EVENT_FALLBACK_COMPLETED: &str = "transcript.text.done";
// Also accept legacy transcription session lifecycle events for compatibility.
const EVENT_SESSION_CREATED_LEGACY: &str = "transcription_session.created";
const EVENT_SESSION_UPDATED_LEGACY: &str = "transcription_session.updated";
const EVENT_ERROR: &str = "error";

#[derive(Debug, Clone)]
pub struct OpenAiRealtimeTranscriptionConfig {
    pub api_key: Option<String>,
    pub api_key_store_app_data_dir: Option<PathBuf>,
    pub endpoint: String,
    pub realtime_model: String,
    pub transcription_model: String,
    pub commit_timeout_secs: u64,
}

impl Default for OpenAiRealtimeTranscriptionConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            api_key_store_app_data_dir: None,
            endpoint: DEFAULT_OPENAI_REALTIME_ENDPOINT.to_string(),
            realtime_model: DEFAULT_OPENAI_REALTIME_MODEL.to_string(),
            transcription_model: DEFAULT_OPENAI_TRANSCRIPTION_MODEL.to_string(),
            commit_timeout_secs: DEFAULT_COMMIT_TIMEOUT_SECS,
        }
    }
}

impl OpenAiRealtimeTranscriptionConfig {
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Some(endpoint) = read_non_empty_env("OPENAI_REALTIME_TRANSCRIPTION_ENDPOINT")
            .or_else(|| read_non_empty_env("OPENAI_REALTIME_ENDPOINT"))
        {
            config.endpoint = endpoint;
        }

        if let Some(realtime_model) = read_non_empty_env("OPENAI_REALTIME_MODEL") {
            config.realtime_model = realtime_model;
        }

        if let Some(transcription_model) = read_non_empty_env("OPENAI_REALTIME_TRANSCRIPTION_MODEL")
            .or_else(|| read_non_empty_env("OPENAI_TRANSCRIPTION_MODEL"))
        {
            config.transcription_model = transcription_model;
        }

        if let Some(timeout_secs) = read_u64_env("OPENAI_REALTIME_COMMIT_TIMEOUT_SECS") {
            config.commit_timeout_secs = timeout_secs.max(1);
        }

        debug!(
            endpoint = %config.endpoint,
            realtime_model = %config.realtime_model,
            transcription_model = %config.transcription_model,
            commit_timeout_secs = config.commit_timeout_secs,
            "loaded OpenAI realtime transcription config"
        );

        config
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiRealtimeTranscriptionClient {
    config: OpenAiRealtimeTranscriptionConfig,
}

impl OpenAiRealtimeTranscriptionClient {
    pub fn new(config: OpenAiRealtimeTranscriptionConfig) -> Self {
        info!(
            endpoint = %config.endpoint,
            realtime_model = %config.realtime_model,
            transcription_model = %config.transcription_model,
            commit_timeout_secs = config.commit_timeout_secs,
            "OpenAI realtime transcription client initialized"
        );
        Self { config }
    }

    pub fn model_supports_realtime(&self) -> bool {
        model_supports_realtime(&self.config.realtime_model)
    }

    pub fn model(&self) -> &str {
        &self.config.realtime_model
    }

    pub fn begin_session(
        &self,
        options: TranscriptionOptions,
    ) -> Result<RealtimeTranscriptionSession, TranscriptionError> {
        if !self.model_supports_realtime() {
            return Err(TranscriptionError::Provider(format!(
                "Configured model `{}` does not support realtime transcription",
                self.config.realtime_model
            )));
        }

        let api_key = self.api_key()?;
        let commit_timeout = Duration::from_secs(self.config.commit_timeout_secs.max(1));
        let (command_tx, command_rx) = mpsc::unbounded_channel::<RealtimeCommand>();
        let (result_tx, result_rx) =
            oneshot::channel::<Result<TranscriptionResult, TranscriptionError>>();

        let runtime_config = self.config.clone();
        tauri::async_runtime::spawn(async move {
            let result = run_realtime_session(runtime_config, api_key, options, command_rx).await;
            match &result {
                Ok(transcription) => info!(
                    transcript_chars = transcription.text.chars().count(),
                    "realtime transcription session finished successfully"
                ),
                Err(error) => warn!(error = %error, "realtime transcription session failed"),
            }
            let _ = result_tx.send(result);
        });

        Ok(RealtimeTranscriptionSession {
            audio_sender: RealtimeAudioSender { command_tx },
            result_rx,
            commit_timeout,
        })
    }

    fn api_key(&self) -> Result<String, TranscriptionError> {
        if let Some(explicit_key) = self
            .config
            .api_key
            .clone()
            .and_then(|value| normalize_optional_string(Some(value)))
        {
            return Ok(explicit_key);
        }

        #[cfg(not(test))]
        {
            if let Some(app_data_dir) = self.config.api_key_store_app_data_dir.clone() {
                match ApiKeyStore::new(app_data_dir).get_api_key("openai") {
                    Ok(Some(stored_key)) => return Ok(stored_key),
                    Ok(None) => {}
                    Err(error) => {
                        if let Some(env_key) = read_non_empty_env("OPENAI_API_KEY") {
                            warn!(
                                error = %error,
                                "falling back to OPENAI_API_KEY after API key file read failure"
                            );
                            return Ok(env_key);
                        }

                        return Err(TranscriptionError::Provider(format!(
                            "Unable to read API key from local API key store: {error}",
                        )));
                    }
                }
            }
        }

        read_non_empty_env("OPENAI_API_KEY").ok_or(TranscriptionError::MissingApiKey)
    }
}

#[derive(Debug, Clone)]
pub struct RealtimeAudioSender {
    command_tx: mpsc::UnboundedSender<RealtimeCommand>,
}

impl RealtimeAudioSender {
    pub fn append_pcm16_mono(
        &self,
        samples: Vec<i16>,
        sample_rate_hz: u32,
    ) -> Result<(), TranscriptionError> {
        self.command_tx
            .send(RealtimeCommand::Append(AudioChunk {
                samples,
                sample_rate_hz,
            }))
            .map_err(|_| {
                TranscriptionError::Network(
                    "Realtime transcription session is no longer active".to_string(),
                )
            })
    }

    pub fn close(&self) {
        let _ = self.command_tx.send(RealtimeCommand::Close);
    }

    fn commit(&self) -> Result<(), TranscriptionError> {
        self.command_tx.send(RealtimeCommand::Commit).map_err(|_| {
            TranscriptionError::Network(
                "Realtime transcription session is no longer active".to_string(),
            )
        })
    }
}

pub struct RealtimeTranscriptionSession {
    audio_sender: RealtimeAudioSender,
    result_rx: oneshot::Receiver<Result<TranscriptionResult, TranscriptionError>>,
    commit_timeout: Duration,
}

impl std::fmt::Debug for RealtimeTranscriptionSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealtimeTranscriptionSession")
            .field("commit_timeout", &self.commit_timeout)
            .finish_non_exhaustive()
    }
}

impl RealtimeTranscriptionSession {
    pub fn audio_sender(&self) -> RealtimeAudioSender {
        self.audio_sender.clone()
    }

    pub fn close(&self) {
        self.audio_sender.close();
    }

    pub async fn commit_and_wait(self) -> Result<TranscriptionResult, TranscriptionError> {
        let RealtimeTranscriptionSession {
            audio_sender,
            result_rx,
            commit_timeout,
        } = self;

        if let Err(commit_error) = audio_sender.commit() {
            warn!(
                error = %commit_error,
                "realtime commit command was rejected; waiting for session result"
            );
            return match result_rx.await {
                Ok(result) => result,
                Err(_) => Err(commit_error),
            };
        }

        match tokio::time::timeout(commit_timeout, result_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(TranscriptionError::Network(
                "Realtime transcription session closed unexpectedly".to_string(),
            )),
            Err(_) => {
                audio_sender.close();
                Err(TranscriptionError::Network(format!(
                    "Timed out waiting for realtime transcription completion after {}s",
                    commit_timeout.as_secs(),
                )))
            }
        }
    }
}

#[derive(Debug)]
struct AudioChunk {
    samples: Vec<i16>,
    sample_rate_hz: u32,
}

#[derive(Debug)]
enum RealtimeCommand {
    Append(AudioChunk),
    Commit,
    Close,
}

#[derive(Debug, PartialEq, Eq)]
enum ParsedServerEvent {
    SessionCreated,
    SessionUpdated,
    SpeechStarted,
    SpeechStopped,
    Delta(String),
    Completed(String),
    Error(String),
    Ignore,
}

async fn run_realtime_session(
    config: OpenAiRealtimeTranscriptionConfig,
    api_key: String,
    options: TranscriptionOptions,
    mut command_rx: mpsc::UnboundedReceiver<RealtimeCommand>,
) -> Result<TranscriptionResult, TranscriptionError> {
    let endpoint = resolve_realtime_endpoint(&config.endpoint)?;
    let mut request = endpoint.clone().into_client_request().map_err(|error| {
        TranscriptionError::Provider(format!(
            "Invalid realtime websocket endpoint `{endpoint}`: {error}",
        ))
    })?;
    let authorization_header_value =
        HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|error| {
            TranscriptionError::Provider(format!(
                "Invalid realtime websocket Authorization header: {error}",
            ))
        })?;
    request
        .headers_mut()
        .insert("Authorization", authorization_header_value);
    request.headers_mut().insert(
        "OpenAI-Beta",
        HeaderValue::from_static(OPENAI_REALTIME_BETA_HEADER_VALUE),
    );

    info!(
        endpoint = %endpoint,
        realtime_model = %config.realtime_model,
        transcription_model = %config.transcription_model,
        "connecting realtime transcription websocket"
    );
    let (ws_stream, response) = connect_async(request).await.map_err(|error| {
        let mapped = map_websocket_error(error);
        warn!(
            endpoint = %endpoint,
            realtime_model = %config.realtime_model,
            transcription_model = %config.transcription_model,
            error = %mapped,
            "failed to connect realtime transcription websocket"
        );
        mapped
    })?;
    info!(
        endpoint = %endpoint,
        status = %response.status(),
        request_id = ?response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        negotiated_protocol = ?response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|value| value.to_str().ok()),
        "connected realtime transcription websocket"
    );

    let (mut ws_writer, mut ws_reader) = ws_stream.split();

    let session_update = build_session_update_payload(&config, &options);
    ws_writer
        .send(Message::Text(session_update.to_string().into()))
        .await
        .map_err(|error| {
            let mapped = map_websocket_error(error);
            warn!(error = %mapped, "failed to send realtime session update");
            mapped
        })?;

    let request_language = normalize_optional_string(options.language.clone());
    let on_delta = options.on_delta.clone();
    let mut transcript_from_deltas = String::new();
    let mut transcript_done: Option<String> = None;
    let mut commit_sent = false;

    loop {
        tokio::select! {
            maybe_command = command_rx.recv() => {
                let Some(command) = maybe_command else {
                    return Err(TranscriptionError::Provider(
                        "Realtime transcription session ended before completion".to_string(),
                    ));
                };

                match command {
                    RealtimeCommand::Append(chunk) => {
                        if commit_sent {
                            continue;
                        }
                        let samples = resample_pcm16_linear(
                            &chunk.samples,
                            chunk.sample_rate_hz.max(1),
                            REALTIME_OUTPUT_SAMPLE_RATE_HZ,
                        );
                        if samples.is_empty() {
                            continue;
                        }
                        let audio_payload = encode_pcm16_base64(&samples);
                        let payload = json!({
                            "type": "input_audio_buffer.append",
                            "audio": audio_payload,
                        });
                        ws_writer
                            .send(Message::Text(payload.to_string().into()))
                            .await
                            .map_err(|error| {
                                let mapped = map_websocket_error(error);
                                warn!(error = %mapped, "failed to send realtime audio chunk");
                                mapped
                            })?;
                    }
                    RealtimeCommand::Commit => {
                        if commit_sent {
                            continue;
                        }
                        commit_sent = true;
                        let payload = json!({ "type": "input_audio_buffer.commit" });
                        ws_writer
                            .send(Message::Text(payload.to_string().into()))
                            .await
                            .map_err(|error| {
                                let mapped = map_websocket_error(error);
                                warn!(error = %mapped, "failed to send realtime commit");
                                mapped
                            })?;

                        if transcript_done.is_some() {
                            break;
                        }
                    }
                    RealtimeCommand::Close => {
                        let _ = ws_writer.send(Message::Close(None)).await;
                        return Err(TranscriptionError::Provider(
                            "Realtime transcription session closed".to_string(),
                        ));
                    }
                }
            }
            maybe_message = ws_reader.next() => {
                let Some(message_result) = maybe_message else {
                    warn!("realtime websocket stream ended before transcript completion");
                    break;
                };
                let message = message_result.map_err(|error| {
                    let mapped = map_websocket_error(error);
                    warn!(error = %mapped, "realtime websocket read failed");
                    mapped
                })?;

                match message {
                    Message::Text(text) => {
                        let payload = serde_json::from_str::<Value>(text.as_ref()).map_err(|error| {
                            TranscriptionError::InvalidResponse(format!(
                                "Realtime websocket payload was not valid JSON: {error}",
                            ))
                        })?;
                        match parse_server_event(&payload) {
                            ParsedServerEvent::SessionCreated => {
                                debug!("realtime session created");
                            }
                            ParsedServerEvent::SessionUpdated => {
                                debug!("realtime session updated");
                            }
                            ParsedServerEvent::SpeechStarted => {
                                debug!("realtime VAD detected speech started");
                            }
                            ParsedServerEvent::SpeechStopped => {
                                debug!("realtime VAD detected speech stopped");
                            }
                            ParsedServerEvent::Delta(delta) => {
                                if let Some(callback) = on_delta.as_ref() {
                                    callback(delta.clone());
                                }
                                transcript_from_deltas.push_str(&delta);
                            }
                            ParsedServerEvent::Completed(text) => {
                                transcript_done = Some(text);
                                if commit_sent {
                                    break;
                                }
                            }
                            ParsedServerEvent::Error(message) => {
                                warn!(error_message = %message, "realtime API returned an error event");
                                return Err(TranscriptionError::Provider(message));
                            }
                            ParsedServerEvent::Ignore => {}
                        }
                    }
                    Message::Binary(_) => {}
                    Message::Ping(payload) => {
                        ws_writer
                            .send(Message::Pong(payload))
                            .await
                            .map_err(map_websocket_error)?;
                    }
                    Message::Pong(_) => {}
                    Message::Close(frame) => {
                        info!(
                            close_code = ?frame.as_ref().map(|close| close.code),
                            close_reason = ?frame.as_ref().map(|close| close.reason.to_string()),
                            "realtime websocket closed by server"
                        );
                        break;
                    }
                    Message::Frame(_) => {}
                }
            }
        }
    }

    let final_text = if let Some(done) = transcript_done {
        done
    } else if transcript_from_deltas.trim().is_empty() {
        warn!(
            commit_sent,
            "realtime session ended without a transcript payload"
        );
        return Err(TranscriptionError::InvalidResponse(
            "Realtime API did not return a transcript".to_string(),
        ));
    } else {
        transcript_from_deltas
    };

    Ok(TranscriptionResult {
        text: normalize_transcript_text(&final_text),
        language: request_language,
        duration_secs: None,
        confidence: None,
    })
}

fn resolve_realtime_endpoint(endpoint: &str) -> Result<String, TranscriptionError> {
    let mut url = Url::parse(endpoint).map_err(|error| {
        TranscriptionError::Provider(format!(
            "Invalid realtime websocket endpoint `{endpoint}`: {error}",
        ))
    })?;

    let retained_pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter_map(|(key, value)| {
            if key == "model" || key == "intent" {
                None
            } else {
                Some((key.into_owned(), value.into_owned()))
            }
        })
        .collect();

    {
        let mut query_pairs = url.query_pairs_mut();
        query_pairs.clear();
        for (key, value) in retained_pairs {
            query_pairs.append_pair(&key, &value);
        }
        query_pairs.append_pair("intent", "transcription");
    }

    Ok(url.to_string())
}

fn parse_server_event(payload: &Value) -> ParsedServerEvent {
    let Some(event_type) = payload.get("type").and_then(Value::as_str) else {
        return ParsedServerEvent::Ignore;
    };

    match event_type {
        EVENT_SESSION_CREATED | EVENT_SESSION_CREATED_LEGACY => ParsedServerEvent::SessionCreated,
        EVENT_SESSION_UPDATED | EVENT_SESSION_UPDATED_LEGACY => ParsedServerEvent::SessionUpdated,
        EVENT_SPEECH_STARTED => ParsedServerEvent::SpeechStarted,
        EVENT_SPEECH_STOPPED => ParsedServerEvent::SpeechStopped,
        EVENT_DELTA | EVENT_FALLBACK_DELTA => {
            extract_first_text(payload, &["delta", "/delta", "/item/delta", "/item/text"])
                .map(ParsedServerEvent::Delta)
                .unwrap_or(ParsedServerEvent::Ignore)
        }
        EVENT_COMPLETED | EVENT_FALLBACK_COMPLETED => extract_first_string(
            payload,
            &["transcript", "text", "/item/transcript", "/item/text"],
        )
        .map(ParsedServerEvent::Completed)
        .unwrap_or(ParsedServerEvent::Ignore),
        EVENT_ERROR => ParsedServerEvent::Error(
            extract_first_text(
                payload,
                &[
                    "/error/message",
                    "/error/type",
                    "message",
                    "error",
                    "/details/message",
                ],
            )
            .unwrap_or_else(|| "Realtime API returned an error event".to_string()),
        ),
        _ => ParsedServerEvent::Ignore,
    }
}

fn extract_first_text(payload: &Value, keys_or_pointers: &[&str]) -> Option<String> {
    for key_or_pointer in keys_or_pointers {
        let maybe_value = if key_or_pointer.starts_with('/') {
            payload.pointer(key_or_pointer)
        } else {
            payload.get(*key_or_pointer)
        };
        if let Some(value) = maybe_value.and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn extract_first_string(payload: &Value, keys_or_pointers: &[&str]) -> Option<String> {
    for key_or_pointer in keys_or_pointers {
        let maybe_value = if key_or_pointer.starts_with('/') {
            payload.pointer(key_or_pointer)
        } else {
            payload.get(*key_or_pointer)
        };
        if let Some(value) = maybe_value.and_then(Value::as_str) {
            return Some(value.to_string());
        }
    }
    None
}

fn build_session_update_payload(
    config: &OpenAiRealtimeTranscriptionConfig,
    options: &TranscriptionOptions,
) -> Value {
    let mut transcription_config = json!({ "model": config.transcription_model.clone() });

    if let Some(language) = normalize_optional_string(options.language.clone()) {
        transcription_config["language"] = Value::String(language);
    }

    if let Some(prompt) = build_prompt(options.prompt.clone(), options.context_hint.clone()) {
        transcription_config["prompt"] = Value::String(prompt);
    }

    json!({
        "type": "transcription_session.update",
        "session": {
            "input_audio_format": "pcm16",
            // Disable server VAD so explicit commit controls when transcription occurs.
            "turn_detection": null,
            "input_audio_transcription": transcription_config,
        }
    })
}

fn build_prompt(prompt: Option<String>, context_hint: Option<String>) -> Option<String> {
    match (
        normalize_optional_string(prompt),
        normalize_optional_string(context_hint),
    ) {
        (Some(prompt), Some(context_hint)) => Some(format!("{prompt}\n{context_hint}")),
        (Some(prompt), None) => Some(prompt),
        (None, Some(context_hint)) => Some(context_hint),
        (None, None) => None,
    }
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|content| {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn model_supports_realtime(model: &str) -> bool {
    let normalized = model.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }

    normalized.contains("realtime")
}

fn resample_pcm16_linear(input: &[i16], input_rate_hz: u32, output_rate_hz: u32) -> Vec<i16> {
    if input.is_empty() {
        return Vec::new();
    }

    if input_rate_hz == output_rate_hz {
        return input.to_vec();
    }

    let ratio = output_rate_hz as f64 / input_rate_hz as f64;
    let output_len = ((input.len() as f64) * ratio).round().max(1.0) as usize;
    let mut output = Vec::with_capacity(output_len);

    for output_index in 0..output_len {
        let source_position = output_index as f64 / ratio;
        let base_index = source_position.floor() as usize;
        let next_index = (base_index + 1).min(input.len().saturating_sub(1));
        let fraction = source_position - base_index as f64;

        let start = input[base_index] as f64;
        let end = input[next_index] as f64;
        let interpolated = (start + (end - start) * fraction)
            .round()
            .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        output.push(interpolated);
    }

    output
}

fn encode_pcm16_base64(samples: &[i16]) -> String {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    BASE64_STANDARD.encode(bytes)
}

fn map_websocket_error(error: tungstenite::Error) -> TranscriptionError {
    match error {
        tungstenite::Error::Http(response) => {
            let status = response.status();
            match status.as_u16() {
                401 | 403 => TranscriptionError::Authentication(format!(
                    "Realtime websocket authentication failed (HTTP {status})",
                )),
                429 => TranscriptionError::RateLimited(format!(
                    "Realtime websocket was rate limited (HTTP {status})",
                )),
                _ if status.is_server_error() => TranscriptionError::Network(format!(
                    "Realtime websocket server error (HTTP {status})",
                )),
                _ => TranscriptionError::Provider(format!(
                    "Realtime websocket connection failed (HTTP {status})",
                )),
            }
        }
        tungstenite::Error::Io(io_error) => TranscriptionError::Network(io_error.to_string()),
        tungstenite::Error::Tls(tls_error) => TranscriptionError::Network(tls_error.to_string()),
        tungstenite::Error::AlreadyClosed | tungstenite::Error::ConnectionClosed => {
            TranscriptionError::Network("Realtime websocket connection closed".to_string())
        }
        tungstenite::Error::Protocol(protocol_error) => {
            TranscriptionError::InvalidResponse(protocol_error.to_string())
        }
        other => TranscriptionError::Provider(other.to_string()),
    }
}

fn read_non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn read_u64_env(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{
        accept_hdr_async,
        tungstenite::{
            handshake::server::{Request, Response},
            Message,
        },
    };

    use super::*;

    #[test]
    fn model_supports_realtime_rejects_whisper() {
        assert!(model_supports_realtime("gpt-realtime"));
        assert!(model_supports_realtime("gpt-4o-realtime-preview"));
        assert!(!model_supports_realtime("gpt-4o-mini-transcribe"));
        assert!(!model_supports_realtime("whisper-1"));
        assert!(!model_supports_realtime("whisper-large"));
    }

    #[test]
    fn build_session_update_payload_includes_required_realtime_fields() {
        let config = OpenAiRealtimeTranscriptionConfig::default();
        let payload = build_session_update_payload(
            &config,
            &TranscriptionOptions {
                language: Some("en".to_string()),
                prompt: Some("Dictation".to_string()),
                context_hint: Some("Short sentences".to_string()),
                ..TranscriptionOptions::default()
            },
        );

        assert_eq!(
            payload["type"],
            Value::String("transcription_session.update".to_string())
        );
        assert_eq!(
            payload["session"]["input_audio_transcription"]["model"],
            Value::String(config.transcription_model.clone())
        );
        assert_eq!(
            payload["session"]["input_audio_transcription"]["language"],
            Value::String("en".to_string())
        );
        assert_eq!(
            payload["session"]["input_audio_transcription"]["prompt"],
            Value::String("Dictation\nShort sentences".to_string())
        );
        assert_eq!(
            payload["session"]["input_audio_format"],
            Value::String("pcm16".to_string())
        );
        assert_eq!(payload["session"]["turn_detection"], Value::Null);
    }

    #[test]
    fn resolve_realtime_endpoint_enforces_transcription_intent_and_strips_model() {
        let endpoint = "wss://api.openai.com/v1/realtime";
        let resolved =
            resolve_realtime_endpoint(endpoint).expect("endpoint should parse and include intent");
        let parsed = Url::parse(&resolved).expect("resolved endpoint should be valid URL");
        let model = parsed
            .query_pairs()
            .find(|(key, _)| key == "model")
            .map(|(_, value)| value.to_string());
        assert_eq!(model, None);
        let intent = parsed
            .query_pairs()
            .find(|(key, _)| key == "intent")
            .map(|(_, value)| value.to_string());
        assert_eq!(intent.as_deref(), Some("transcription"));
    }

    #[test]
    fn resolve_realtime_endpoint_overrides_existing_intent() {
        let endpoint =
            "wss://api.openai.com/v1/realtime?intent=conversation&foo=bar&model=gpt-realtime";
        let resolved = resolve_realtime_endpoint(endpoint).expect("endpoint should parse");
        let parsed = Url::parse(&resolved).expect("resolved endpoint should be valid URL");
        let intent = parsed
            .query_pairs()
            .find(|(key, _)| key == "intent")
            .map(|(_, value)| value.to_string());
        assert_eq!(intent.as_deref(), Some("transcription"));
        let model = parsed
            .query_pairs()
            .find(|(key, _)| key == "model")
            .map(|(_, value)| value.to_string());
        assert_eq!(model, None);
        let foo = parsed
            .query_pairs()
            .find(|(key, _)| key == "foo")
            .map(|(_, value)| value.to_string());
        assert_eq!(foo.as_deref(), Some("bar"));
    }

    #[test]
    fn parse_server_event_extracts_transcript_and_errors() {
        let completed_payload = json!({
            "type": "conversation.item.input_audio_transcription.completed",
            "transcript": "hello world"
        });
        let error_payload = json!({
            "type": "error",
            "error": {
                "message": "invalid event"
            }
        });

        assert_eq!(
            parse_server_event(&completed_payload),
            ParsedServerEvent::Completed("hello world".to_string())
        );
        assert_eq!(
            parse_server_event(&error_payload),
            ParsedServerEvent::Error("invalid event".to_string())
        );
    }

    #[test]
    fn resample_pcm16_linear_changes_sample_count() {
        let input = vec![0_i16, 1_000, 2_000, 3_000, 4_000, 5_000];
        let downsampled = resample_pcm16_linear(&input, 48_000, 24_000);
        let upsampled = resample_pcm16_linear(&input, 24_000, 48_000);

        assert!(!downsampled.is_empty());
        assert!(downsampled.len() < input.len());
        assert!(upsampled.len() > input.len());
    }

    #[tokio::test]
    async fn commit_and_wait_returns_session_error_when_commit_channel_is_closed() {
        let (command_tx, command_rx) = mpsc::unbounded_channel::<RealtimeCommand>();
        drop(command_rx);
        let (result_tx, result_rx) =
            oneshot::channel::<Result<TranscriptionResult, TranscriptionError>>();
        let expected_error = TranscriptionError::Authentication(
            "Realtime websocket authentication failed (HTTP 401 Unauthorized)".to_string(),
        );
        result_tx
            .send(Err(expected_error.clone()))
            .expect("session result should be sent");

        let session = RealtimeTranscriptionSession {
            audio_sender: RealtimeAudioSender { command_tx },
            result_rx,
            commit_timeout: Duration::from_secs(1),
        };

        let error = session
            .commit_and_wait()
            .await
            .expect_err("commit should surface the session error");
        assert_eq!(error, expected_error);
    }

    #[tokio::test]
    async fn websocket_protocol_flow_sends_session_update_append_and_commit() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have local address");
        let endpoint = format!("ws://{address}");
        let observed_authorization_header = Arc::new(Mutex::new(None::<String>));
        let authorization_header_for_server = Arc::clone(&observed_authorization_header);
        let observed_request_uri = Arc::new(Mutex::new(None::<String>));
        let request_uri_for_server = Arc::clone(&observed_request_uri);

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .expect("server should accept connection");
            let ws_stream =
                accept_hdr_async(stream, move |request: &Request, response: Response| {
                    let authorization_header = request
                        .headers()
                        .get("Authorization")
                        .and_then(|value| value.to_str().ok())
                        .map(|value| value.to_string());
                    *authorization_header_for_server
                        .lock()
                        .expect("authorization header lock should not be poisoned") =
                        authorization_header;

                    *request_uri_for_server
                        .lock()
                        .expect("request URI lock should not be poisoned") =
                        Some(request.uri().to_string());

                    Ok(response)
                })
                .await
                .expect("server handshake should succeed");

            let (mut write, mut read) = ws_stream.split();

            let first = read
                .next()
                .await
                .expect("session update message should arrive")
                .expect("session update frame should decode");
            let first_text = first.into_text().expect("session update should be text");
            let first_payload: Value = serde_json::from_str(first_text.as_ref())
                .expect("session update JSON should parse");
            assert_eq!(first_payload["type"], "transcription_session.update");
            assert_eq!(
                first_payload["session"]["input_audio_format"],
                Value::String("pcm16".to_string())
            );
            assert_eq!(
                first_payload["session"]["input_audio_transcription"]["model"],
                Value::String(DEFAULT_OPENAI_TRANSCRIPTION_MODEL.to_string())
            );
            assert_eq!(first_payload["session"]["turn_detection"], Value::Null);

            let append = read
                .next()
                .await
                .expect("append message should arrive")
                .expect("append frame should decode");
            let append_text = append.into_text().expect("append message should be text");
            let append_payload: Value =
                serde_json::from_str(append_text.as_ref()).expect("append JSON should parse");
            assert_eq!(append_payload["type"], "input_audio_buffer.append");
            assert!(append_payload["audio"]
                .as_str()
                .is_some_and(|audio| !audio.is_empty()));

            let commit = read
                .next()
                .await
                .expect("commit message should arrive")
                .expect("commit frame should decode");
            let commit_text = commit.into_text().expect("commit message should be text");
            let commit_payload: Value =
                serde_json::from_str(commit_text.as_ref()).expect("commit JSON should parse");
            assert_eq!(commit_payload["type"], "input_audio_buffer.commit");

            write
                .send(Message::Text(
                    json!({
                        "type": "conversation.item.input_audio_transcription.delta",
                        "delta": "hello "
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("server should send delta");
            write
                .send(Message::Text(
                    json!({
                        "type": "conversation.item.input_audio_transcription.completed",
                        "transcript": "hello world"
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("server should send completed transcript");
        });

        let mut config = OpenAiRealtimeTranscriptionConfig::default();
        config.endpoint = endpoint;
        config.api_key = Some("test-key".to_string());
        config.commit_timeout_secs = 5;

        let delta_text = Arc::new(Mutex::new(String::new()));
        let delta_text_for_callback = Arc::clone(&delta_text);
        let client = OpenAiRealtimeTranscriptionClient::new(config);
        let session = client
            .begin_session(TranscriptionOptions {
                on_delta: Some(Arc::new(move |delta| {
                    delta_text_for_callback
                        .lock()
                        .expect("delta lock should not be poisoned")
                        .push_str(&delta);
                })),
                ..TranscriptionOptions::default()
            })
            .expect("session should start");

        let sender = session.audio_sender();
        sender
            .append_pcm16_mono(vec![0, 1_000, -1_000, 2_000], 24_000)
            .expect("audio append should be accepted");

        let result = session
            .commit_and_wait()
            .await
            .expect("session should return transcript");
        server_task
            .await
            .expect("server task should finish without panic");

        assert_eq!(result.text, "hello world");
        assert_eq!(
            delta_text
                .lock()
                .expect("delta lock should not be poisoned")
                .as_str(),
            "hello "
        );
        let authorization_header = observed_authorization_header
            .lock()
            .expect("authorization header lock should not be poisoned")
            .clone()
            .unwrap_or_default();
        assert_eq!(authorization_header, "Bearer test-key");

        let request_uri = observed_request_uri
            .lock()
            .expect("request URI lock should not be poisoned")
            .clone()
            .unwrap_or_default();
        assert!(request_uri.contains("intent=transcription"));
        assert!(!request_uri.contains("model="));
    }
}
