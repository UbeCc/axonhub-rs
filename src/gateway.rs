//! HTTP surface and upstream forwarding.
//!
//! The hot path is deliberately simple: read the body once, find the top-level
//! `model`, pick a channel, forward the original bytes (rewriting only the
//! `model` value), and stream the upstream response back untouched.  No
//! request/response bodies are stored, parsed more than once, or written to a
//! database.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::TryStreamExt;
use serde_json::{json, Value};

use crate::config::{AuthHeader, Channel, Config};
use crate::metrics::Metrics;
use crate::model::{extract_model, replace_model};
use crate::stream_translate;
use crate::translate;

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub struct AppState {
    pub config: ArcSwap<Config>,
    pub config_path: PathBuf,
    pub client: reqwest::Client,
    pub metrics: Arc<Metrics>,
    pub started: Instant,
    pub access_log: parking_lot::Mutex<Option<std::fs::File>>,
}

impl AppState {
    pub fn new(config: Arc<Config>, config_path: PathBuf) -> Arc<Self> {
        let client = build_client(&config);
        let access_log = config.access_log.as_ref().and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
        Arc::new(Self {
            config: ArcSwap::from(config),
            config_path,
            client,
            metrics: Arc::new(Metrics::default()),
            started: Instant::now(),
            access_log: parking_lot::Mutex::new(access_log),
        })
    }

    pub fn snapshot(&self) -> Arc<Config> {
        self.config.load_full()
    }

    pub fn reload(&self) -> anyhow::Result<()> {
        let config = Config::load(&self.config_path)?;
        self.config.store(config);
        Ok(())
    }
}

fn build_client(config: &Config) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(64)
        .tcp_nodelay(true)
        // HTTP/1.1 only: long-lived multiplexed h2 connections to the
        // upstreams wedge under load (kernel Recv-Q grows, the connection
        // stops being read, and every request multiplexed on it hangs,
        // including brand-new ones that reuse it from the pool). On h1 a
        // wedged stream only blocks itself. Revisit only with a reproducer.
        .http1_only()
        .build()
        .expect("build upstream HTTP client")
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/anthropic/v1/messages", post(handle_anthropic))
        .route("/anthropic/v1/messages/", post(handle_anthropic))
        .route("/v1/messages", post(handle_anthropic))
        .route("/anthropic/v1/models", get(handle_models))
        .route("/v1/models", get(handle_models))
        .route("/v1/chat/completions", post(handle_openai))
        .route("/v1/chat/completions/", post(handle_openai))
        .route("/health", get(handle_health))
        .route("/healthz", get(handle_health))
        .route("/admin/system/status", get(handle_health))
        .route("/admin/status", get(handle_admin_status))
        .route("/admin/routes", get(handle_admin_routes))
        .route("/admin/reload", post(handle_reload))
        .route("/metrics", get(handle_metrics))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Anthropic Messages pass-through
// ---------------------------------------------------------------------------

async fn handle_anthropic(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let config = state.snapshot();
    if let Err(error) = authorize(&config, &headers) {
        return auth_error(error);
    }
    let Some(model) = extract_model(&body) else {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "missing model",
        );
    };
    let routes = config.routes(&model);
    if routes.is_empty() {
        return anthropic_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request_error",
            &format!("model not found: {model}"),
        );
    }

    let mut last_error: Option<(StatusCode, Bytes)> = None;
    let attempts = routes.len().min(config.max_attempts.max(1));
    for (channel, route) in routes.iter().take(attempts) {
        let body = if route.upstream_model == model {
            body.clone()
        } else {
            replace_model(&body, &route.upstream_model)
        };
        match send_anthropic(&state, &config, channel, &headers, body).await {
            Ok(response) => {
                let status = response.status().as_u16();
                state
                    .metrics
                    .record(&channel.name, status, started.elapsed().as_millis() as u64);
                access_log(
                    &state,
                    &model,
                    &channel.name,
                    status,
                    started.elapsed(),
                    true,
                );
                return stream_response(response, channel);
            }
            Err(UpstreamFailure::Retry(status, error_body)) => {
                state
                    .metrics
                    .upstream_errors
                    .fetch_add(1, Ordering::Relaxed);
                last_error = Some((status, error_body));
            }
            Err(UpstreamFailure::Fatal(status, error_body)) => {
                state.metrics.record(
                    &channel.name,
                    status.as_u16(),
                    started.elapsed().as_millis() as u64,
                );
                access_log(
                    &state,
                    &model,
                    &channel.name,
                    status.as_u16(),
                    started.elapsed(),
                    true,
                );
                return response_with_body(status, error_body, channel);
            }
        }
    }
    let (status, body) = last_error.unwrap_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            Bytes::from_static(b"{\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"all upstream channels failed\"}}"),
        )
    });
    state.metrics.errors.fetch_add(1, Ordering::Relaxed);
    state.metrics.record(
        "none",
        status.as_u16(),
        started.elapsed().as_millis() as u64,
    );
    access_log(
        &state,
        &model,
        "none",
        status.as_u16(),
        started.elapsed(),
        true,
    );
    response_with_body(status, body, &config.channels[0])
}

