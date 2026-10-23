use serde::{Deserialize, Serialize};

pub const PLUGIN_EVENT_SCHEMA_V1: &str = "infernaut.plugin-event.v1";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub enum PluginEventKind {
    #[serde(rename = "request.content")]
    RequestContent,
    #[serde(rename = "response.delta")]
    ResponseDelta,
    #[serde(rename = "response.completed")]
    ResponseCompleted,
    #[serde(rename = "provider.error")]
    ProviderError,
    #[serde(rename = "diagnostic.matched")]
    DiagnosticMatched,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PluginEvent {
    pub schema: String,
    pub event: PluginEventKind,
    pub request_id: String,
    pub sequence: u64,
    pub timestamp_unix_ms: u64,
    pub endpoint: String,
    pub provider: String,
    pub model: String,
    pub realm: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic_code: Option<String>,
}
