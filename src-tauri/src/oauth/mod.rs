use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::{timeout, Duration},
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::auth_store::now_epoch_seconds;

const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OAUTH_SCOPE: &str = "openid profile email offline_access";
const JWT_AUTH_CLAIM_PATH: &str = "https://api.openai.com/auth";
const CALLBACK_PATH: &str = "/auth/callback";
const OAUTH_CALLBACK_BIND_HOST: &str = "127.0.0.1";
const OAUTH_CALLBACK_BIND_PORT: u16 = 1455;
const OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const OAUTH_TIMEOUT_SECS: u64 = 300;
const CHATGPT_HOME_URL: &str = "https://chatgpt.com/";
pub const CHATGPT_AUTH_WINDOW_LABEL: &str = "chatgpt-auth";
const OAUTH_ORIGINATOR: &str = "pi";

const SUCCESS_HTML: &str = "<!doctype html>\
<html lang=\"en\">\
<head><meta charset=\"utf-8\"><title>Authentication successful</title></head>\
<body><p>Authentication successful. Return to Voice to continue.</p></body>\
</html>";

#[derive(Debug, Clone)]
pub struct OAuthLoginResult {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub account_id: String,
}

#[derive(Debug, Clone)]
pub struct OAuthRefreshResult {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
    pub account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Debug)]
struct CallbackPayload {
    code: String,
}

pub async fn start_chatgpt_login<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<OAuthLoginResult, String> {
    let listener = TcpListener::bind((OAUTH_CALLBACK_BIND_HOST, OAUTH_CALLBACK_BIND_PORT))
        .await
        .map_err(|error| {
            format!(
                "Failed to bind local OAuth callback server on {}:{}: {error}",
                OAUTH_CALLBACK_BIND_HOST, OAUTH_CALLBACK_BIND_PORT
            )
        })?;
    let redirect_uri = OAUTH_REDIRECT_URI;

    let code_verifier = generate_code_verifier();
    let code_challenge = generate_code_challenge(&code_verifier);
    let state = generate_state();

    let authorize_url = build_authorize_url(redirect_uri, &state, &code_challenge)?;
    info!(
        callback_bind_host = OAUTH_CALLBACK_BIND_HOST,
        callback_bind_port = OAUTH_CALLBACK_BIND_PORT,
        redirect_uri = %redirect_uri,
        "starting ChatGPT OAuth login flow"
    );

    open_authorize_url_in_system_browser(app, &authorize_url)?;

    let callback = wait_for_callback(listener, &state).await?;
    let token_response =
        exchange_authorization_code(&callback.code, &code_verifier, redirect_uri).await?;

    let refresh_token = normalize_required_string(token_response.refresh_token, "refresh_token")?;
    let account_id = extract_chatgpt_account_id(&token_response.access_token)
        .ok_or_else(|| "OAuth token did not include a ChatGPT account id claim".to_string())?;

    seed_auth_window_after_login(app);

    Ok(OAuthLoginResult {
        access_token: token_response.access_token,
        refresh_token,
        expires_at: now_epoch_seconds().saturating_add(token_response.expires_in),
        account_id,
    })
}

pub async fn refresh_access_token(refresh_token: &str) -> Result<OAuthRefreshResult, String> {
    let normalized_refresh_token =
        normalize_required_string(Some(refresh_token.to_string()), "refresh_token")?;

    let response = reqwest::Client::new()
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", normalized_refresh_token.as_str()),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|error| format!("Failed to refresh ChatGPT OAuth token: {error}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let response_body = response.text().await.unwrap_or_default();
        return Err(format!(
            "ChatGPT OAuth token refresh failed with status {}: {}",
            status.as_u16(),
            truncate_response_body(response_body)
        ));
    }

    let token_response = response
        .json::<OAuthTokenResponse>()
        .await
        .map_err(|error| format!("Failed to parse ChatGPT OAuth refresh response: {error}"))?;

    let account_id = extract_chatgpt_account_id(&token_response.access_token);

    Ok(OAuthRefreshResult {
        access_token: token_response.access_token,
        refresh_token: token_response
            .refresh_token
            .and_then(|value| normalize_optional_string(Some(value))),
        expires_at: now_epoch_seconds().saturating_add(token_response.expires_in),
        account_id,
    })
}

pub fn extract_chatgpt_account_id(access_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(access_token)?;
    payload
        .get(JWT_AUTH_CLAIM_PATH)
        .and_then(|value| value.get("chatgpt_account_id"))
        .and_then(|value| value.as_str())
        .and_then(|value| normalize_optional_string(Some(value.to_string())))
}

fn build_authorize_url(
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> Result<Url, String> {
    let mut authorize_url = Url::parse(AUTHORIZE_URL)
        .map_err(|error| format!("Invalid OAuth authorize URL: {error}"))?;

    authorize_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", OAUTH_SCOPE)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", OAUTH_ORIGINATOR);

    Ok(authorize_url)
}