enum UpstreamFailure {
    Retry(StatusCode, Bytes),
    Fatal(StatusCode, Bytes),
}

async fn send_anthropic(
    state: &AppState,
    config: &Config,
    channel: &Channel,
    inbound: &HeaderMap,
    body: Bytes,
) -> Result<reqwest::Response, UpstreamFailure> {
    let url = format!("{}/v1/messages", channel.base_url.trim_end_matches('/'));
    let headers = upstream_headers(inbound, channel);
    let request = state
        .client
        .post(&url)
        .headers(headers)
        .timeout(Duration::from_secs(config.request_timeout_secs))
        .body(body);
    match request.send().await {
        Ok(response) if response.status().is_success() => Ok(response),
        Ok(response) => {
            let status = response.status();
            let body = read_limited(response).await;
            if retryable(status) {
                Err(UpstreamFailure::Retry(status, body))
            } else {
                Err(UpstreamFailure::Fatal(status, body))
            }
        }
        Err(error) => Err(UpstreamFailure::Retry(
            StatusCode::BAD_GATEWAY,
            Bytes::from(
                json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": format!("upstream {} unreachable: {error}", channel.name),
                    }
                })
                .to_string(),
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// OpenAI Chat Completions -> Anthropic Messages
// ---------------------------------------------------------------------------

async fn handle_openai(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let config = state.snapshot();
    if let Err(error) = authorize(&config, &headers) {
        return auth_error(error);
    }
    let Some(model) = translate::openai_model(&body) else {
        return openai_error(StatusCode::BAD_REQUEST, "missing model");
    };
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return openai_error(StatusCode::BAD_REQUEST, &format!("invalid JSON: {error}"))
        }
    };
    let stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let include_usage = request
        .get("stream_options")
        .and_then(|options| options.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let routes = config.routes(&model);
    if routes.is_empty() {
        return openai_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("model not found: {model}"),
        );
    }

    let mut last_error: Option<(StatusCode, Bytes)> = None;
    let attempts = routes.len().min(config.max_attempts.max(1));
    for (channel, route) in routes.iter().take(attempts) {
        let translated = match translate::openai_to_anthropic(
            &body,
            &route.upstream_model,
            config.default_max_tokens,
        ) {
            Ok(value) => value,
            Err(message) => return openai_error(StatusCode::BAD_REQUEST, &message),
        };
        let translated_body = Bytes::from(translated.to_string());
        let mut outbound = headers.clone();
        if stream {
            outbound.insert(
                header::ACCEPT,
                HeaderValue::from_static("text/event-stream"),
            );
        }
        match send_anthropic(&state, &config, channel, &outbound, translated_body).await {
            Ok(response) => {
                let status = response.status().as_u16();
                state
                    .metrics
                    .record(&channel.name, status, started.elapsed().as_millis() as u64);
                access_log(
                    &state,
                    &model,
                    &channel.name,
                    status,
                    started.elapsed(),
                    stream,
                );
                if stream {
                    return openai_stream_response(response, &model, include_usage);
                }
                let bytes = match response.bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        return openai_error(
                            StatusCode::BAD_GATEWAY,
                            &format!("reading upstream: {error}"),
                        )
                    }
                };
                let value: Value = match serde_json::from_slice(&bytes) {
                    Ok(value) => value,
                    Err(_) => return response_with_body(StatusCode::BAD_GATEWAY, bytes, channel),
                };
                let converted = stream_translate::convert_response(&value, &model);
                return json_response(StatusCode::OK, converted);
            }
            Err(UpstreamFailure::Retry(status, error_body)) => {
                state
                    .metrics
                    .upstream_errors
                    .fetch_add(1, Ordering::Relaxed);
                last_error = Some((status, error_body));
            }
            Err(UpstreamFailure::Fatal(status, error_body)) => {
                state.metrics.record(
                    &channel.name,
                    status.as_u16(),
                    started.elapsed().as_millis() as u64,
                );
                access_log(
                    &state,
                    &model,
                    &channel.name,
                    status.as_u16(),
                    started.elapsed(),
                    stream,
                );
                return response_with_body(status, error_body, channel);
            }
        }
    }
    let (status, body) =
        last_error.unwrap_or_else(|| (StatusCode::BAD_GATEWAY, Bytes::from_static(b"{}")));
    state.metrics.errors.fetch_add(1, Ordering::Relaxed);
    state.metrics.record(
        "none",
        status.as_u16(),
        started.elapsed().as_millis() as u64,
    );
    response_with_body(status, body, &config.channels[0])
}

