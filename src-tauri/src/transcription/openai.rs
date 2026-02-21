use async_trait::async_trait;
use reqwest::{multipart, Client, StatusCode};
use serde::Deserialize;

use super::{
    normalize_transcript_text, TranscriptionError, TranscriptionOptions, TranscriptionProvider,
    TranscriptionResult,
};

const DEFAULT_OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";
const DEFAULT_OPENAI_MODEL: &str = "whisper-1";

#[derive(Debug, Clone)]
pub struct OpenAiTranscriptionConfig {
    pub api_key: Option<String>,
    pub endpoint: String,
    pub model: String,
}

impl Default for OpenAiTranscriptionConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            endpoint: DEFAULT_OPENAI_ENDPOINT.to_string(),
            model: DEFAULT_OPENAI_MODEL.to_string(),
        }
    }
}

impl OpenAiTranscriptionConfig {
    pub fn from_env() -> Self {
        let mut config = Self::default();
        config.api_key = read_non_empty_env("OPENAI_API_KEY");

        if let Some(model) = read_non_empty_env("OPENAI_TRANSCRIPTION_MODEL") {
            config.model = model;
        }

        if let Some(endpoint) = read_non_empty_env("OPENAI_TRANSCRIPTION_ENDPOINT") {
            config.endpoint = endpoint;
        }

        config
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiTranscriptionProvider {
    client: Client,
    config: OpenAiTranscriptionConfig,
}

impl OpenAiTranscriptionProvider {
    pub fn new(config: OpenAiTranscriptionConfig) -> Self {
        Self {
            client: Client::new(),
            config,
        }
    }

    fn api_key(&self) -> Result<&str, TranscriptionError> {
        self.config
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(TranscriptionError::MissingApiKey)
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
        let api_key = self.api_key()?.to_string();
        let request_language = normalize_optional_string(options.language.clone());
        let request_prompt = build_prompt(options.prompt, options.context_hint);

        let mut form = multipart::Form::new()
            .text("model", self.config.model.clone())
            .text("response_format", "verbose_json".to_string());

        if let Some(language) = request_language.clone() {
            form = form.text("language", language);
        }

        if let Some(prompt) = request_prompt {
            form = form.text("prompt", prompt);
        }

        let file_part = multipart::Part::bytes(audio_data)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|error| {
                TranscriptionError::Provider(format!("Unable to prepare audio upload: {error}"))
            })?;

        form = form.part("file", file_part);

        let response = self
            .client
            .post(&self.config.endpoint)
            .bearer_auth(api_key)
            .multipart(form)
            .send()
            .await
            .map_err(map_transport_error)?;

        let status = response.status();
        if !status.is_success() {
            return Err(map_http_error(status, response).await);
        }

        let response_payload: OpenAiTranscriptionResponse = response
            .json()
            .await
            .map_err(|error| TranscriptionError::InvalidResponse(error.to_string()))?;

        Ok(TranscriptionResult {
            text: normalize_transcript_text(&response_payload.text),
            language: response_payload.language.or(request_language),
            duration_secs: response_payload.duration,
            confidence: response_payload
                .confidence
                .or_else(|| derive_confidence_from_segments(&response_payload.segments)),
        })
    }
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

fn map_transport_error(error: reqwest::Error) -> TranscriptionError {
    if error.is_timeout() || error.is_connect() {
        return TranscriptionError::Network(error.to_string());
    }

    TranscriptionError::Provider(error.to_string())
}

async fn map_http_error(status: StatusCode, response: reqwest::Response) -> TranscriptionError {
    let response_body = response.text().await.unwrap_or_default();
    let fallback_message = format!("OpenAI request failed with status {}", status.as_u16());
    let error_message = parse_openai_error_message(&response_body).unwrap_or(fallback_message);

    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            TranscriptionError::Authentication(error_message)
        }
        StatusCode::TOO_MANY_REQUESTS => TranscriptionError::RateLimited(error_message),
        StatusCode::REQUEST_TIMEOUT
        | StatusCode::BAD_GATEWAY
        | StatusCode::SERVICE_UNAVAILABLE
        | StatusCode::GATEWAY_TIMEOUT => TranscriptionError::Network(error_message),
        _ => TranscriptionError::Provider(error_message),
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

#[cfg(test)]
mod tests {
    use mockito::Server;

    use super::*;

    fn provider_for_test(server: &Server, api_key: Option<&str>) -> OpenAiTranscriptionProvider {
        OpenAiTranscriptionProvider::new(OpenAiTranscriptionConfig {
            api_key: api_key.map(ToString::to_string),
            endpoint: format!("{}/v1/audio/transcriptions", server.url()),
            model: "whisper-1".to_string(),
        })
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
    async fn returns_rate_limit_error_for_429_response() {
        let mut server = Server::new_async().await;
        let request_mock = server
            .mock("POST", "/v1/audio/transcriptions")
            .with_status(429)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":{"message":"Rate limit exceeded"}} "#)
            .create_async()
            .await;

        let provider = provider_for_test(&server, Some("test-key"));
        let error = provider
            .transcribe(vec![1, 2, 3], TranscriptionOptions::default())
            .await
            .expect_err("request should fail");

        request_mock.assert_async().await;
        assert_eq!(
            error,
            TranscriptionError::RateLimited("Rate limit exceeded".to_string())
        );
    }

    #[tokio::test]
    async fn returns_missing_api_key_when_not_configured() {
        let server = Server::new_async().await;
        let provider = provider_for_test(&server, None);

        let error = provider
            .transcribe(vec![1, 2, 3], TranscriptionOptions::default())
            .await
            .expect_err("request should fail");

        assert_eq!(error, TranscriptionError::MissingApiKey);
    }
}
