use std::{collections::HashMap, error::Error, pin::Pin};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use hyper::body::Frame;
use serde_json::{Value, json};

type BoxError = Box<dyn Error + Send + Sync>;
pub type AdapterStream = Pin<Box<dyn Stream<Item = Result<Frame<Bytes>, BoxError>> + Send>>;

pub fn chat_to_responses(payload: &Value, model: &str) -> Result<Value, String> {
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "chat completions payload must contain messages".to_owned())?;
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if matches!(role, "system" | "developer") {
            if let Some(text) = content_text(message.get("content")) {
                instructions.push(text);
            }
            continue;
        }
        if role == "tool" {
            input.push(json!({
                "type": "function_call_output",
                "call_id": message.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                "output": content_text(message.get("content")).unwrap_or_default(),
            }));
            continue;
        }
        if let Some(content) = message.get("content")
            && !content.is_null()
        {
            input.push(json!({
                "type": "message",
                "role": role,
                "content": response_content(content, role),
            }));
        }
        if role == "assistant"
            && let Some(calls) = message.get("tool_calls").and_then(Value::as_array)
        {
            for call in calls {
                let function = call.get("function").unwrap_or(&Value::Null);
                input.push(json!({
                    "type": "function_call",
                    "call_id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                    "name": function.get("name").and_then(Value::as_str).unwrap_or(""),
                    "arguments": function.get("arguments").and_then(Value::as_str).unwrap_or("{}"),
                }));
            }
        }
    }

    let mut output = json!({
        "model": model,
        "instructions": instructions.join("\n\n"),
        "input": input,
        "store": false,
        // ChatGPT's Codex subscription endpoint requires streaming upstream.
        // Infernaut aggregates the event stream for non-streaming clients.
        "stream": true,
    });
    if output["instructions"].as_str().is_some_and(str::is_empty) {
        output["instructions"] = json!("You are a helpful assistant.");
    }
    if let Some(effort) = payload
        .get("reasoning_effort")
        .filter(|value| !value.is_null())
    {
        if !effort.is_string() {
            return Err("reasoning_effort must be a string".to_owned());
        }
        output["reasoning"] = json!({ "effort": effort });
    }
    if let Some(tools) = payload.get("tools").and_then(Value::as_array) {
        output["tools"] = Value::Array(
            tools
                .iter()
                .filter_map(|tool| tool.get("function"))
                .map(|function| {
                    json!({
                        "type": "function",
                        "name": function.get("name").cloned().unwrap_or(Value::Null),
                        "description": function.get("description").cloned().unwrap_or(json!("")),
                        "parameters": function.get("parameters").cloned().unwrap_or(json!({})),
                        "strict": function.get("strict").cloned().unwrap_or(json!(false)),
                    })
                })
                .collect(),
        );
    }
    // The ChatGPT subscription transport deliberately exposes a smaller
    // request surface than the public Responses API. In particular it rejects
    // max_output_tokens and temperature, so Chat Completions sampling controls
    // must not be translated into those fields.
    if payload
        .get("max_tokens")
        .or_else(|| payload.get("max_completion_tokens"))
        .is_some()
    {
        tracing::warn!(
            "Codex subscription transport does not enforce the requested output token cap"
        );
    }
    for (source, target) in [
        ("tool_choice", "tool_choice"),
        ("parallel_tool_calls", "parallel_tool_calls"),
    ] {
        if let Some(value) = payload.get(source) {
            output[target] = value.clone();
        }
    }
    if let Some(choice) = payload.get("tool_choice") {
        output["tool_choice"] = if choice.get("type").and_then(Value::as_str) == Some("function") {
            json!({
                "type": "function",
                "name": choice
                    .get("function")
                    .and_then(|value| value.get("name"))
                    .cloned()
                    .unwrap_or(Value::Null),
            })
        } else {
            choice.clone()
        };
    }
    if let Some(format) = payload.get("response_format") {
        output["text"] = json!({ "format": response_format_to_text_format(format)? });
    }
    Ok(output)
}

