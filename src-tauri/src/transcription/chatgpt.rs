use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use bytes::Bytes;
use reqwest::{multipart, Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::{
    auth_store::{now_epoch_seconds, AuthMethod, AuthStore},
    oauth,
};

use super::{
    normalize_transcript_text, TranscriptionError, TranscriptionOptions, TranscriptionProvider,
    TranscriptionResult,
};

const DEFAULT_CHATGPT_ENDPOINT: &str = "https://chatgpt.com/backend-api/transcribe";
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 180;
const CHATGPT_ACCOUNT_HEADER: &str = "ChatGPT-Account-Id";
const CODEX_BASE64_HEADER: &str = "X-Codex-Base64";
const CODEX_BASE64_HEADER_VALUE: &str = "1";

#[derive(Debug, Clone)]
pub struct ChatGptTranscriptionConfig {
    pub endpoint: String,
    pub request_timeout_secs: u64,
}

impl Default for ChatGptTranscriptionConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_CHATGPT_ENDPOINT.to_string(),
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
        }
    }
}

impl ChatGptTranscriptionConfig {
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Some(endpoint) = read_non_empty_env("CHATGPT_TRANSCRIPTION_ENDPOINT") {
            config.endpoint = endpoint;
        }

        if let Some(timeout_secs) = read_u64_env("CHATGPT_TRANSCRIPTION_TIMEOUT_SECS") {
            config.request_timeout_secs = timeout_secs.max(1);
        }

        debug!(
            endpoint = %config.endpoint,
            request_timeout_secs = config.request_timeout_secs,
            "loaded ChatGPT transcription config"
        );

        config
    }
}

#[derive(Debug, Clone)]
pub struct ChatGptTranscriptionProvider {
    client: Client,
    config: ChatGptTranscriptionConfig,
    auth_store: AuthStore,
}

#[derive(Debug, Clone)]
struct ChatGptAuthContext {
    access_token: String,
    account_id: String,
}

impl ChatGptTranscriptionProvider {
    pub fn new(config: ChatGptTranscriptionConfig, auth_store: AuthStore) -> Self {
        info!(
            endpoint = %config.endpoint,
            request_timeout_secs = config.request_timeout_secs,
            "ChatGPT transcription provider initialized"
        );

        Self {
            client: build_client(&config),
            config,
            auth_store,
        }
    }

    async fn auth_context(&self) -> Result<ChatGptAuthContext, TranscriptionError> {
        let method = self
            .auth_store
            .current_auth_method()
            .map_err(TranscriptionError::Provider)?;

        if method != AuthMethod::ChatgptOauth {
            return Err(TranscriptionError::Authentication(
                "ChatGPT OAuth login is not active".to_string(),
            ));
        }

        let Some(credentials) = self
            .auth_store
            .chatgpt_credentials()
            .map_err(TranscriptionError::Provider)?
        else {
            return Err(TranscriptionError::Authentication(
                "Missing ChatGPT OAuth credentials. Please login again.".to_string(),
            ));
        };

        if credentials.expires_at <= now_epoch_seconds().saturating_add(60) {
            warn!("ChatGPT OAuth token expired or near expiry; refreshing");
            let refreshed = oauth::refresh_access_token(&credentials.refresh_token)
                .await
                .map_err(TranscriptionError::Authentication)?;

            let refreshed_refresh_token = refreshed
                .refresh_token
                .unwrap_or(credentials.refresh_token.clone());
            let refreshed_account_id = refreshed
                .account_id
                .unwrap_or(credentials.account_id.clone());

            self.auth_store
                .update_chatgpt_tokens(
                    &refreshed.access_token,
                    &refreshed_refresh_token,
                    refreshed.expires_at,
                    &refreshed_account_id,
                )
                .map_err(TranscriptionError::Provider)?;

            return Ok(ChatGptAuthContext {
                access_token: refreshed.access_token,
                account_id: refreshed_account_id,
            });
        }

        Ok(ChatGptAuthContext {
            access_token: credentials.access_token,
            account_id: credentials.account_id,
        })
    }

    fn build_form(&self, audio_data: Vec<u8>) -> Result<multipart::Form, TranscriptionError> {
        let encoded_audio = BASE64_STANDARD.encode(Bytes::from(audio_data));
        let audio_len = u64::try_from(encoded_audio.len())
            .map_err(|_| TranscriptionError::Provider("Audio upload is too large".to_string()))?;

        let file_part = multipart::Part::stream_with_length(encoded_audio.into_bytes(), audio_len)
            .file_name("audio.wav")
            .mime_str("application/octet-stream")
            .map_err(|error| {
                TranscriptionError::Provider(format!("Unable to prepare audio upload: {error}"))
            })?;

        Ok(multipart::Form::new().part("file", file_part))
    }
}

