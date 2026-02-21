use async_trait::async_trait;
use bytes::Bytes;
use reqwest::{
    header::{HeaderMap, RETRY_AFTER},
    multipart, Client, StatusCode,
};
use serde::Deserialize;
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tracing::{debug, error, info, warn};

#[cfg(not(test))]
use crate::api_key_store::ApiKeyStore;

use super::{
    normalize_transcript_text, TranscriptionDeltaCallback, TranscriptionError,
    TranscriptionOptions, TranscriptionProvider, TranscriptionResult,
};

const DEFAULT_OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini-transcribe";
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 180;
const DEFAULT_MAX_RETRIES: u32 = 3;
const DEFAULT_INITIAL_BACKOFF_MS: u64 = 500;
const DEFAULT_MAX_BACKOFF_MS: u64 = 5_000;
const STREAMING_TRANSCRIPT_DELTA_EVENT: &str = "transcript.text.delta";
const STREAMING_TRANSCRIPT_DONE_EVENT: &str = "transcript.text.done";

#[derive(Debug, Clone)]
pub struct OpenAiTranscriptionConfig {
    pub api_key: Option<String>,
    pub api_key_store_app_data_dir: Option<PathBuf>,
    pub endpoint: String,
    pub model: String,
    pub request_timeout_secs: u64,
    pub max_retries: u32,
    pub retry_initial_backoff_ms: u64,
    pub retry_max_backoff_ms: u64,
}

impl Default for OpenAiTranscriptionConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            api_key_store_app_data_dir: None,
            endpoint: DEFAULT_OPENAI_ENDPOINT.to_string(),
            model: DEFAULT_OPENAI_MODEL.to_string(),
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_initial_backoff_ms: DEFAULT_INITIAL_BACKOFF_MS,
            retry_max_backoff_ms: DEFAULT_MAX_BACKOFF_MS,
        }
    }
}

impl OpenAiTranscriptionConfig {
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Some(model) = read_non_empty_env("OPENAI_TRANSCRIPTION_MODEL") {
            config.model = model;
        }

        if let Some(endpoint) = read_non_empty_env("OPENAI_TRANSCRIPTION_ENDPOINT") {
            config.endpoint = endpoint;
        }

        if let Some(timeout_secs) = read_u64_env("OPENAI_TRANSCRIPTION_TIMEOUT_SECS") {
            config.request_timeout_secs = timeout_secs.max(1);
        }

        if let Some(max_retries) = read_u32_env("OPENAI_TRANSCRIPTION_MAX_RETRIES") {
            config.max_retries = max_retries;
        }

        if let Some(initial_backoff_ms) =
            read_u64_env("OPENAI_TRANSCRIPTION_RETRY_INITIAL_BACKOFF_MS")
        {
            config.retry_initial_backoff_ms = initial_backoff_ms.max(1);
        }

        if let Some(max_backoff_ms) = read_u64_env("OPENAI_TRANSCRIPTION_RETRY_MAX_BACKOFF_MS") {
            config.retry_max_backoff_ms = max_backoff_ms.max(1);
        }

        if config.retry_initial_backoff_ms > config.retry_max_backoff_ms {
            config.retry_initial_backoff_ms = config.retry_max_backoff_ms;
        }

        debug!(
            endpoint = %config.endpoint,
            model = %config.model,
            request_timeout_secs = config.request_timeout_secs,
            max_retries = config.max_retries,
            retry_initial_backoff_ms = config.retry_initial_backoff_ms,
            retry_max_backoff_ms = config.retry_max_backoff_ms,
            "loaded OpenAI transcription config"
        );
        config
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiTranscriptionProvider {
    client: Client,
    config: OpenAiTranscriptionConfig,
    jitter_seed: u64,
}

impl OpenAiTranscriptionProvider {
    pub fn new(config: OpenAiTranscriptionConfig) -> Self {
        Self::new_with_jitter_seed(config, seed_from_clock())
    }