fn response_format_to_text_format(format: &Value) -> Result<Value, String> {
    let kind = format
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "response_format.type must be a string".to_owned())?;
    match kind {
        "text" => Ok(json!({ "type": "text" })),
        "json_object" => Ok(json!({ "type": "json_object" })),
        "json_schema" => {
            let schema = format
                .get("json_schema")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    "response_format.json_schema must be an object for type 'json_schema'"
                        .to_owned()
                })?;
            let name = schema
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| "response_format.json_schema.name is required".to_owned())?;
            let json_schema = schema
                .get("schema")
                .filter(|schema| !schema.is_null())
                .ok_or_else(|| "response_format.json_schema.schema is required".to_owned())?;
            let mut translated = json!({
                "type": "json_schema",
                "name": name,
                "schema": json_schema,
            });
            for field in ["description", "strict"] {
                if let Some(value) = schema.get(field) {
                    translated[field] = value.clone();
                }
            }
            Ok(translated)
        }
        other => Err(format!(
            "unsupported response_format.type '{other}'; expected text, json_object, or json_schema"
        )),
    }
}

pub fn response_to_chat(response: Value, model: &str) -> Value {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if matches!(
                                part.get("type").and_then(Value::as_str),
                                Some("output_text" | "text")
                            ) {
                                text.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                            }
                        }
                    }
                }
                Some("function_call") => tool_calls.push(json!({
                    "id": item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {
                        "name": item.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": item.get("arguments").cloned().unwrap_or(json!("{}")),
                    }
                })),
                _ => {}
            }
        }
    }
    let mut message = json!({ "role": "assistant", "content": text });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    json!({
        "id": response.get("id").cloned().unwrap_or(json!("infernaut-codex")),
        "object": "chat.completion",
        "created": response.get("created_at").cloned().unwrap_or(json!(0)),
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": if message.get("tool_calls").is_some() { "tool_calls" } else { "stop" }
        }],
        "usage": normalize_usage(response.get("usage")),
    })
}

pub fn response_sse_to_chat_response(body: &[u8], model: &str) -> Result<Value, String> {
    let mut response_id = "infernaut-codex".to_owned();
    let mut created = 0_u64;
    let mut text = String::new();
    let mut output = Vec::new();
    let mut usage = Value::Null;
    let mut completed = false;

    for event in body
        .split(|byte| *byte == b'\n')
        .collect::<Vec<_>>()
        .split(|line| line.is_empty())
    {
        let payload = event
            .iter()
            .filter_map(|line| std::str::from_utf8(line).ok())
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let value: Value = serde_json::from_str(&payload)
            .map_err(|error| format!("Codex returned an invalid SSE event: {error}"))?;
        if let Some(response) = value.get("response") {
            if let Some(id) = response.get("id").and_then(Value::as_str) {
                response_id = id.to_owned();
            }
            created = response
                .get("created_at")
                .and_then(Value::as_u64)
                .unwrap_or(created);
            if let Some(value) = response.get("usage")
                && !value.is_null()
            {
                usage = value.clone();
            }
        }
        match value.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                text.push_str(value.get("delta").and_then(Value::as_str).unwrap_or(""));
            }
            Some("response.output_item.done") => {
                let item = value.get("item").cloned().unwrap_or(Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    output.push(item);
                }
            }
            Some("response.completed") => completed = true,
            Some("response.incomplete") => {
                return Err(format!(
                    "Codex response incomplete: {}",
                    value
                        .pointer("/response/incomplete_details/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unspecified reason")
                ));
            }
            Some("error") | Some("response.failed") => {
                let message = value
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex response failed");
                return Err(message.to_owned());
            }
            _ => {}
        }
    }

    if !completed {
        return Err("Codex stream ended without response.completed".to_owned());
    }
    output.insert(
        0,
        json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        }),
    );
    Ok(response_to_chat(
        json!({
            "id": response_id,
            "created_at": created,
            "output": output,
            "usage": usage,
        }),
        model,
    ))
}

