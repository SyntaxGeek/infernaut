use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use serde_json::Value;
use std::pin::Pin;

const INFERNAUT_DIAGNOSTICS_KEY: &str = "infernaut_diagnostics";
type BoxError = Box<dyn std::error::Error + Send + Sync>;
type DiagnosticStream = Pin<Box<dyn Stream<Item = Result<Bytes, BoxError>> + Send>>;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: &'static str,
    pub summary: &'static str,
    pub guidance: &'static str,
}

impl Diagnostic {
    pub fn display_message(&self) -> String {
        format!(
            "Infernaut diagnostic [{}]: {} {}",
            self.code, self.summary, self.guidance
        )
    }
}

pub fn detect(text: &str) -> Option<Diagnostic> {
    let normalized = text.to_ascii_lowercase();
    if normalized.contains("failed to create metal shared event") {
        return Some(Diagnostic {
            code: "mlx_metal_shared_event_unavailable",
            severity: "error",
            summary: "The provider's MLX worker could not create a Metal shared event.",
            guidance: "Restarting only the provider may not clear this macOS Metal state; fully reboot the Mac, then reload the model and retry.",
        });
    }
    None
}

/// Adds Infernaut guidance without replacing the provider's payload. If the
/// payload has a conventional error message, the guidance is appended so
/// clients that only render `error.message` still show it.
pub fn annotate_json_body(body: &[u8]) -> (Bytes, Option<Diagnostic>) {
    let Some(diagnostic) = detect(&String::from_utf8_lossy(body)) else {
        return (Bytes::copy_from_slice(body), None);
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return (Bytes::copy_from_slice(body), Some(diagnostic));
    };
    if !value.is_object() {
        return (Bytes::copy_from_slice(body), Some(diagnostic));
    }
    annotate_json_value(&mut value, &diagnostic);
    match serde_json::to_vec(&value) {
        Ok(encoded) => (Bytes::from(encoded), Some(diagnostic)),
        Err(_) => (Bytes::copy_from_slice(body), Some(diagnostic)),
    }
}

fn annotate_json_value(value: &mut Value, diagnostic: &Diagnostic) {
    let display = diagnostic.display_message();
    for pointer in ["/error/message", "/response/error/message"] {
        if let Some(original) = value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            if !original.contains(&display) {
                *value.pointer_mut(pointer).expect("message pointer exists") =
                    Value::String(format!("{original}\n\n{display}"));
            }
            break;
        }
    }
    value[INFERNAUT_DIAGNOSTICS_KEY] =
        serde_json::to_value([diagnostic]).expect("diagnostic is serializable");
}

/// Preserves each upstream SSE event, decorating only JSON error events that
/// match a known diagnostic. Buffering to event boundaries also handles an
/// error signature split across network chunks.
#[cfg(test)]
pub fn annotate_sse<S, E>(upstream: S) -> DiagnosticStream
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Into<BoxError> + Send + 'static,
{
    annotate_sse_with(upstream, |_| {})
}

pub fn annotate_sse_with<S, E, F>(upstream: S, on_diagnostic: F) -> DiagnosticStream
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Into<BoxError> + Send + 'static,
    F: Fn(Diagnostic) + Send + 'static,
{
    let stream = async_stream::stream! {
        let mut upstream = Box::pin(upstream);
        let mut pending = Vec::new();
        while let Some(item) = upstream.next().await {
            match item {
                Ok(chunk) => {
                    if pending.len().saturating_add(chunk.len()) > 32 * 1024 * 1024 {
                        yield Err(BoxError::from(std::io::Error::other("SSE buffer exceeds 32 MiB")));
                        return;
                    }
                    pending.extend_from_slice(&chunk);
                },
                Err(error) => {
                    if !pending.is_empty() {
                        yield Ok(Bytes::from(std::mem::take(&mut pending)));
                    }
                    yield Err(error.into());
                    return;
                }
            }
            while let Some(end) = sse_event_end(&pending) {
                let event = pending.drain(..end).collect::<Vec<_>>();
                let (event, diagnostic) = annotate_sse_event(&event);
                if let Some(diagnostic) = diagnostic {
                    on_diagnostic(diagnostic);
                }
                yield Ok(event);
            }
        }
        if !pending.is_empty() {
            let (event, diagnostic) = annotate_sse_event(&pending);
            if let Some(diagnostic) = diagnostic {
                on_diagnostic(diagnostic);
            }
            yield Ok(event);
        }
    };
    Box::pin(stream)
}

