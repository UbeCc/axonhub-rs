//! Protocol translation between the OpenAI Chat Completions API (used by omp
//! and other OpenAI-shaped clients) and the Anthropic Messages API (the shape
//! every currently configured upstream speaks).
//!
//! Only the fields that matter in practice are translated; unknown OpenAI
//! fields are ignored rather than forwarded, because an Anthropic upstream
//! would reject them.

use serde_json::{json, Map, Value};

use crate::model::extract_model;

/// Translate an OpenAI `/v1/chat/completions` request body into an Anthropic
/// `/v1/messages` request body.
pub fn openai_to_anthropic(
    body: &[u8],
    upstream_model: &str,
    default_max_tokens: u64,
) -> Result<Value, String> {
    let request: Value =
        serde_json::from_slice(body).map_err(|error| format!("invalid JSON: {error}"))?;
    let object = request
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;

    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "messages must be an array".to_string())?;

    let mut system_parts: Vec<String> = Vec::new();
    let mut anthropic_messages: Vec<Value> = Vec::new();

    for message in messages {
        let message = message
            .as_object()
            .ok_or_else(|| "each message must be an object".to_string())?;
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "system" | "developer" => {
                for text in content_texts(message.get("content")) {
                    system_parts.push(text);
                }
            }
            "user" => {
                let blocks = content_blocks(message.get("content"))?;
                push_message(&mut anthropic_messages, "user", blocks);
            }
            "assistant" => {
                let mut blocks = content_blocks(message.get("content"))?;
                if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for call in tool_calls {
                        let call = call
                            .as_object()
                            .ok_or_else(|| "tool_calls entries must be objects".to_string())?;
                        let function =
                            call.get("function")
                                .and_then(Value::as_object)
                                .ok_or_else(|| {
                                    "tool_calls entries need a function object".to_string()
                                })?;
                        let name = function
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let arguments = function
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        let input: Value =
                            serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
                            "name": name,
                            "input": input,
                        }));
                    }
                }
                push_message(&mut anthropic_messages, "assistant", blocks);
            }
            "tool" => {
                let tool_use_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mut result = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content_texts(message.get("content")).join("\n"),
                });
                if message
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    result["is_error"] = json!(true);
                }
                push_message(&mut anthropic_messages, "user", vec![result]);
            }
            other => return Err(format!("unsupported message role {other}")),
        }
    }

    if anthropic_messages.is_empty() {
        return Err("messages must contain at least one user or assistant message".to_string());
    }

    let max_tokens = object
        .get("max_completion_tokens")
        .or_else(|| object.get("max_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(default_max_tokens);

    let mut out = Map::new();
    out.insert("model".to_string(), json!(upstream_model));
    out.insert("max_tokens".to_string(), json!(max_tokens));
    out.insert("messages".to_string(), Value::Array(anthropic_messages));
    if !system_parts.is_empty() {
        out.insert("system".to_string(), json!(system_parts.join("\n\n")));
    }
    if let Some(value) = object.get("stream") {
        out.insert("stream".to_string(), value.clone());
    }
    if let Some(value) = object.get("temperature").filter(|value| !value.is_null()) {
        out.insert("temperature".to_string(), value.clone());
    }
    if let Some(value) = object.get("top_p").filter(|value| !value.is_null()) {
        out.insert("top_p".to_string(), value.clone());
    }
    if let Some(stop) = object.get("stop").filter(|value| !value.is_null()) {
        let sequences = match stop {
            Value::String(text) => vec![json!(text)],
            Value::Array(items) => items.clone(),
            _ => Vec::new(),
        };
        if !sequences.is_empty() {
            out.insert("stop_sequences".to_string(), Value::Array(sequences));
        }
    }
    if let Some(metadata) = object.get("metadata").and_then(Value::as_object) {
        if let Some(user) = metadata.get("user_id").or_else(|| metadata.get("user")) {
            out.insert("metadata".to_string(), json!({ "user_id": user }));
        }
    }
    if let Some(tools) = object.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools
            .iter()
            .filter_map(|tool| {
                let function = tool.get("function")?.as_object()?;
                let name = function.get("name")?.as_str()?;
                let mut tool_out = json!({
                    "name": name,
                    "input_schema": function.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}})),
                });
                if let Some(description) = function.get("description").and_then(Value::as_str) {
                    tool_out["description"] = json!(description);
                }
                Some(tool_out)
            })
            .collect();
        if !converted.is_empty() {
            out.insert("tools".to_string(), Value::Array(converted));
        }
    }
    if let Some(choice) = object.get("tool_choice") {
        let mapped = match choice {
            Value::String(kind) if kind == "auto" => json!({"type":"auto"}),
            Value::String(kind) if kind == "required" => json!({"type":"any"}),
            Value::String(kind) if kind == "none" => json!({"type":"none"}),
            Value::Object(map) if map.get("type").and_then(Value::as_str) == Some("function") => {
                let name = map
                    .get("function")
                    .and_then(Value::as_object)
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                json!({"type":"tool","name":name})
            }
            _ => Value::Null,
        };
        if !mapped.is_null() {
            out.insert("tool_choice".to_string(), mapped);
        }
    }
    if let Some(effort) = object.get("reasoning_effort").and_then(Value::as_str) {
        out.insert("output_config".to_string(), json!({ "effort": effort }));
    }
    Ok(Value::Object(out))
}