pub fn response_sse_to_chat<S>(mut upstream: S, model: String) -> AdapterStream
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    let stream = async_stream::try_stream! {
        let mut buffer = Vec::<u8>::new();
        let mut response_id = "infernaut-codex".to_owned();
        let mut created = 0_u64;
        let mut started = false;
        let mut tool_indexes: HashMap<String, usize> = HashMap::new();
        let mut next_tool = 0_usize;
        let mut used_tools = false;

        while let Some(chunk) = upstream.next().await {
            let chunk = chunk?;
            if buffer.len().saturating_add(chunk.len()) > 32 * 1024 * 1024 {
                Err(std::io::Error::other("Codex SSE buffer exceeds 32 MiB"))?;
            }
            buffer.extend_from_slice(&chunk);
            while let Some(end) = crate::diagnostics::sse_event_end(&buffer) {
                let event = buffer.drain(..end).collect::<Vec<_>>();
                let payload = sse_data(&event);
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                let value: Value = match serde_json::from_str(&payload) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
                if let Some(response) = value.get("response") {
                    if let Some(id) = response.get("id").and_then(Value::as_str) {
                        response_id = id.to_owned();
                    }
                    created = response.get("created_at").and_then(Value::as_u64).unwrap_or(created);
                }
                if !started {
                    started = true;
                    yield Frame::data(chat_sse(&json!({
                        "id": response_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }]
                    })));
                }
                match kind {
                    "response.output_text.delta" => {
                        let delta = value.get("delta").and_then(Value::as_str).unwrap_or("");
                        yield Frame::data(chat_sse(&json!({
                            "id": response_id, "object": "chat.completion.chunk", "created": created, "model": model,
                            "choices": [{ "index": 0, "delta": { "content": delta }, "finish_reason": null }]
                        })));
                    }
                    "response.output_item.added" => {
                        let item = value.get("item").unwrap_or(&Value::Null);
                        if item.get("type").and_then(Value::as_str) == Some("function_call") {
                            used_tools = true;
                            let key = item.get("id").or_else(|| item.get("call_id")).and_then(Value::as_str).unwrap_or("").to_owned();
                            let index = next_tool;
                            next_tool += 1;
                            tool_indexes.insert(key, index);
                            yield Frame::data(chat_sse(&json!({
                                "id": response_id, "object": "chat.completion.chunk", "created": created, "model": model,
                                "choices": [{ "index": 0, "delta": { "tool_calls": [{
                                    "index": index,
                                    "id": item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or(Value::Null),
                                    "type": "function",
                                    "function": { "name": item.get("name").cloned().unwrap_or(Value::Null), "arguments": "" }
                                }]}, "finish_reason": null }]
                            })));
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        used_tools = true;
                        let key = value.get("item_id").and_then(Value::as_str).unwrap_or("");
                        let index = tool_indexes.get(key).copied().unwrap_or(0);
                        yield Frame::data(chat_sse(&json!({
                            "id": response_id, "object": "chat.completion.chunk", "created": created, "model": model,
                            "choices": [{ "index": 0, "delta": { "tool_calls": [{
                                "index": index, "function": { "arguments": value.get("delta").cloned().unwrap_or(json!("")) }
                            }]}, "finish_reason": null }]
                        })));
                    }
                    "error" | "response.failed" | "response.incomplete" => {
                        let detail = value.pointer("/response/error/message")
                            .or_else(|| value.get("message"))
                            .or_else(|| value.pointer("/response/incomplete_details/reason"))
                            .and_then(Value::as_str).unwrap_or("unspecified upstream failure");
                        Err(std::io::Error::other(format!("Codex {kind}: {detail}")))?;
                    }
                    "response.completed" => {
                        let usage = value.get("response").and_then(|v| v.get("usage"));
                        let finish_reason = if used_tools { "tool_calls" } else { "stop" };
                        yield Frame::data(chat_sse(&json!({
                            "id": response_id, "object": "chat.completion.chunk", "created": created, "model": model,
                            "choices": [{ "index": 0, "delta": {}, "finish_reason": finish_reason }],
                            "usage": normalize_usage(usage),
                        })));
                        yield Frame::data(Bytes::from_static(b"data: [DONE]\n\n"));
                        return;
                    }
                    _ => {}
                }
            }
        }
        Err(std::io::Error::other("Codex stream ended without response.completed"))?;
    };
    Box::pin(stream)
}

