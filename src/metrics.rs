use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use serde::Serialize;

#[derive(Clone, Default)]
pub struct Metrics {
    inner: Arc<Mutex<BTreeMap<String, ModelMetrics>>>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ModelMetrics {
    pub requested_model: String,
    pub resolved_model: String,
    pub provider: String,
    pub requests: u64,
    pub successful_responses: u64,
    pub failed_responses: u64,
    pub total_time_to_headers_ms: u64,
    pub average_time_to_headers_ms: u64,
    pub last_status: u16,
}

#[derive(Serialize)]
pub struct MetricsSnapshot {
    pub note: &'static str,
    pub models: Vec<ModelMetrics>,
}

impl Metrics {
    pub fn record(
        &self,
        requested_model: &str,
        resolved_model: &str,
        provider: &str,
        status: u16,
        time_to_headers_ms: u64,
    ) {
        let key = format!("{requested_model}\0{resolved_model}");
        let mut metrics = self.inner.lock().expect("metrics lock poisoned");
        let entry = metrics.entry(key).or_insert_with(|| ModelMetrics {
            requested_model: requested_model.to_owned(),
            resolved_model: resolved_model.to_owned(),
            provider: provider.to_owned(),
            ..ModelMetrics::default()
        });
        entry.requests += 1;
        if (200..400).contains(&status) {
            entry.successful_responses += 1;
        } else {
            entry.failed_responses += 1;
        }
        entry.total_time_to_headers_ms += time_to_headers_ms;
        entry.average_time_to_headers_ms = entry.total_time_to_headers_ms / entry.requests;
        entry.last_status = status;
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let metrics = self.inner.lock().expect("metrics lock poisoned");
        MetricsSnapshot {
            note: "V1 metrics are process-local and contain no prompt or response content.",
            models: metrics.values().cloned().collect(),
        }
    }
}