fn open_authorize_url_in_system_browser<R: Runtime>(
    app: &AppHandle<R>,
    authorize_url: &Url,
) -> Result<(), String> {
    app.opener()
        .open_url(authorize_url.as_str(), None::<&str>)
        .map_err(|error| format!("Failed to open OAuth URL in system browser: {error}"))
}

fn seed_auth_window_after_login<R: Runtime>(app: &AppHandle<R>) {
    let window = match ensure_auth_window(app) {
        Ok(window) => window,
        Err(error) => {
            warn!(%error, "failed to create ChatGPT auth webview after OAuth login");
            return;
        }
    };

    if let Ok(url) = Url::parse(CHATGPT_HOME_URL) {
        if let Err(error) = window.navigate(url) {
            warn!(%error, "failed to navigate ChatGPT auth webview to chatgpt.com");
        }
    }

    if let Err(error) = window.hide() {
        warn!(%error, "failed to hide ChatGPT auth webview after OAuth login");
    }
}

fn ensure_auth_window<R: Runtime>(app: &AppHandle<R>) -> Result<WebviewWindow<R>, String> {
    if let Some(window) = app.get_webview_window(CHATGPT_AUTH_WINDOW_LABEL) {
        return Ok(window);
    }

    let initial_url = Url::parse(CHATGPT_HOME_URL)
        .map_err(|error| format!("Invalid ChatGPT warmup URL: {error}"))?;

    WebviewWindowBuilder::new(
        app,
        CHATGPT_AUTH_WINDOW_LABEL,
        WebviewUrl::External(initial_url),
    )
    .title("Login with ChatGPT")
    .inner_size(980.0, 760.0)
    .min_inner_size(700.0, 520.0)
    .resizable(true)
    .visible(false)
    .build()
    .map_err(|error| format!("Failed to create ChatGPT auth webview: {error}"))
}

async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
) -> Result<CallbackPayload, String> {
    let wait_for_callback = async {
        loop {
            let (mut stream, _) = listener
                .accept()
                .await
                .map_err(|error| format!("Failed to accept OAuth callback connection: {error}"))?;

            let request = read_request_head(&mut stream).await?;
            let Ok((method, request_target)) = parse_request_line(&request) else {
                let _ =
                    respond_plain_text(&mut stream, "400 Bad Request", "Malformed request").await;
                continue;
            };

            if method != "GET" {
                let _ =
                    respond_plain_text(&mut stream, "405 Method Not Allowed", "Method not allowed")
                        .await;
                continue;
            }

            let callback_url = Url::parse(&format!("http://localhost{request_target}"))
                .map_err(|error| format!("Failed to parse OAuth callback URL: {error}"))?;

            if callback_url.path() != CALLBACK_PATH {
                let _ = respond_plain_text(&mut stream, "404 Not Found", "Not found").await;
                continue;
            }

            let callback_state = callback_url
                .query_pairs()
                .find_map(|(key, value)| (key == "state").then_some(value.into_owned()));
            if callback_state.as_deref() != Some(expected_state) {
                let _ = respond_plain_text(&mut stream, "400 Bad Request", "State mismatch").await;
                return Err("OAuth state mismatch from callback".to_string());
            }

            let code = callback_url
                .query_pairs()
                .find_map(|(key, value)| (key == "code").then_some(value.into_owned()))
                .and_then(|value| normalize_optional_string(Some(value)))
                .ok_or_else(|| {
                    "OAuth callback did not include an authorization code".to_string()
                })?;

            let _ = respond_html(&mut stream, "200 OK", SUCCESS_HTML).await;
            return Ok(CallbackPayload { code });
        }
    };

    timeout(Duration::from_secs(OAUTH_TIMEOUT_SECS), wait_for_callback)
        .await
        .map_err(|_| "Timed out waiting for OAuth browser callback".to_string())?
}

async fn exchange_authorization_code(
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthTokenResponse, String> {
    let response = reqwest::Client::new()
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", code_verifier),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|error| format!("Failed to exchange OAuth code for tokens: {error}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let response_body = response.text().await.unwrap_or_default();
        return Err(format!(
            "OAuth code exchange failed with status {}: {}",
            status.as_u16(),
            truncate_response_body(response_body)
        ));
    }

    response
        .json::<OAuthTokenResponse>()
        .await
        .map_err(|error| format!("Failed to parse OAuth token response: {error}"))
}