fn response_content(content: &Value, role: &str) -> Vec<Value> {
    let text_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    match content {
        Value::String(text) => vec![json!({ "type": text_type, "text": text })],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    Some(json!({ "type": text_type, "text": text }))
                } else {
                    let url = part
                        .get("image_url")
                        .and_then(|value| value.get("url").or(Some(value)))
                        .and_then(Value::as_str)?;
                    Some(json!({ "type": "input_image", "image_url": url }))
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn content_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(value) => Some(value.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        value => Some(value.to_string()),
    }
}

fn normalize_usage(usage: Option<&Value>) -> Value {
    let usage = usage.unwrap_or(&Value::Null);
    let prompt = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": usage.get("total_tokens").and_then(Value::as_u64).unwrap_or(prompt + completion),
    })
}

fn sse_data(event: &[u8]) -> String {
    String::from_utf8_lossy(event)
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n")
}

fn chat_sse(value: &Value) -> Bytes {
    Bytes::from(format!("data: {}\n\n", value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_chat_messages_and_tools() {
        let translated = chat_to_responses(
            &json!({
                "model": "ignored",
                "messages": [
                    {"role":"system", "content":"Be concise"},
                    {"role":"user", "content":"weather?"},
                    {"role":"assistant", "content":null, "tool_calls":[{"id":"c1","type":"function","function":{"name":"weather","arguments":"{}"}}]},
                    {"role":"tool", "tool_call_id":"c1", "content":"sunny"}
                ],
                "tools": [{"type":"function","function":{"name":"weather","parameters":{"type":"object"}}}],
                "stream": true,
                "max_tokens": 100,
                "max_completion_tokens": 200,
                "temperature": 0.2
            }),
            "gpt-test",
        ).unwrap();
        assert_eq!(translated["model"], "gpt-test");
        assert_eq!(translated["instructions"], "Be concise");
        assert_eq!(translated["input"][1]["type"], "function_call");
        assert_eq!(translated["input"][2]["type"], "function_call_output");
        assert_eq!(translated["tools"][0]["name"], "weather");
        assert!(translated.get("max_output_tokens").is_none());
        assert!(translated.get("temperature").is_none());
    }

    #[test]
    fn preserves_requested_reasoning_effort() {
        let translated =
            chat_to_responses(&json!({"messages": [], "reasoning_effort": "high"}), "test")
                .unwrap();
        assert_eq!(translated["reasoning"]["effort"], "high");
    }

    #[test]
    fn flattens_chat_json_schema_for_responses_text_format() {
        let translated = chat_to_responses(
            &json!({
                "model": "ignored",
                "messages": [{"role": "user", "content": "extract it"}],
                "response_format": {
                    "type": "json_schema",
                    "json_schema": {
                        "name": "result",
                        "description": "Structured result",
                        "strict": true,
                        "schema": {
                            "type": "object",
                            "properties": {"answer": {"type": "string"}},
                            "required": ["answer"],
                            "additionalProperties": false
                        }
                    }
                }
            }),
            "gpt-test",
        )
        .unwrap();

        assert_eq!(translated["text"]["format"]["type"], "json_schema");
        assert_eq!(translated["text"]["format"]["name"], "result");
        assert_eq!(translated["text"]["format"]["strict"], true);
        assert_eq!(translated["text"]["format"]["schema"]["type"], "object");
        assert!(translated["text"]["format"].get("json_schema").is_none());
    }

    #[test]
    fn translates_json_object_response_format() {
        let translated = chat_to_responses(
            &json!({
                "messages": [{"role": "user", "content": "return JSON"}],
                "response_format": {"type": "json_object"}
            }),
            "gpt-test",
        )
        .unwrap();
        assert_eq!(translated["text"]["format"], json!({"type": "json_object"}));
    }

    #[test]
    fn rejects_malformed_chat_json_schema_before_proxying() {
        let error = chat_to_responses(
            &json!({
                "messages": [{"role": "user", "content": "extract it"}],
                "response_format": {
                    "type": "json_schema",
                    "json_schema": {"name": "missing_schema"}
                }
            }),
            "gpt-test",
        )
        .unwrap_err();
        assert!(error.contains("response_format.json_schema.schema is required"));
    }

    #[test]
    fn translates_response_object_to_chat_completion() {
        let chat = response_to_chat(
            json!({
                "id": "resp_1",
                "output": [{"type":"message","content":[{"type":"output_text","text":"hello"}]}],
                "usage": {"input_tokens":2,"output_tokens":1,"total_tokens":3}
            }),
            "gpt-test",
        );
        assert_eq!(chat["choices"][0]["message"]["content"], "hello");
        assert_eq!(chat["usage"]["total_tokens"], 3);
    }

    #[test]
    fn aggregates_response_sse_for_non_streaming_chat_clients() {
        let body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"created_at\":7}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":1,\"total_tokens\":3}}}\n\n",
        );
        let chat = response_sse_to_chat_response(body.as_bytes(), "gpt-test").unwrap();
        assert_eq!(chat["id"], "r1");
        assert_eq!(chat["choices"][0]["message"]["content"], "hello");
        assert_eq!(chat["usage"]["total_tokens"], 3);
    }

    #[tokio::test]
    async fn failed_incomplete_and_truncated_streams_never_finish_successfully() {
        for kind in [
            "response.failed",
            "response.incomplete",
            "response.output_text.delta",
        ] {
            let body = format!("data: {{\"type\":\"{kind}\",\"delta\":\"partial\"}}\n\n");
            assert!(response_sse_to_chat_response(body.as_bytes(), "test").is_err());
            let chunks = body
                .as_bytes()
                .chunks(3)
                .map(|chunk| Ok::<_, reqwest::Error>(Bytes::copy_from_slice(chunk)))
                .collect::<Vec<_>>();
            let mut stream =
                response_sse_to_chat(futures_util::stream::iter(chunks), "test".into());
            let mut failed = false;
            while let Some(frame) = stream.next().await {
                match frame {
                    Err(_) => failed = true,
                    Ok(frame) => assert!(
                        !String::from_utf8_lossy(frame.data_ref().unwrap()).contains("[DONE]")
                    ),
                }
            }
            assert!(failed);
        }
    }

    #[tokio::test]
    async fn translates_streaming_text_and_completion_marker() {
        let events = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"created_at\":7}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":1,\"total_tokens\":3}}}\n\n",
            "data: [DONE]\n\n"
        );
        let upstream = futures_util::stream::iter(vec![Ok::<Bytes, reqwest::Error>(
            Bytes::from_static(events.as_bytes()),
        )]);
        let mut translated = response_sse_to_chat(upstream, "gpt-test".into());
        let mut body = Vec::new();
        while let Some(frame) = translated.next().await {
            let frame = frame.unwrap();
            body.extend_from_slice(frame.data_ref().unwrap());
        }
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("\"content\":\"hello\""));
        assert!(body.contains("\"finish_reason\":\"stop\""));
        assert!(body.ends_with("data: [DONE]\n\n"));
    }
}
