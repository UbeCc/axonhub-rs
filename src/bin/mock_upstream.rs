//! Minimal Anthropic-shaped mock upstream for gateway benchmarks.
//!
//!     cargo run --release --bin mock_upstream -- 127.0.0.1:18082
//!
//! It answers instantly, so measured latency is the gateway's own overhead.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use serde_json::{json, Value};

#[derive(Clone)]
struct MockState {
    streaming_delay_ms: u64,
}

#[tokio::main]
async fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:18082".to_string());
    let state = MockState {
        streaming_delay_ms: std::env::var("MOCK_STREAM_DELAY_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
    };
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/messages", post(messages))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("mock upstream listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}

async fn messages(State(state): State<MockState>, _headers: HeaderMap, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("mock")
        .to_string();
    let credential = _headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if credential == "mock-fail" || model.contains("fail") {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"type":"error","error":{"type":"api_error","message":"mock failure"}})
                    .to_string(),
            ))
            .unwrap();
    }
    if request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        if state.streaming_delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(state.streaming_delay_ms)).await;
        }
        let events = [
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"msg_mock","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"usage":{"input_tokens":5,"output_tokens":0}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ];
        let mut body = String::new();
        for (event, data) in events {
            body.push_str(&format!("event: {event}\ndata: {data}\n\n"));
        }
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "id":"msg_mock","type":"message","role":"assistant","model":model,
                "content":[{"type":"text","text":"hello"}],
                "stop_reason":"end_turn",
                "usage":{"input_tokens":5,"output_tokens":1}
            })
            .to_string(),
        ))
        .unwrap()
}