fn openai_stream_response(
    response: reqwest::Response,
    model: &str,
    include_usage: bool,
) -> Response {
    let stream =
        stream_translate::convert_stream(response.bytes_stream(), model.to_string(), include_usage);
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no");
    builder = builder.header(header::CONNECTION, "keep-alive");
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ---------------------------------------------------------------------------
// Response/header plumbing
// ---------------------------------------------------------------------------

fn stream_response(response: reqwest::Response, channel: &Channel) -> Response {
    let status = response.status();
    let headers = response.headers().clone();
    let stream = response
        .bytes_stream()
        .map_err(io::Error::other)
        .map_ok(|chunk| chunk);
    let mut builder = Response::builder().status(status);
    for (name, value) in headers.iter() {
        if skip_response_header(name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder = builder.header("x-gateway-channel", &channel.name);
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn response_with_body(status: StatusCode, body: Bytes, channel: &Channel) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-gateway-channel", &channel.name)
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn json_response(status: StatusCode, value: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn auth_error(error: AuthError) -> Response {
    let message = match error {
        AuthError::Missing => "missing api key",
        AuthError::Invalid => "invalid api key",
    };
    anthropic_error(StatusCode::UNAUTHORIZED, "authentication_error", message)
}

fn anthropic_error(status: StatusCode, kind: &str, message: &str) -> Response {
    json_response(
        status,
        json!({"type": "error", "error": {"type": kind, "message": message}}),
    )
}

fn openai_error(status: StatusCode, message: &str) -> Response {
    json_response(
        status,
        json!({"error": {"message": message, "type": "invalid_request_error"}}),
    )
}

fn skip_response_header(name: &str) -> bool {
    HOP_BY_HOP.contains(&name)
        || name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case("connection")
}

fn upstream_headers(inbound: &HeaderMap, channel: &Channel) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (name, value) in inbound.iter() {
        let key = name.as_str();
        if HOP_BY_HOP.contains(&key)
            || key.eq_ignore_ascii_case("host")
            || key.eq_ignore_ascii_case("content-length")
            || key.eq_ignore_ascii_case("authorization")
            || key.eq_ignore_ascii_case("x-api-key")
        {
            continue;
        }
        if key.eq_ignore_ascii_case("user-agent") && !channel.pass_through_user_agent {
            continue;
        }
        if seen.insert(key.to_ascii_lowercase()) {
            headers.append(name.clone(), value.clone());
        }
    }
    match channel.auth {
        AuthHeader::XApiKey => {
            if let Ok(value) = HeaderValue::from_str(&channel.api_key) {
                headers.insert(HeaderName::from_static("x-api-key"), value);
            }
        }
        AuthHeader::Bearer => {
            if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", channel.api_key)) {
                headers.insert(header::AUTHORIZATION, value);
            }
        }
        AuthHeader::None => {}
    }
    for (name, value) in &channel.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

async fn read_limited(response: reqwest::Response) -> Bytes {
    let mut body = response.bytes().await.unwrap_or_default();
    if body.len() > 512 * 1024 {
        body.truncate(512 * 1024);
    }
    body
}

fn retryable(status: StatusCode) -> bool {
    matches!(
        status.as_u16(),
        408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529
    )
}

fn authorize(config: &Config, headers: &HeaderMap) -> Result<(), AuthError> {
    authorize_keys(&config.api_keys, headers)
}

fn authorize_admin(config: &Config, headers: &HeaderMap) -> Result<(), AuthError> {
    authorize_keys(config.admin_keys(), headers)
}

enum AuthError {
    Missing,
    Invalid,
}

fn authorize_keys(keys: &[String], headers: &HeaderMap) -> Result<(), AuthError> {
    if keys.is_empty() {
        return Ok(());
    }
    let presented = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
        });
    let Some(presented) = presented else {
        return Err(AuthError::Missing);
    };
    let presented = presented.as_bytes();
    let authorized = keys
        .iter()
        .any(|key| constant_time_eq(key.as_bytes(), presented));
    if authorized {
        Ok(())
    } else {
        Err(AuthError::Invalid)
    }
}

/// Length-independent comparison for the accepted API keys.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

fn access_log(
    state: &AppState,
    model: &str,
    channel: &str,
    status: u16,
    elapsed: Duration,
    stream: bool,
) {
    let elapsed_ms = elapsed.as_millis() as u64;
    if state.snapshot().log_requests {
        tracing::info!(model, channel, status, elapsed_ms, stream, "request");
    }
    let mut guard = state.access_log.lock();
    if let Some(file) = guard.as_mut() {
        use std::io::Write;
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0);
        let _ = writeln!(
            file,
            "{{\"ts\":{ts},\"model\":{},\"channel\":{},\"status\":{status},\"ms\":{elapsed_ms},\"stream\":{stream}}}",
            serde_json::to_string(model).unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(channel).unwrap_or_else(|_| "\"\"".to_string()),
        );
    }
}

// ---------------------------------------------------------------------------
// Small endpoints
// ---------------------------------------------------------------------------

async fn handle_health(State(state): State<Arc<AppState>>) -> Response {
    json_response(
        StatusCode::OK,
        json!({
            "isInitialized": true,
            "status": "ok",
            "version": format!("axonhub-rs/{}", env!("CARGO_PKG_VERSION")),
            "uptime_secs": state.started.elapsed().as_secs(),
        }),
    )
}

async fn handle_admin_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let config = state.snapshot();
    if let Err(error) = authorize_admin(&config, &headers) {
        return auth_error(error);
    }
    json_response(
        StatusCode::OK,
        json!({
            "version": format!("axonhub-rs/{}", env!("CARGO_PKG_VERSION")),
            "uptime_secs": state.started.elapsed().as_secs(),
            "channels": config.channels.len(),
            "models": config.public_models().len(),
            "requests": state.metrics.requests.load(Ordering::Relaxed),
            "errors": state.metrics.errors.load(Ordering::Relaxed),
            "active": state.metrics.active.load(Ordering::Relaxed),
            "config": state.config_path.display().to_string(),
        }),
    )
}

