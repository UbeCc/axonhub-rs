//! Streaming translation: Anthropic server-sent events -> OpenAI
//! `chat.completion.chunk` events.
//!
//! The upstream stream is parsed line by line and each event is emitted
//! immediately; nothing is buffered beyond one SSE line.  That keeps
//! time-to-first-token identical to the upstream and adds only a few
//! microseconds of JSON reshaping per event.

use std::collections::HashMap;
use std::io;

use bytes::Bytes;
use futures_util::Stream;
use serde_json::{json, Value};

use crate::translate::{anthropic_to_openai, openai_usage};

/// Convert an upstream Anthropic SSE byte stream into an OpenAI SSE stream.
pub fn convert_stream<S>(
    upstream: S,
    requested_model: String,
    include_usage: bool,
) -> impl Stream<Item = Result<Bytes, io::Error>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    async_stream::stream! {
        use futures_util::StreamExt;

        let mut upstream = Box::pin(upstream);
        let mut buffer: Vec<u8> = Vec::with_capacity(8192);
        let id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let mut role_sent = false;
        let mut tool_indices: HashMap<u64, usize> = HashMap::new();
        let mut next_tool_index = 0usize;
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<Value> = None;

        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    let payload = json!({
                        "error": {"message": format!("upstream stream error: {error}"), "type": "upstream_error"}
                    });
                    yield Ok(sse(&payload));
                    yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
                    return;
                }
            };
            buffer.extend_from_slice(&chunk);
            while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = buffer.drain(..=position).collect();
                let line = trim_line(&line);
                if line.is_empty() {
                    continue;
                }
                let Some(data) = line.strip_prefix(b"data:") else { continue };
                let data = trim_ascii(data);
                if data == b"[DONE]" {
                    continue;
                }
                let Ok(event) = serde_json::from_slice::<Value>(data) else { continue };
                match event.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "message_start" => {
                        if let Some(value) = event.get("message").and_then(|message| message.get("usage")) {
                            usage = Some(value.clone());
                        }
                        if !role_sent {
                            role_sent = true;
                            yield Ok(chunk_event(&id, created, &requested_model, json!({"role": "assistant"}), None));
                        }
                    }
                    "content_block_start" => {
                        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                        if let Some(block) = event.get("content_block") {
                            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                                let tool_index = next_tool_index;
                                next_tool_index += 1;
                                tool_indices.insert(index, tool_index);
                                yield Ok(chunk_event(
                                    &id,
                                    created,
                                    &requested_model,
                                    json!({"tool_calls": [{
                                        "index": tool_index,
                                        "id": block.get("id").cloned().unwrap_or(Value::Null),
                                        "type": "function",
                                        "function": {
                                            "name": block.get("name").cloned().unwrap_or(Value::Null),
                                            "arguments": "",
                                        }
                                    }]}),
                                    None,
                                ));
                            } else if block.get("type").and_then(Value::as_str) == Some("text") && !role_sent {
                                role_sent = true;
                                yield Ok(chunk_event(&id, created, &requested_model, json!({"role": "assistant"}), None));
                            }
                        }
                    }
                    "content_block_delta" => {
                        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                        if let Some(delta) = event.get("delta") {
                            match delta.get("type").and_then(Value::as_str).unwrap_or_default() {
                                "text_delta" => {
                                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                        yield Ok(chunk_event(&id, created, &requested_model, json!({"content": text}), None));
                                    }
                                }
                                "thinking_delta" => {
                                    if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                                        yield Ok(chunk_event(&id, created, &requested_model, json!({"reasoning_content": text}), None));
                                    }
                                }
                                "input_json_delta" => {
                                    if let Some(text) = delta.get("partial_json").and_then(Value::as_str) {
                                        let tool_index = *tool_indices.get(&index).unwrap_or(&0);
                                        yield Ok(chunk_event(
                                            &id,
                                            created,
                                            &requested_model,
                                            json!({"tool_calls": [{
                                                "index": tool_index,
                                                "function": { "arguments": text }
                                            }]}),
                                            None,
                                        ));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(delta) = event.get("delta") {
                            if let Some(reason) = delta.get("stop_reason").and_then(Value::as_str) {
                                finish_reason = Some(map_finish_reason(reason).to_string());
                            }
                        }
                        if let Some(value) = event.get("usage") {
                            let mut merged = usage.clone().unwrap_or_else(|| json!({}));
                            if let (Some(target), Some(source)) = (merged.as_object_mut(), value.as_object()) {
                                for (key, value) in source {
                                    target.insert(key.clone(), value.clone());
                                }
                            }
                            usage = Some(merged);
                        }
                    }
                    "message_stop" => {
                        if !role_sent {
                            yield Ok(chunk_event(&id, created, &requested_model, json!({"role": "assistant"}), None));
                        }
                        yield Ok(chunk_event(
                            &id,
                            created,
                            &requested_model,
                            json!({}),
                            Some(finish_reason.clone().unwrap_or_else(|| "stop".to_string()).as_str()),
                        ));
                        if include_usage {
                            if let Some(usage) = usage.as_ref() {
                                yield Ok(usage_event(&id, created, &requested_model, usage));
                            }
                        }
                        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
                        return;
                    }
                    "error" => {
                        let payload = event.get("error").cloned().unwrap_or_else(|| json!({"message": "upstream error"}));
                        yield Ok(sse(&json!({"error": payload})));
                        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
                        return;
                    }
                    _ => {}
                }
            }
        }

        // Upstream ended without message_stop: still terminate the stream.
        {
            yield Ok(chunk_event(
                &id,
                created,
                &requested_model,
                json!({}),
                Some(finish_reason.unwrap_or_else(|| "stop".to_string()).as_str()),
            ));
            if include_usage {
                if let Some(usage) = usage.as_ref() {
                    yield Ok(usage_event(&id, created, &requested_model, usage));
                }
            }
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        }
    }
}

fn trim_line(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b'\n' || line[end - 1] == b'\r') {
        end -= 1;
    }
    &line[..end]
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while let Some((first, rest)) = value.split_first() {
        if first.is_ascii_whitespace() {
            value = rest;
        } else {
            break;
        }
    }
    value
}

fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => "stop",
    }
}

fn sse(payload: &Value) -> Bytes {
    let mut out = Vec::with_capacity(64 + payload.to_string().len());
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(payload.to_string().as_bytes());
    out.extend_from_slice(b"\n\n");
    Bytes::from(out)
}

fn chunk_event(
    id: &str,
    created: u64,
    model: &str,
    delta: Value,
    finish_reason: Option<&str>,
) -> Bytes {
    let payload = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
    });
    sse(&payload)
}

fn usage_event(id: &str, created: u64, model: &str, usage: &Value) -> Bytes {
    let payload = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [],
        "usage": openai_usage(usage),
    });
    sse(&payload)
}

/// Non-streaming helper kept here so callers share the finish-reason mapping.
pub fn convert_response(response: &Value, requested_model: &str) -> Value {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    anthropic_to_openai(response, requested_model, created)
}