    fn new_with_jitter_seed(config: OpenAiTranscriptionConfig, jitter_seed: u64) -> Self {
        info!(
            endpoint = %config.endpoint,
            model = %config.model,
            request_timeout_secs = config.request_timeout_secs,
            max_retries = config.max_retries,
            "OpenAI transcription provider initialized"
        );
        Self {
            client: build_client(&config),
            config,
            jitter_seed,
        }
    }

    fn api_key(&self) -> Result<String, TranscriptionError> {
        if let Some(explicit_key) = self
            .config
            .api_key
            .clone()
            .and_then(|value| normalize_optional_string(Some(value)))
        {
            debug!("using OpenAI API key from explicit provider configuration");
            return Ok(explicit_key);
        }

        #[cfg(not(test))]
        {
            if let Some(app_data_dir) = self.config.api_key_store_app_data_dir.clone() {
                match ApiKeyStore::new(app_data_dir).get_api_key(self.name()) {
                    Ok(Some(stored_key)) => {
                        debug!("using OpenAI API key from persisted API key file");
                        return Ok(stored_key);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        if let Some(env_key) = read_non_empty_env("OPENAI_API_KEY") {
                            warn!(
                                error = %error,
                                "falling back to OPENAI_API_KEY environment variable after API key file read failure"
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

        read_non_empty_env("OPENAI_API_KEY")
            .inspect(|_| debug!("using OpenAI API key from environment"))
            .ok_or(TranscriptionError::MissingApiKey)
    }

    fn retry_delay(&self, attempt_index: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(delay) = retry_after {
            return delay;
        }

        let growth_factor = 1_u64.checked_shl(attempt_index.min(20)).unwrap_or(u64::MAX);
        let uncapped_ms = self
            .config
            .retry_initial_backoff_ms
            .saturating_mul(growth_factor);
        let capped_ms = uncapped_ms.min(self.config.retry_max_backoff_ms).max(1);

        // Equal jitter: spread retries in [base/2, base] to reduce thundering herd while
        // retaining monotonic growth.
        let half_ms = capped_ms / 2;
        let jitter_span_ms = capped_ms.saturating_sub(half_ms);
        let jitter_offset = if jitter_span_ms == 0 {
            0
        } else {
            self.pseudo_random(attempt_index) % (jitter_span_ms + 1)
        };

        Duration::from_millis(half_ms.saturating_add(jitter_offset).max(1))
    }

    fn pseudo_random(&self, attempt_index: u32) -> u64 {
        let mut state =
            self.jitter_seed ^ (attempt_index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn model_supports_streaming(&self) -> bool {
        self.config
            .model
            .to_ascii_lowercase()
            .contains("transcribe")
    }

    fn build_form(
        &self,
        audio_data: Bytes,
        language: Option<&str>,
        prompt: Option<&str>,
        stream: bool,
    ) -> Result<multipart::Form, TranscriptionError> {
        let response_format = if stream { "text" } else { "verbose_json" };
        let mut form = multipart::Form::new()
            .text("model", self.config.model.clone())
            .text("response_format", response_format.to_string());

        if stream {
            form = form.text("stream", "true".to_string());
        }

        if let Some(language) = language {
            form = form.text("language", language.to_string());
        }

        if let Some(prompt) = prompt {
            form = form.text("prompt", prompt.to_string());
        }

        let audio_len = u64::try_from(audio_data.len())
            .map_err(|_| TranscriptionError::Provider("Audio upload is too large".to_string()))?;

        let file_part = multipart::Part::stream_with_length(audio_data, audio_len)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|error| {
                TranscriptionError::Provider(format!("Unable to prepare audio upload: {error}"))
            })?;

        Ok(form.part("file", file_part))
    }

    async fn parse_streaming_response(
        &self,
        mut response: reqwest::Response,
        request_language: Option<String>,
        on_delta: Option<&TranscriptionDeltaCallback>,
    ) -> Result<TranscriptionResult, TranscriptionError> {
        let mut stream_state = OpenAiStreamingState::default();
        let mut line_buffer = Vec::<u8>::new();

        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| map_transport_error(error).error)?
        {
            line_buffer.extend_from_slice(&chunk);
            while let Some(newline_index) = line_buffer.iter().position(|byte| *byte == b'\n') {
                let mut line_bytes = line_buffer.drain(..=newline_index).collect::<Vec<_>>();
                if line_bytes.last() == Some(&b'\n') {
                    line_bytes.pop();
                }
                if line_bytes.last() == Some(&b'\r') {
                    line_bytes.pop();
                }
                let line = String::from_utf8(line_bytes).map_err(|error| {
                    TranscriptionError::InvalidResponse(format!(
                        "OpenAI stream payload contained invalid UTF-8: {error}",
                    ))
                })?;
                stream_state.handle_line(&line, on_delta)?;
            }
        }

        if !line_buffer.is_empty() {
            let line = String::from_utf8(line_buffer).map_err(|error| {
                TranscriptionError::InvalidResponse(format!(
                    "OpenAI stream payload contained invalid UTF-8: {error}",
                ))
            })?;
            stream_state.handle_line(&line, on_delta)?;
        }

        stream_state.finalize(on_delta)?;
        let final_text = stream_state.final_text()?;

        Ok(TranscriptionResult {
            text: normalize_transcript_text(&final_text),
            language: request_language,
            duration_secs: None,
            confidence: None,
        })
    }
}

#[async_trait]
impl TranscriptionProvider for OpenAiTranscriptionProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    async fn transcribe(
        &self,
        audio_data: Vec<u8>,
        options: TranscriptionOptions,
    ) -> Result<TranscriptionResult, TranscriptionError> {
        let TranscriptionOptions {
            language,
            prompt,
            context_hint,
            on_delta,
        } = options;
        let api_key = self.api_key()?;
        let request_language = normalize_optional_string(language);
        let request_prompt = build_prompt(prompt, context_hint);
        let request_language_for_payload = request_language.clone();
        let stream_response = self.model_supports_streaming();
        let audio_data = Bytes::from(audio_data);
        let mut attempt_index = 0;
        info!(
            endpoint = %self.config.endpoint,
            model = %self.config.model,
            stream = stream_response,
            audio_bytes = audio_data.len(),
            language = ?request_language,
            has_prompt = request_prompt.is_some(),
            "starting OpenAI transcription request"
        );

        loop {
            debug!(
                attempt = attempt_index + 1,
                "sending OpenAI transcription request"
            );
            let form = self.build_form(
                audio_data.clone(),
                request_language.as_deref(),
                request_prompt.as_deref(),
                stream_response,
            )?;

            let response = self
                .client
                .post(&self.config.endpoint)
                .bearer_auth(&api_key)
                .multipart(form)
                .send()
                .await;

            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    let transport_error = map_transport_error(error);
                    if transport_error.retryable && attempt_index < self.config.max_retries {
                        let delay = self.retry_delay(attempt_index, None);
                        warn!(
                            attempt = attempt_index + 1,
                            delay_ms = delay.as_millis() as u64,
                            error = %transport_error.error,
                            "retrying OpenAI transcription request after transport error"
                        );
                        tokio::time::sleep(delay).await;
                        attempt_index += 1;
                        continue;
                    }
                    error!(
                        attempt = attempt_index + 1,
                        error = %transport_error.error,
                        "OpenAI transcription request failed without retry"
                    );
                    return Err(transport_error.error);
                }
            };

            if response.status().is_success() {
                info!(
                    attempt = attempt_index + 1,
                    "OpenAI transcription request succeeded"
                );
                if stream_response {
                    return self
                        .parse_streaming_response(
                            response,
                            request_language_for_payload.clone(),
                            on_delta.as_ref(),
                        )
                        .await;
                }

                let response_payload: OpenAiTranscriptionResponse = response
                    .json()
                    .await
                    .map_err(|error| TranscriptionError::InvalidResponse(error.to_string()))?;

                return Ok(TranscriptionResult {
                    text: normalize_transcript_text(&response_payload.text),
                    language: response_payload
                        .language
                        .or(request_language_for_payload.clone()),
                    duration_secs: response_payload.duration,
                    confidence: response_payload
                        .confidence
                        .or_else(|| derive_confidence_from_segments(&response_payload.segments)),
                });
            }

            let http_error = map_http_error(response).await;
            if http_error.retryable && attempt_index < self.config.max_retries {
                let delay = self.retry_delay(attempt_index, http_error.retry_after);
                warn!(
                    attempt = attempt_index + 1,
                    delay_ms = delay.as_millis() as u64,
                    error = %http_error.error,
                    "retrying OpenAI transcription request after HTTP error"
                );
                tokio::time::sleep(delay).await;
                attempt_index += 1;
                continue;
            }

            error!(
                attempt = attempt_index + 1,
                error = %http_error.error,
                "OpenAI transcription request failed without retry"
            );
            return Err(http_error.error);
        }
    }
}

#[derive(Debug, Default)]
struct OpenAiStreamingState {
    current_event_name: Option<String>,
    current_data_lines: Vec<String>,
    saw_event_payload: bool,
    transcript_from_deltas: String,
    transcript_done: Option<String>,
}

impl OpenAiStreamingState {
    fn handle_line(
        &mut self,
        line: &str,
        on_delta: Option<&TranscriptionDeltaCallback>,
    ) -> Result<(), TranscriptionError> {
        if line.is_empty() {
            self.flush_current_event(on_delta)?;
            return Ok(());
        }

        if line.starts_with(':') {
            return Ok(());
        }

        if let Some(event_name) = line.strip_prefix("event:") {
            self.current_event_name = normalize_optional_string(Some(event_name.to_string()));
            return Ok(());
        }

        if let Some(data) = line.strip_prefix("data:") {
            self.current_data_lines
                .push(data.trim_start_matches(' ').to_string());
        }

        Ok(())
    }

    fn finalize(
        &mut self,
        on_delta: Option<&TranscriptionDeltaCallback>,
    ) -> Result<(), TranscriptionError> {
        self.flush_current_event(on_delta)
    }

    fn final_text(self) -> Result<String, TranscriptionError> {
        if !self.saw_event_payload {
            return Err(TranscriptionError::InvalidResponse(
                "OpenAI streaming response did not contain transcript events".to_string(),
            ));
        }

        Ok(self.transcript_done.unwrap_or(self.transcript_from_deltas))
    }

    fn flush_current_event(
        &mut self,
        on_delta: Option<&TranscriptionDeltaCallback>,
    ) -> Result<(), TranscriptionError> {
        let event_name = self.current_event_name.take();
        if self.current_data_lines.is_empty() {
            return Ok(());
        }

        let data_payload = self.current_data_lines.join("\n");
        self.current_data_lines.clear();
        if data_payload.trim().is_empty() || data_payload.trim() == "[DONE]" {
            return Ok(());
        }

        let parsed_payload = serde_json::from_str::<OpenAiStreamingEventPayload>(&data_payload)
            .map_err(|error| {
                TranscriptionError::InvalidResponse(format!(
                    "Unable to parse OpenAI streaming payload: {error}",
                ))
            })?;
        self.saw_event_payload = true;

        let payload_event_name = parsed_payload
            .kind
            .and_then(|kind| normalize_optional_string(Some(kind)));
        let resolved_event = payload_event_name.or(event_name);

        match resolved_event.as_deref() {
            Some(STREAMING_TRANSCRIPT_DELTA_EVENT) => {
                if let Some(delta) = parsed_payload.delta {
                    if let Some(callback) = on_delta {
                        callback(delta.clone());
                    }
                    self.transcript_from_deltas.push_str(&delta);
                }
            }
            Some(STREAMING_TRANSCRIPT_DONE_EVENT) => {
                if let Some(text) = parsed_payload.text {
                    self.transcript_done = Some(text);
                }
            }
            _ => {}
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamingEventPayload {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiTranscriptionResponse {
    text: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    segments: Vec<OpenAiSegment>,
}

#[derive(Debug, Deserialize)]
struct OpenAiSegment {
    #[serde(default)]
    avg_logprob: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct OpenAiErrorEnvelope {
    error: OpenAiErrorBody,
}

#[derive(Debug, Deserialize)]
struct OpenAiErrorBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

fn derive_confidence_from_segments(segments: &[OpenAiSegment]) -> Option<f32> {
    let probabilities = segments
        .iter()
        .filter_map(|segment| segment.avg_logprob)
        .map(|log_prob| (log_prob as f64).exp().clamp(0.0, 1.0))
        .collect::<Vec<_>>();

    if probabilities.is_empty() {
        return None;
    }

    let avg = probabilities.iter().sum::<f64>() / probabilities.len() as f64;
    Some(avg as f32)
}

#[derive(Debug)]
struct RetryableError {
    error: TranscriptionError,
    retryable: bool,
    retry_after: Option<Duration>,
}

fn map_transport_error(error: reqwest::Error) -> RetryableError {
    let retryable = error.is_timeout() || error.is_connect();
    let mapped = if retryable {
        TranscriptionError::Network(error.to_string())
    } else {
        TranscriptionError::Provider(error.to_string())
    };

    RetryableError {
        error: mapped,
        retryable,
        retry_after: None,
    }
}

async fn map_http_error(response: reqwest::Response) -> RetryableError {
    let status = response.status();
    let retry_after = if status == StatusCode::TOO_MANY_REQUESTS {
        parse_retry_after(response.headers())
    } else {
        None
    };
    let response_body = response.text().await.unwrap_or_default();
    let fallback_message = format!("OpenAI request failed with status {}", status.as_u16());
    let error_message = parse_openai_error_message(&response_body).unwrap_or(fallback_message);
    debug!(
        status = status.as_u16(),
        retry_after_ms = retry_after.map(|d| d.as_millis() as u64),
        "mapped OpenAI HTTP error response"
    );

    let mapped = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            TranscriptionError::Authentication(error_message)
        }
        StatusCode::TOO_MANY_REQUESTS => TranscriptionError::RateLimited(error_message),
        StatusCode::REQUEST_TIMEOUT => TranscriptionError::Network(error_message),
        _ if status.is_server_error() => TranscriptionError::Network(error_message),
        _ => TranscriptionError::Provider(error_message),
    };

    RetryableError {
        retryable: status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::REQUEST_TIMEOUT
            || status.is_server_error(),
        error: mapped,
        retry_after,
    }
}

fn parse_openai_error_message(raw_body: &str) -> Option<String> {
    let parsed = serde_json::from_str::<OpenAiErrorEnvelope>(raw_body).ok()?;

    if let Some(message) = normalize_optional_string(parsed.error.message) {
        return Some(message);
    }

    normalize_optional_string(parsed.error.kind)
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

fn read_u32_env(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<u32>().ok())
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let header_value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if header_value.is_empty() {
        return None;
    }

    if let Ok(seconds) = header_value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    let retry_at = httpdate::parse_http_date(header_value).ok()?;
    let now = SystemTime::now();
    Some(
        retry_at
            .duration_since(now)
            .unwrap_or(Duration::from_secs(0)),
    )
}

fn build_client(config: &OpenAiTranscriptionConfig) -> Client {
    let timeout = Duration::from_secs(config.request_timeout_secs.max(1));
    debug!(
        timeout_secs = timeout.as_secs(),
        "building OpenAI HTTP client"
    );
    Client::builder()
        .timeout(timeout)
        .build()
        .expect("OpenAI client construction should succeed")
}

fn seed_from_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0xA5A5_A5A5_A5A5_A5A5)
}

#[cfg(test)]
mod tests {
    use mockito::{Matcher, Server};
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use super::*;

    fn config_for_test(server: &Server, api_key: Option<&str>) -> OpenAiTranscriptionConfig {
        OpenAiTranscriptionConfig {
            api_key: api_key.map(ToString::to_string),
            api_key_store_app_data_dir: None,
            endpoint: format!("{}/v1/audio/transcriptions", server.url()),
            model: "whisper-1".to_string(),
            request_timeout_secs: 5,
            max_retries: 3,
            retry_initial_backoff_ms: 10,
            retry_max_backoff_ms: 50,
        }
    }

    fn provider_with_config(config: OpenAiTranscriptionConfig) -> OpenAiTranscriptionProvider {
        OpenAiTranscriptionProvider::new_with_jitter_seed(config, 42)
    }

    fn provider_for_test(server: &Server, api_key: Option<&str>) -> OpenAiTranscriptionProvider {
        provider_with_config(config_for_test(server, api_key))
    }

    #[tokio::test]
    async fn returns_transcription_payload_for_success_response() {
        let mut server = Server::new_async().await;

        let request_mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .match_header("authorization", "Bearer test-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "text": "  hello    world\nfrom   whisper  ",
                    "language": "en",
                    "duration": 2.4,
                    "segments": [
                        { "avg_logprob": -0.2 },
                        { "avg_logprob": -0.1 }
                    ]
                }"#,
            )
            .create_async()
            .await;

        let provider = provider_for_test(&server, Some("test-key"));
        let result = provider
            .transcribe(
                vec![1, 2, 3, 4],
                TranscriptionOptions {
                    language: None,
                    prompt: Some("voice memo".to_string()),
                    context_hint: Some("meeting notes".to_string()),
                    ..TranscriptionOptions::default()
                },
            )
            .await
            .expect("request should succeed");

        request_mock.assert_async().await;
        assert_eq!(result.text, "hello world from whisper");
        assert_eq!(result.language.as_deref(), Some("en"));
        assert_eq!(result.duration_secs, Some(2.4));
        assert!(result.confidence.is_some());
    }

    #[tokio::test]
    async fn streams_deltas_and_returns_done_payload_for_transcribe_models() {
        let mut server = Server::new_async().await;
        let stream_mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .match_header("authorization", "Bearer test-key")
            .match_body(Matcher::Regex(r#"name="stream"\r\n\r\ntrue"#.to_string()))
            .match_body(Matcher::Regex(
                r#"name="response_format"\r\n\r\ntext"#.to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(
                "event: transcript.text.delta\n\
                 data: {\"type\":\"transcript.text.delta\",\"delta\":\"Hello\"}\n\n\
                 event: transcript.text.delta\n\
                 data: {\"type\":\"transcript.text.delta\",\"delta\":\" world\"}\n\n\
                 event: transcript.text.done\n\
                 data: {\"type\":\"transcript.text.done\",\"text\":\"Hello world\"}\n\n",
            )
            .create_async()
            .await;

        let mut config = config_for_test(&server, Some("test-key"));
        config.model = "gpt-4o-mini-transcribe".to_string();
        let provider = provider_with_config(config);
        let deltas = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured_deltas = Arc::clone(&deltas);
        let options = TranscriptionOptions {
            on_delta: Some(Arc::new(move |delta| {
                captured_deltas
                    .lock()
                    .expect("delta lock should not be poisoned")
                    .push(delta);
            })),
            ..TranscriptionOptions::default()
        };

        let result = provider
            .transcribe(vec![1, 2, 3], options)
            .await
            .expect("streaming transcription should succeed");

        stream_mock.assert_async().await;
        assert_eq!(result.text, "Hello world");
        assert_eq!(result.language, None);
        assert_eq!(result.duration_secs, None);
        assert_eq!(result.confidence, None);
        assert_eq!(
            deltas
                .lock()
                .expect("delta lock should not be poisoned")
                .clone(),
            vec!["Hello".to_string(), " world".to_string()]
        );
    }

    #[tokio::test]
    async fn returns_authentication_error_for_unauthorized_response() {
        let mut server = Server::new_async().await;
        let request_mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .with_status(401)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":{"message":"Invalid API key"}} "#)
            .create_async()
            .await;

        let provider = provider_for_test(&server, Some("bad-key"));
        let error = provider
            .transcribe(vec![1, 2, 3], TranscriptionOptions::default())
            .await
            .expect_err("request should fail");

        request_mock.assert_async().await;
        assert_eq!(
            error,
            TranscriptionError::Authentication("Invalid API key".to_string())
        );
    }

    #[tokio::test]
    async fn retries_server_errors_then_returns_success() {
        let mut server = Server::new_async().await;
        let server_error_mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .expect(1)
            .with_status(500)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":{"message":"Service unavailable"}} "#)
            .create_async()
            .await;
        let success_mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .expect(1)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"text":"hello retry"}"#)
            .create_async()
            .await;

        let mut config = config_for_test(&server, Some("test-key"));
        config.max_retries = 2;
        config.retry_initial_backoff_ms = 80;
        config.retry_max_backoff_ms = 80;
        let provider = provider_with_config(config);

        let started_at = Instant::now();
        let result = provider
            .transcribe(vec![1, 2, 3], TranscriptionOptions::default())
            .await
            .expect("request should succeed");
        let elapsed = started_at.elapsed();

        server_error_mock.assert_async().await;
        success_mock.assert_async().await;
        assert_eq!(result.text, "hello retry");
        assert!(
            elapsed >= Duration::from_millis(35),
            "elapsed {elapsed:?} should include retry backoff",
        );
    }

    #[tokio::test]
    async fn retries_rate_limited_responses_until_retry_limit() {
        let mut server = Server::new_async().await;
        let rate_limited_mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .expect(3)
            .with_status(429)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":{"message":"Rate limit exceeded"}} "#)
            .create_async()
            .await;

        let mut config = config_for_test(&server, Some("test-key"));
        config.max_retries = 2;
        config.retry_initial_backoff_ms = 80;
        config.retry_max_backoff_ms = 80;
        let provider = provider_with_config(config);

        let started_at = Instant::now();
        let error = provider
            .transcribe(vec![1, 2, 3], TranscriptionOptions::default())
            .await
            .expect_err("request should fail");
        let elapsed = started_at.elapsed();

        rate_limited_mock.assert_async().await;
        assert_eq!(
            error,
            TranscriptionError::RateLimited("Rate limit exceeded".to_string())
        );
        assert!(
            elapsed >= Duration::from_millis(70),
            "elapsed {elapsed:?} should include two retry delays",
        );
    }

    #[tokio::test]
    async fn honors_retry_after_header_for_rate_limit_responses() {
        let mut server = Server::new_async().await;
        let rate_limited_mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .expect(1)
            .with_status(429)
            .with_header("retry-after", "1")
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":{"message":"Rate limit exceeded"}} "#)
            .create_async()
            .await;
        let success_mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .expect(1)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"text":"retry-after honored"}"#)
            .create_async()
            .await;

        let mut config = config_for_test(&server, Some("test-key"));
        config.max_retries = 1;
        config.retry_initial_backoff_ms = 1;
        config.retry_max_backoff_ms = 1;
        let provider = provider_with_config(config);

        let started_at = Instant::now();
        let result = provider
            .transcribe(vec![1, 2, 3], TranscriptionOptions::default())
            .await
            .expect("request should succeed after retry");
        let elapsed = started_at.elapsed();

        rate_limited_mock.assert_async().await;
        success_mock.assert_async().await;
        assert_eq!(result.text, "retry-after honored");
        assert!(
            elapsed >= Duration::from_millis(900),
            "elapsed {elapsed:?} should include retry-after delay",
        );
    }
}