fn content_texts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                let part = part.as_object()?;
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    part.get("text").and_then(Value::as_str).map(str::to_string)
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn content_blocks(content: Option<&Value>) -> Result<Vec<Value>, String> {
    let mut blocks = Vec::new();
    match content {
        Some(Value::String(text)) if !text.is_empty() => {
            blocks.push(json!({"type":"text","text":text}))
        }
        Some(Value::String(_)) | Some(Value::Null) | None => {}
        Some(Value::Array(parts)) => {
            for part in parts {
                let part = part
                    .as_object()
                    .ok_or_else(|| "content parts must be objects".to_string())?;
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                        blocks.push(json!({"type":"text","text":text}));
                    }
                    Some("image_url") => {
                        let url = part
                            .get("image_url")
                            .and_then(|value| value.as_object())
                            .and_then(|value| value.get("url"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        blocks.push(image_block(url)?);
                    }
                    Some("image") => {
                        if let Some(source) = part.get("source") {
                            blocks.push(json!({"type":"image","source":source}));
                        }
                    }
                    Some(other) => return Err(format!("unsupported content part {other}")),
                    None => {}
                }
            }
        }
        Some(_) => return Err("content must be a string or an array".to_string()),
    }
    Ok(blocks)
}

fn image_block(url: &str) -> Result<Value, String> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest
            .split_once(',')
            .ok_or_else(|| "malformed data URL".to_string())?;
        let media_type = meta.split(';').next().unwrap_or("image/png");
        if !meta.contains("base64") {
            return Err("only base64 data URLs are supported for images".to_string());
        }
        Ok(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data },
        }))
    } else {
        Ok(json!({
            "type": "image",
            "source": { "type": "url", "url": url },
        }))
    }
}

fn push_message(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    // Anthropic rejects two assistant messages in a row; merge instead.
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) && role == "user" {
            if let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut) {
                existing.extend(blocks);
                return;
            }
        }
    }
    messages.push(json!({"role": role, "content": blocks}));
}

/// Translate an Anthropic Messages response into an OpenAI Chat Completions
/// response.
pub fn anthropic_to_openai(response: &Value, requested_model: &str, created: u64) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    if let Some(blocks) = response.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(chunk) = block.get("text").and_then(Value::as_str) {
                        text.push_str(chunk);
                    }
                }
                Some("thinking") => {
                    if let Some(chunk) = block.get("thinking").and_then(Value::as_str) {
                        reasoning.push_str(chunk);
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(json!({
                        "id": block.get("id").cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": {
                            "name": block.get("name").cloned().unwrap_or(Value::Null),
                            "arguments": block.get("input").map(|input| input.to_string()).unwrap_or_else(|| "{}".to_string()),
                        }
                    }));
                }
                _ => {}
            }
        }
    }
    let mut message = Map::new();
    message.insert("role".to_string(), json!("assistant"));
    message.insert(
        "content".to_string(),
        if text.is_empty() && !tool_calls.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
    );
    if !reasoning.is_empty() {
        message.insert("reasoning_content".to_string(), json!(reasoning));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    let finish_reason = match response.get("stop_reason").and_then(Value::as_str) {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        _ => "stop",
    };
    let usage = response.get("usage").cloned().unwrap_or_else(|| json!({}));
    json!({
        "id": response.get("id").cloned().unwrap_or_else(|| json!(format!("chatcmpl-{}", uuid::Uuid::new_v4()))),
        "object": "chat.completion",
        "created": created,
        "model": requested_model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason,
        }],
        "usage": openai_usage(&usage),
    })
}

pub fn openai_usage(usage: &Value) -> Value {
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input + output,
        "prompt_tokens_details": { "cached_tokens": cached },
    })
}

/// Extract the client-facing model from an OpenAI request body.
pub fn openai_model(body: &[u8]) -> Option<String> {
    extract_model(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_basic_chat() {
        let body = br#"{"model":"claude-sonnet-latest","messages":[{"role":"system","content":"be terse"},{"role":"user","content":"hi"}],"max_tokens":16,"temperature":0.2}"#;
        let out = openai_to_anthropic(body, "claude-sonnet-4-5", 4096).unwrap();
        assert_eq!(out["model"], "claude-sonnet-4-5");
        assert_eq!(out["max_tokens"], 16);
        assert_eq!(out["system"], "be terse");
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(out["temperature"], 0.2);
    }

    #[test]
    fn translates_tools_and_tool_results() {
        let body = json!({
            "model": "m",
            "messages": [
                {"role":"user","content":"weather?"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]},
                {"role":"tool","tool_call_id":"call_1","content":"sunny"}
            ],
            "tools": [{"type":"function","function":{"name":"weather","description":"get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}]
        })
        .to_string();
        let out = openai_to_anthropic(body.as_bytes(), "m", 4096).unwrap();
        assert_eq!(out["tools"][0]["name"], "weather");
        assert_eq!(out["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(out["messages"][1]["content"][0]["input"]["city"], "Paris");
        assert_eq!(out["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(out["messages"][2]["content"][0]["tool_use_id"], "call_1");
    }

    #[test]
    fn translates_anthropic_response() {
        let response = json!({
            "id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-5",
            "content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"hello"}],
            "stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":4}
        });
        let out = anthropic_to_openai(&response, "claude-sonnet-latest", 42);
        assert_eq!(out["choices"][0]["message"]["content"], "hello");
        assert_eq!(out["choices"][0]["message"]["reasoning_content"], "hmm");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 10);
        assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 4);
    }
}