pub(crate) fn sse_event_end(bytes: &[u8]) -> Option<usize> {
    (0..bytes.len()).find_map(|index| {
        let remaining = &bytes[index..];
        if remaining.starts_with(b"\n\n") {
            Some(index + 2)
        } else if remaining.starts_with(b"\r\n\r\n") {
            Some(index + 4)
        } else {
            None
        }
    })
}

fn annotate_sse_event(event: &[u8]) -> (Bytes, Option<Diagnostic>) {
    let text = String::from_utf8_lossy(event);
    let Some(diagnostic) = detect(&text) else {
        return (Bytes::copy_from_slice(event), None);
    };
    tracing::warn!(
        diagnostic_code = diagnostic.code,
        diagnostic_summary = diagnostic.summary,
        "provider stream matched Infernaut diagnostic"
    );

    let line_ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut rebuilt = Vec::new();
    let mut decorated = false;
    for line in text.split_inclusive(line_ending) {
        let without_ending = line.strip_suffix(line_ending).unwrap_or(line);
        let data = without_ending
            .strip_prefix("data: ")
            .or_else(|| without_ending.strip_prefix("data:"));
        if !decorated
            && let Some(data) = data
            && let Ok(mut value) = serde_json::from_str::<Value>(data.trim_start())
            && value.is_object()
        {
            annotate_json_value(&mut value, &diagnostic);
            rebuilt.extend_from_slice(b"data: ");
            rebuilt.extend_from_slice(
                &serde_json::to_vec(&value).expect("annotated SSE JSON is serializable"),
            );
            rebuilt.extend_from_slice(line_ending.as_bytes());
            decorated = true;
        } else {
            rebuilt.extend_from_slice(line.as_bytes());
        }
    }
    if decorated {
        (Bytes::from(rebuilt), Some(diagnostic))
    } else {
        let mut preserved = event.to_vec();
        let extension = format!(
            "event: infernaut.diagnostic{line_ending}data: {}{line_ending}{line_ending}",
            serde_json::to_string(&diagnostic).expect("diagnostic is serializable")
        );
        preserved.extend_from_slice(extension.as_bytes());
        (Bytes::from(preserved), Some(diagnostic))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;
    use serde_json::json;

    const METAL_ERROR: &str = "RuntimeError: [Event::Event] Failed to create Metal shared event.";

    #[test]
    fn decorates_json_without_losing_provider_message() {
        let original = json!({ "error": { "message": METAL_ERROR, "type": "provider_error" } });
        let (body, diagnostic) = annotate_json_body(&serde_json::to_vec(&original).unwrap());
        let body: Value = serde_json::from_slice(&body).unwrap();
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.starts_with(METAL_ERROR));
        assert!(message.contains("fully reboot the Mac"));
        assert_eq!(
            body[INFERNAUT_DIAGNOSTICS_KEY][0]["code"],
            "mlx_metal_shared_event_unavailable"
        );
        assert!(diagnostic.is_some());
    }

    #[test]
    fn leaves_unrecognized_json_byte_for_byte_unchanged() {
        let body = br#"{"error":{"message":"something else"}}"#;
        let (annotated, diagnostic) = annotate_json_body(body);
        assert_eq!(&annotated[..], body);
        assert!(diagnostic.is_none());
    }

    #[tokio::test]
    async fn detects_sse_error_split_across_chunks() {
        let first = Bytes::from_static(b"data: {\"error\":{\"message\":\"Failed to create Metal ");
        let second = Bytes::from_static(b"shared event.\"}}\n\ndata: [DONE]\n\n");
        let upstream =
            futures_util::stream::iter(vec![Ok::<_, std::convert::Infallible>(first), Ok(second)]);
        let output = annotate_sse(upstream)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .concat();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Failed to create Metal shared event."));
        assert!(output.contains("mlx_metal_shared_event_unavailable"));
        assert!(output.contains("fully reboot the Mac"));
        assert!(output.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn mixed_sse_delimiters_use_the_first_event_boundary() {
        assert_eq!(sse_event_end(b"one\n\ntwo\r\n\r\n"), Some(5));
        assert_eq!(sse_event_end(b"one\r\n\r\n"), Some(7));
        assert_eq!(sse_event_end(b"one\n"), None);
    }
}