#[async_trait]
impl TranscriptionProvider for ChatGptTranscriptionProvider {
    fn name(&self) -> &'static str {
        "chatgpt-oauth"
    }

    async fn transcribe(
        &self,
        audio_data: Vec<u8>,
        options: TranscriptionOptions,
    ) -> Result<TranscriptionResult, TranscriptionError> {
        let TranscriptionOptions {
            on_delta,
            language: _,
            prompt: _,
            context_hint: _,
        } = options;

        let auth = self.auth_context().await?;
        let form = self.build_form(audio_data)?;

        info!(endpoint = %self.config.endpoint, "starting ChatGPT transcription request");
        let response = self
            .client
            .post(&self.config.endpoint)
            .bearer_auth(auth.access_token)
            .header(CHATGPT_ACCOUNT_HEADER, auth.account_id)
            .header(CODEX_BASE64_HEADER, CODEX_BASE64_HEADER_VALUE)
            .multipart(form)
            .send()
            .await
            .map_err(map_transport_error)?;

        if !response.status().is_success() {
            return Err(map_http_error(response).await);
        }

        let payload = response
            .json::<ChatGptTranscriptionResponse>()
            .await
            .map_err(|error| {
                TranscriptionError::InvalidResponse(format!(
                    "Unable to parse ChatGPT transcription response: {error}"
                ))
            })?;

        let normalized = normalize_transcript_text(&payload.text);
        if let Some(callback) = on_delta {
            callback(normalized.clone());
        }

        Ok(TranscriptionResult {
            text: normalized,
            language: None,
            duration_secs: None,
            confidence: None,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ChatGptTranscriptionResponse {
    text: String,
}

fn map_transport_error(error: reqwest::Error) -> TranscriptionError {
    if error.is_timeout() || error.is_connect() {
        TranscriptionError::Network(error.to_string())
    } else {
        TranscriptionError::Provider(error.to_string())
    }
}

async fn map_http_error(response: reqwest::Response) -> TranscriptionError {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let message = parse_chatgpt_error_message(&body)
        .unwrap_or_else(|| format!("ChatGPT request failed with status {}", status.as_u16()));

    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            TranscriptionError::Authentication(message)
        }
        StatusCode::TOO_MANY_REQUESTS => TranscriptionError::RateLimited(message),
        StatusCode::REQUEST_TIMEOUT => TranscriptionError::Network(message),
        _ if status.is_server_error() => TranscriptionError::Network(message),
        _ => TranscriptionError::Provider(message),
    }
}

fn parse_chatgpt_error_message(raw: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(raw).ok()?;

    if let Some(message) = value
        .get("error")
        .and_then(|error| {
            error
                .as_str()
                .map(ToString::to_string)
                .or_else(|| error.get("message")?.as_str().map(ToString::to_string))
        })
        .and_then(|message| normalize_optional_string(Some(message)))
    {
        return Some(message);
    }

    if let Some(message) = value
        .get("message")
        .and_then(|message| message.as_str())
        .and_then(|message| normalize_optional_string(Some(message.to_string())))
    {
        return Some(message);
    }

    normalize_optional_string(Some(truncate_response_body(raw.to_string())))
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

fn truncate_response_body(value: String) -> String {
    let trimmed = value.trim();
    if trimmed.len() <= 300 {
        return trimmed.to_string();
    }

    format!("{}...", &trimmed[..300])
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

fn build_client(config: &ChatGptTranscriptionConfig) -> Client {
    Client::builder()
        .timeout(Duration::from_secs(config.request_timeout_secs.max(1)))
        .build()
        .expect("ChatGPT client construction should succeed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_store::AuthStore;
    use mockito::{Matcher, Server};
    use std::fs;

    fn temp_app_data_dir(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "voice-chatgpt-transcription-tests-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&path)
            .expect("temp chatgpt transcription test directory should be created");
        path
    }

    fn provider_for_test(server: &Server, auth_store: AuthStore) -> ChatGptTranscriptionProvider {
        ChatGptTranscriptionProvider::new(
            ChatGptTranscriptionConfig {
                endpoint: format!("{}/backend-api/transcribe", server.url()),
                request_timeout_secs: 5,
            },
            auth_store,
        )
    }

    #[tokio::test]
    async fn sends_required_headers_and_base64_audio() {
        let mut server = Server::new_async().await;
        let app_data_dir = temp_app_data_dir("headers");
        let auth_store = AuthStore::new(app_data_dir);
        auth_store
            .save_chatgpt_login(
                "access-token",
                "refresh-token",
                now_epoch_seconds().saturating_add(600),
                "acct_123",
            )
            .expect("oauth credentials should persist");

        let mock = server
            .mock("POST", "/backend-api/transcribe")
            .match_header("authorization", "Bearer access-token")
            .match_header("chatgpt-account-id", "acct_123")
            .match_header("x-codex-base64", "1")
            .match_body(Matcher::Regex("AQID".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"text":"  hello   world "}"#)
            .create_async()
            .await;

        let provider = provider_for_test(&server, auth_store);
        let result = provider
            .transcribe(vec![1, 2, 3], TranscriptionOptions::default())
            .await
            .expect("transcription should succeed");

        mock.assert_async().await;
        assert_eq!(result.text, "hello world");
    }

    #[tokio::test]
    async fn maps_unauthorized_errors() {
        let mut server = Server::new_async().await;
        let app_data_dir = temp_app_data_dir("auth-error");
        let auth_store = AuthStore::new(app_data_dir);
        auth_store
            .save_chatgpt_login(
                "bad-token",
                "refresh-token",
                now_epoch_seconds().saturating_add(600),
                "acct_123",
            )
            .expect("oauth credentials should persist");

        let mock = server
            .mock("POST", "/backend-api/transcribe")
            .with_status(401)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":{"message":"Token invalid"}}"#)
            .create_async()
            .await;

        let provider = provider_for_test(&server, auth_store);
        let error = provider
            .transcribe(vec![1, 2, 3], TranscriptionOptions::default())
            .await
            .expect_err("request should fail");

        mock.assert_async().await;
        assert_eq!(
            error,
            TranscriptionError::Authentication("Token invalid".to_string())
        );
    }
}