async fn handle_admin_routes(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let config = state.snapshot();
    if let Err(error) = authorize_admin(&config, &headers) {
        return auth_error(error);
    }
    let channels: Vec<Value> = config
        .channels
        .iter()
        .map(|channel| {
            json!({
                "name": channel.name,
                "base_url": channel.base_url,
                "enabled": channel.enabled,
                "priority": channel.priority,
                "models": channel.models,
                "mappings": channel.mappings,
                "hide_mapped_models": channel.hide_mapped_models,
            })
        })
        .collect();
    json_response(
        StatusCode::OK,
        json!({"channels": channels, "models": config.public_models()}),
    )
}

async fn handle_reload(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = authorize_admin(&state.snapshot(), &headers) {
        return auth_error(error);
    }
    match state.reload() {
        Ok(()) => json_response(StatusCode::OK, json!({"reloaded": true})),
        Err(error) => json_response(
            StatusCode::BAD_REQUEST,
            json!({"reloaded": false, "error": error.to_string()}),
        ),
    }
}

async fn handle_metrics(State(state): State<Arc<AppState>>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(Body::from(state.metrics.render()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn handle_models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let config = state.snapshot();
    if let Err(error) = authorize(&config, &headers) {
        return auth_error(error);
    }
    let created = 1_700_000_000u64;
    let data: Vec<Value> = config
        .public_models()
        .into_iter()
        .map(|model| json!({"id": model, "object": "model", "created": created, "owned_by": "axonhub-rs"}))
        .collect();
    json_response(StatusCode::OK, json!({"object": "list", "data": data}))
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn missing_and_invalid_credentials_are_rejected() {
        let known = keys(&["k1"]);
        assert!(matches!(
            authorize_keys(&known, &HeaderMap::new()),
            Err(AuthError::Missing)
        ));
        assert!(matches!(
            authorize_keys(&known, &headers(&[("x-api-key", "nope")])),
            Err(AuthError::Invalid)
        ));
    }

    #[test]
    fn x_api_key_and_bearer_are_both_accepted() {
        let known = keys(&["k1", "k2"]);
        assert!(authorize_keys(&known, &headers(&[("x-api-key", "k2")])).is_ok());
        assert!(authorize_keys(&known, &headers(&[("authorization", "Bearer k1")])).is_ok());
        assert!(authorize_keys(&known, &headers(&[("authorization", "Basic k1")])).is_err());
    }

    #[test]
    fn an_empty_key_list_leaves_the_endpoint_open() {
        assert!(authorize_keys(&[], &HeaderMap::new()).is_ok());
    }

    #[test]
    fn admin_keys_fall_back_to_the_client_keys() {
        let mut config: Config =
            serde_yaml::from_str("listen: 127.0.0.1:0\napi_keys: [client]\nchannels: []\n")
                .unwrap();
        assert_eq!(config.admin_keys(), ["client".to_string()].as_slice());

        config.admin_api_keys = vec!["admin".to_string()];
        assert_eq!(config.admin_keys(), ["admin".to_string()].as_slice());
        assert!(authorize_admin(&config, &headers(&[("x-api-key", "client")])).is_err());
        assert!(authorize_admin(&config, &headers(&[("x-api-key", "admin")])).is_ok());
    }
}