fn generate_code_verifier() -> String {
    let mut bytes = [0_u8; 32];
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    bytes[..16].copy_from_slice(first.as_bytes());
    bytes[16..].copy_from_slice(second.as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

fn generate_code_challenge(code_verifier: &str) -> String {
    let digest = Sha256::digest(code_verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn generate_state() -> String {
    Uuid::new_v4().simple().to_string()
}

fn decode_jwt_payload(token: &str) -> Option<Value> {
    let payload_segment = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload_segment).ok().or_else(|| {
        let padded = add_base64_padding(payload_segment);
        URL_SAFE_NO_PAD.decode(padded).ok()
    })?;

    serde_json::from_slice::<Value>(&decoded).ok()
}

fn add_base64_padding(input: &str) -> String {
    let missing = (4 - (input.len() % 4)) % 4;
    format!("{input}{}", "=".repeat(missing))
}

async fn read_request_head(stream: &mut tokio::net::TcpStream) -> Result<String, String> {
    let mut buffer = Vec::<u8>::with_capacity(1024);
    let mut chunk = [0_u8; 1024];

    loop {
        let bytes_read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| format!("Failed to read OAuth callback request: {error}"))?;
        if bytes_read == 0 {
            break;
        }

        buffer.extend_from_slice(&chunk[..bytes_read]);

        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }

        if buffer.len() > 16 * 1024 {
            warn!("OAuth callback request exceeded max read size");
            break;
        }
    }

    String::from_utf8(buffer)
        .map_err(|error| format!("OAuth callback request was not valid UTF-8: {error}"))
}

fn parse_request_line(request: &str) -> Result<(&str, &str), String> {
    let request_line = request
        .lines()
        .next()
        .ok_or_else(|| "Missing HTTP request line".to_string())?;

    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "Missing HTTP method".to_string())?;
    let target = parts
        .next()
        .ok_or_else(|| "Missing HTTP request target".to_string())?;

    Ok((method, target))
}

async fn respond_html(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> Result<(), String> {
    respond(stream, status, "text/html; charset=utf-8", body).await
}

async fn respond_plain_text(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> Result<(), String> {
    respond(stream, status, "text/plain; charset=utf-8", body).await
}

async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<(), String> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|error| format!("Failed to write OAuth callback response: {error}"))?;

    let _ = stream.shutdown().await;
    Ok(())
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn normalize_required_string(value: Option<String>, field_name: &str) -> Result<String, String> {
    normalize_optional_string(value).ok_or_else(|| format!("Missing required `{field_name}` value"))
}

fn truncate_response_body(value: String) -> String {
    let trimmed = value.trim();
    if trimmed.len() <= 300 {
        return trimmed.to_string();
    }

    format!("{}...", &trimmed[..300])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    #[test]
    fn extracts_chatgpt_account_id_from_jwt_claims() {
        let payload = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_123"
            }
        });
        let encoded_payload = URL_SAFE_NO_PAD.encode(payload.to_string());
        let token = format!("header.{encoded_payload}.signature");

        assert_eq!(
            extract_chatgpt_account_id(&token).as_deref(),
            Some("acct_123")
        );
    }

    #[test]
    fn parse_request_line_requires_method_and_target() {
        assert!(parse_request_line("GET /auth/callback HTTP/1.1\r\nHost: localhost").is_ok());
        assert!(parse_request_line("BROKEN").is_err());
    }

    #[test]
    fn code_verifier_and_challenge_are_url_safe() {
        let verifier = generate_code_verifier();
        let challenge = generate_code_challenge(&verifier);

        assert!(verifier.len() >= 43);
        assert!(challenge.len() >= 43);
        assert!(!verifier.contains('='));
        assert!(!challenge.contains('='));
    }

    #[test]
    fn authorize_url_contains_expected_openai_codex_query_parameters() {
        let state = "state_123";
        let challenge = "challenge_123";

        let url = build_authorize_url(OAUTH_REDIRECT_URI, state, challenge).expect("authorize url");
        let query: HashMap<String, String> = url.query_pairs().into_owned().collect();

        assert_eq!(
            url.as_str(),
            "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&scope=openid+profile+email+offline_access&code_challenge=challenge_123&code_challenge_method=S256&state=state_123&id_token_add_organizations=true&codex_cli_simplified_flow=true&originator=pi"
        );
        assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(query.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(
            query.get("redirect_uri").map(String::as_str),
            Some(OAUTH_REDIRECT_URI)
        );
        assert_eq!(query.get("scope").map(String::as_str), Some(OAUTH_SCOPE));
        assert_eq!(query.get("state").map(String::as_str), Some(state));
        assert_eq!(
            query.get("code_challenge").map(String::as_str),
            Some(challenge)
        );
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            query.get("id_token_add_organizations").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            query.get("codex_cli_simplified_flow").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            query.get("originator").map(String::as_str),
            Some(OAUTH_ORIGINATOR)
        );
    }

    #[tokio::test]
    async fn callback_listener_accepts_code_and_matching_state() {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind callback listener");
        let port = listener.local_addr().expect("listener address").port();

        let callback_task =
            tokio::spawn(async move { wait_for_callback(listener, "expected_state").await });

        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect to callback listener");
        let request = "GET /auth/callback?code=test_code&state=expected_state HTTP/1.1\r\n\
Host: localhost\r\n\
Connection: close\r\n\
\r\n";
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write callback request");

        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("read callback response");
        assert!(response.contains("200 OK"));

        let payload = callback_task
            .await
            .expect("callback task join")
            .expect("callback payload");
        assert_eq!(payload.code, "test_code");
    }
}
