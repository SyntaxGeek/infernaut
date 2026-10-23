use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    hash::{Hash, Hasher},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::{
    catalog::{Catalog, DiscoveredModel},
    config::{Config, ProbeMode, ProviderConfig},
};

const PROBE_VERSION: &str = "conformance.v1";
const PROBE_TESTS: [&str; 4] = ["chat", "streaming", "tools", "structured_output"];

#[derive(Clone)]
pub struct ResourceCoordinator {
    active_inference: Arc<AtomicUsize>,
    last_inference_finished_ms: Arc<AtomicU64>,
    inference_started: Arc<Notify>,
}

impl ResourceCoordinator {
    pub fn new() -> Self {
        Self {
            active_inference: Arc::new(AtomicUsize::new(0)),
            last_inference_finished_ms: Arc::new(AtomicU64::new(unix_millis())),
            inference_started: Arc::new(Notify::new()),
        }
    }

    pub fn begin_inference(&self) -> InferenceGuard {
        self.active_inference.fetch_add(1, Ordering::SeqCst);
        self.inference_started.notify_waiters();
        InferenceGuard {
            coordinator: self.clone(),
        }
    }

    fn has_active_inference(&self) -> bool {
        self.active_inference.load(Ordering::SeqCst) > 0
    }

    async fn wait_until_idle(
        &self,
        grace: Duration,
        control: &JobControl,
    ) -> Result<(), ProbeAbort> {
        loop {
            if control.cancelled.load(Ordering::SeqCst) {
                return Err(ProbeAbort::Cancelled);
            }
            let last_finished = self.last_inference_finished_ms.load(Ordering::SeqCst);
            let idle_for = unix_millis().saturating_sub(last_finished);
            if !self.has_active_inference() && idle_for >= grace.as_millis() as u64 {
                return Ok(());
            }
            tokio::select! {
                _ = control.notify.notified() => return Err(ProbeAbort::Cancelled),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    }
}

pub struct InferenceGuard {
    coordinator: ResourceCoordinator,
}

impl Drop for InferenceGuard {
    fn drop(&mut self) {
        self.coordinator
            .active_inference
            .fetch_sub(1, Ordering::SeqCst);
        self.coordinator
            .last_inference_finished_ms
            .store(unix_millis(), Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct ProbeManager {
    config: Arc<Config>,
    catalog: Arc<RwLock<Catalog>>,
    client: reqwest::Client,
    resources: ResourceCoordinator,
    jobs: Arc<RwLock<BTreeMap<String, ProbeJob>>>,
    controls: Arc<Mutex<HashMap<String, Arc<JobControl>>>>,
    evidence: Arc<RwLock<HashMap<String, ProbeEvidence>>>,
    runner: Arc<Mutex<()>>,
    sequence: Arc<AtomicU64>,
}

#[derive(Debug)]
struct JobControl {
    cancelled: AtomicBool,
    notify: Notify,
}

impl JobControl {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProbePlan {
    pub requested_model: String,
    pub resolved_model: String,
    pub fingerprint: String,
    pub provider: String,
    pub realm: String,
    pub loaded: Option<bool>,
    pub would_cold_start: bool,
    pub tests: Vec<String>,
    pub max_output_tokens_per_test: u32,
    pub timeout_seconds_per_operation: u64,
    pub estimated_max_requests: usize,
    pub cached_evidence: bool,
    pub runtime_state_may_change: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProbeJob {
    pub id: String,
    pub model: String,
    pub fingerprint: String,
    pub allow_cold_start: bool,
    pub status: ProbeJobStatus,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u64>,
    pub plan: ProbePlan,
    #[serde(default)]
    pub results: Vec<ProbeTestResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub cached: bool,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeJobStatus {
    Queued,
    WaitingForIdle,
    Running,
    Completed,
    Cancelled,
    Preempted,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProbeTestResult {
    pub test: String,
    pub passed: bool,
    pub latency_ms: u64,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProbeEvidence {
    pub model: String,
    pub fingerprint: String,
    pub probe_version: String,
    pub tested_at: u64,
    pub results: Vec<ProbeTestResult>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProbeRequest {
    pub model: String,
    #[serde(default)]
    pub allow_cold_start: bool,
}

#[derive(Debug)]
pub enum ProbeError {
    Disabled,
    Model(String),
    ColdStartDenied(String),
    NotFound(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(formatter, "manual probes are disabled"),
            Self::Model(message) => formatter.write_str(message),
            Self::ColdStartDenied(model) => write!(
                formatter,
                "model '{model}' is not loaded; pass allow_cold_start=true explicitly"
            ),
            Self::NotFound(id) => write!(formatter, "probe job '{id}' was not found"),
        }
    }
}

#[derive(Debug)]
enum ProbeAbort {
    Cancelled,
    Preempted,
    Timeout,
    Request(String),
}

impl ProbeManager {
    pub fn new(
        config: Arc<Config>,
        catalog: Arc<RwLock<Catalog>>,
        client: reqwest::Client,
        resources: ResourceCoordinator,
    ) -> Self {
        Self {
            config,
            catalog,
            client,
            resources,
            jobs: Arc::new(RwLock::new(BTreeMap::new())),
            controls: Arc::new(Mutex::new(HashMap::new())),
            evidence: Arc::new(RwLock::new(HashMap::new())),
            runner: Arc::new(Mutex::new(())),
            sequence: Arc::new(AtomicU64::new(1)),
        }
    }

    pub async fn plan(&self, requested_model: &str) -> Result<ProbePlan, ProbeError> {
        let catalog = self.catalog.read().await;
        let resolved = catalog
            .resolve(&self.config, requested_model)
            .map_err(|error| ProbeError::Model(error.to_string()))?;
        let model = catalog
            .models
            .iter()
            .find(|model| model.id == resolved.qualified_id)
            .ok_or_else(|| ProbeError::Model("resolved model disappeared from catalog".into()))?;
        let fingerprint = model_fingerprint(model);
        let cached_evidence = self.evidence.read().await.contains_key(&fingerprint);
        let would_cold_start = model.loaded == Some(false);
        Ok(ProbePlan {
            requested_model: requested_model.to_owned(),
            resolved_model: model.id.clone(),
            fingerprint,
            provider: model.provider.clone(),
            realm: model.realm.clone(),
            loaded: model.loaded,
            would_cold_start,
            tests: PROBE_TESTS.iter().map(|test| (*test).to_owned()).collect(),
            max_output_tokens_per_test: self.config.probes.max_output_tokens,
            timeout_seconds_per_operation: self.config.probes.timeout_seconds,
            estimated_max_requests: PROBE_TESTS.len(),
            cached_evidence,
            runtime_state_may_change: would_cold_start,
        })
    }

    pub async fn submit(&self, request: ProbeRequest) -> Result<ProbeJob, ProbeError> {
        if self.config.probes.mode == ProbeMode::Off {
            return Err(ProbeError::Disabled);
        }
        let plan = self.plan(&request.model).await?;
        if plan.would_cold_start
            && self.config.probes.only_loaded_models
            && !request.allow_cold_start
        {
            return Err(ProbeError::ColdStartDenied(plan.resolved_model));
        }

        let id = format!(
            "probe-{}-{}",
            unix_seconds(),
            self.sequence.fetch_add(1, Ordering::SeqCst)
        );
        if let Some(cached) = self.evidence.read().await.get(&plan.fingerprint).cloned() {
            let job = ProbeJob {
                id,
                model: plan.resolved_model.clone(),
                fingerprint: plan.fingerprint.clone(),
                allow_cold_start: request.allow_cold_start,
                status: ProbeJobStatus::Completed,
                created_at: unix_seconds(),
                started_at: Some(cached.tested_at),
                completed_at: Some(cached.tested_at),
                plan,
                results: cached.results,
                error: None,
                cached: true,
            };
            self.store_job(job.clone()).await?;
            return Ok(job);
        }

        let job = ProbeJob {
            id: id.clone(),
            model: plan.resolved_model.clone(),
            fingerprint: plan.fingerprint.clone(),
            allow_cold_start: request.allow_cold_start,
            status: ProbeJobStatus::Queued,
            created_at: unix_seconds(),
            started_at: None,
            completed_at: None,
            plan,
            results: Vec::new(),
            error: None,
            cached: false,
        };
        let control = Arc::new(JobControl::new());
        self.store_job(job.clone()).await?;
        self.controls
            .lock()
            .await
            .insert(id.clone(), control.clone());
        let manager = self.clone();
        tokio::spawn(async move {
            manager.run_job(id.clone(), control).await;
            manager.controls.lock().await.remove(&id);
        });
        Ok(job)
    }

    async fn store_job(&self, job: ProbeJob) -> Result<(), ProbeError> {
        let mut jobs = self.jobs.write().await;
        if jobs.len() >= 1024 {
            if let Some(oldest) = jobs
                .iter()
                .filter(|(_, job)| job.completed_at.is_some())
                .min_by_key(|(_, job)| job.created_at)
                .map(|(id, _)| id.clone())
            {
                jobs.remove(&oldest);
            } else {
                return Err(ProbeError::Model(
                    "probe job capacity reached; wait for existing jobs".into(),
                ));
            }
        }
        jobs.insert(job.id.clone(), job);
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<ProbeJob, ProbeError> {
        self.jobs
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| ProbeError::NotFound(id.to_owned()))
    }

    pub async fn cancel(&self, id: &str) -> Result<ProbeJob, ProbeError> {
        let control = self
            .controls
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| ProbeError::NotFound(id.to_owned()))?;
        control.cancel();
        self.get(id).await
    }

    pub async fn evidence_for_model(
        &self,
        requested_model: &str,
    ) -> Result<Option<ProbeEvidence>, ProbeError> {
        let plan = self.plan(requested_model).await?;
        Ok(self.evidence.read().await.get(&plan.fingerprint).cloned())
    }

    async fn run_job(&self, id: String, control: Arc<JobControl>) {
        let _runner = self.runner.lock().await;
        self.set_status(&id, ProbeJobStatus::WaitingForIdle, None)
            .await;
        let grace = Duration::from_secs(self.config.probes.idle_grace_seconds);
        if let Err(abort) = self.resources.wait_until_idle(grace, &control).await {
            self.finish_aborted(&id, abort).await;
            return;
        }
        self.set_status(&id, ProbeJobStatus::Running, None).await;

        let (model, provider) = match self.model_and_provider(&id).await {
            Ok(value) => value,
            Err(error) => {
                self.set_status(&id, ProbeJobStatus::Failed, Some(error))
                    .await;
                return;
            }
        };
        let upstream_model = if model.loaded == Some(false) {
            if !self
                .jobs
                .read()
                .await
                .get(&id)
                .is_some_and(|job| job.allow_cold_start)
            {
                self.set_status(
                    &id,
                    ProbeJobStatus::Failed,
                    Some("cold start was not authorized".into()),
                )
                .await;
                return;
            }
            if provider.jit_load {
                match &model.management_id {
                    Some(id) => id.clone(),
                    None => {
                        self.set_status(
                            &id,
                            ProbeJobStatus::Failed,
                            Some("provider management ID is missing".into()),
                        )
                        .await;
                        return;
                    }
                }
            } else {
                self.set_status(
                    &id,
                    ProbeJobStatus::Failed,
                    Some("cold-start probes currently require provider jit_load=true".into()),
                )
                .await;
                return;
            }
        } else {
            model.upstream_id.clone()
        };

        let mut results = Vec::new();
        for test in PROBE_TESTS {
            if control.cancelled.load(Ordering::SeqCst) {
                self.finish_aborted(&id, ProbeAbort::Cancelled).await;
                return;
            }
            match self
                .run_test(test, &provider, &upstream_model, &control)
                .await
            {
                Ok(result) => results.push(result),
                Err(abort @ (ProbeAbort::Cancelled | ProbeAbort::Preempted)) => {
                    self.finish_aborted(&id, abort).await;
                    return;
                }
                Err(ProbeAbort::Timeout) => results.push(ProbeTestResult {
                    test: test.to_owned(),
                    passed: false,
                    latency_ms: self.config.probes.timeout_seconds * 1000,
                    detail: "probe operation timed out".into(),
                }),
                Err(ProbeAbort::Request(error)) => results.push(ProbeTestResult {
                    test: test.to_owned(),
                    passed: false,
                    latency_ms: 0,
                    detail: error,
                }),
            }
        }

        let tested_at = unix_seconds();
        let fingerprint = self
            .jobs
            .read()
            .await
            .get(&id)
            .map(|job| job.fingerprint.clone())
            .unwrap_or_default();
        let evidence = ProbeEvidence {
            model: model.id.clone(),
            fingerprint: fingerprint.clone(),
            probe_version: PROBE_VERSION.into(),
            tested_at,
            results: results.clone(),
        };
        {
            let mut cache = self.evidence.write().await;
            if cache.len() >= 1024
                && !cache.contains_key(&fingerprint)
                && let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, evidence)| evidence.tested_at)
                    .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
            cache.insert(fingerprint, evidence);
        }
        self.apply_evidence_to_catalog(&model.id, &results, tested_at)
            .await;
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(&id) {
            job.status = ProbeJobStatus::Completed;
            job.completed_at = Some(tested_at);
            job.results = results;
        }
    }

    async fn model_and_provider(
        &self,
        id: &str,
    ) -> Result<(DiscoveredModel, ProviderConfig), String> {
        let model_id = self
            .jobs
            .read()
            .await
            .get(id)
            .map(|job| job.model.clone())
            .ok_or_else(|| "probe job disappeared".to_owned())?;
        let model = self
            .catalog
            .read()
            .await
            .models
            .iter()
            .find(|model| model.id == model_id)
            .cloned()
            .ok_or_else(|| "model disappeared from catalog".to_owned())?;
        let provider = self
            .config
            .provider(&model.provider)
            .cloned()
            .ok_or_else(|| "provider is no longer eligible".to_owned())?;
        Ok((model, provider))
    }

    async fn run_test(
        &self,
        test: &str,
        provider: &ProviderConfig,
        model: &str,
        control: &JobControl,
    ) -> Result<ProbeTestResult, ProbeAbort> {
        let started = Instant::now();
        let payload = probe_payload(test, model, self.config.probes.max_output_tokens);
        let url = format!(
            "{}/chat/completions",
            provider.effective_base_url().trim_end_matches('/')
        );
        let mut request = self.client.post(url).json(&payload);
        if let Some(api_key) = provider.api_key() {
            request = request.bearer_auth(api_key);
        }
        let response = self.interruptible(request.send(), control).await?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = self.interruptible(response.bytes(), control).await?;
        let latency_ms = started.elapsed().as_millis() as u64;
        if !status.is_success() {
            return Ok(ProbeTestResult {
                test: test.to_owned(),
                passed: false,
                latency_ms,
                detail: format!(
                    "provider returned HTTP {}: {}",
                    status.as_u16(),
                    truncate(&String::from_utf8_lossy(&body), 240)
                ),
            });
        }

        let (passed, detail) = evaluate_probe(test, &content_type, &body);
        Ok(ProbeTestResult {
            test: test.to_owned(),
            passed,
            latency_ms,
            detail,
        })
    }

    async fn interruptible<F, T>(&self, future: F, control: &JobControl) -> Result<T, ProbeAbort>
    where
        F: Future<Output = Result<T, reqwest::Error>>,
    {
        if control.cancelled.load(Ordering::SeqCst) {
            return Err(ProbeAbort::Cancelled);
        }
        if self.resources.has_active_inference() {
            return Err(ProbeAbort::Preempted);
        }
        let timeout = Duration::from_secs(self.config.probes.timeout_seconds);
        tokio::select! {
            biased;
            _ = control.notify.notified() => Err(ProbeAbort::Cancelled),
            _ = self.resources.inference_started.notified(), if self.config.probes.cancel_on_inference => Err(ProbeAbort::Preempted),
            result = tokio::time::timeout(timeout, future) => match result {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(ProbeAbort::Request(error.to_string())),
                Err(_) => Err(ProbeAbort::Timeout),
            }
        }
    }

    async fn apply_evidence_to_catalog(
        &self,
        model_id: &str,
        results: &[ProbeTestResult],
        tested_at: u64,
    ) {
        let mut catalog = self.catalog.write().await;
        let Some(model) = catalog.models.iter_mut().find(|model| model.id == model_id) else {
            return;
        };
        for result in results {
            let capability = match result.test.as_str() {
                "chat" => "chat",
                "streaming" => "streaming",
                "tools" => "tools",
                "structured_output" => "structured_output",
                _ => continue,
            };
            model.capabilities[capability]["probe"] = json!({
                "passed": result.passed,
                "tested_at": tested_at,
                "probe_version": PROBE_VERSION,
                "latency_ms": result.latency_ms,
                "detail": result.detail,
            });
            if result.passed {
                model.capabilities[capability]["support"] = json!("native");
                model.capabilities[capability]["confidence"] = json!("probed");
            }
        }
    }

    async fn set_status(&self, id: &str, status: ProbeJobStatus, error: Option<String>) {
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(id) {
            job.status = status;
            if status == ProbeJobStatus::Running && job.started_at.is_none() {
                job.started_at = Some(unix_seconds());
            }
            if matches!(
                status,
                ProbeJobStatus::Cancelled | ProbeJobStatus::Preempted | ProbeJobStatus::Failed
            ) {
                job.completed_at = Some(unix_seconds());
            }
            job.error = error;
        }
    }

    async fn finish_aborted(&self, id: &str, abort: ProbeAbort) {
        let (status, message) = match abort {
            ProbeAbort::Cancelled => (ProbeJobStatus::Cancelled, "cancelled by user"),
            ProbeAbort::Preempted => (
                ProbeJobStatus::Preempted,
                "preempted because an inference request arrived",
            ),
            ProbeAbort::Timeout => (ProbeJobStatus::Failed, "probe timed out"),
            ProbeAbort::Request(_) => (ProbeJobStatus::Failed, "probe request failed"),
        };
        self.set_status(id, status, Some(message.into())).await;
    }
}

fn probe_payload(test: &str, model: &str, max_tokens: u32) -> Value {
    let mut payload = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "Reply with exactly OK." }],
        "max_tokens": max_tokens,
        "temperature": 0,
        "stream": test == "streaming",
    });
    if test == "tools" {
        payload["messages"] = json!([{
            "role": "user",
            "content": "Call infernaut_probe exactly once with value set to ok. Do not answer in text."
        }]);
        payload["tools"] = json!([{
            "type": "function",
            "function": {
                "name": "infernaut_probe",
                "description": "A harmless Infernaut conformance probe.",
                "parameters": {
                    "type": "object",
                    "properties": { "value": { "type": "string", "const": "ok" } },
                    "required": ["value"],
                    "additionalProperties": false
                }
            }
        }]);
        payload["tool_choice"] = json!({
            "type": "function",
            "function": { "name": "infernaut_probe" }
        });
    } else if test == "structured_output" {
        payload["messages"] = json!([{
            "role": "user",
            "content": "Return a JSON object with ok set to true."
        }]);
        payload["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {
                "name": "infernaut_probe",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": { "ok": { "type": "boolean", "const": true } },
                    "required": ["ok"],
                    "additionalProperties": false
                }
            }
        });
    }
    payload
}

fn evaluate_probe(test: &str, content_type: &str, body: &[u8]) -> (bool, String) {
    if test == "streaming" {
        let text = String::from_utf8_lossy(body);
        let passed = content_type.contains("text/event-stream") && text.contains("[DONE]");
        return (
            passed,
            if passed {
                "received a complete SSE stream".into()
            } else {
                "response was not a complete SSE stream".into()
            },
        );
    }

    let payload: Value = match serde_json::from_slice(body) {
        Ok(payload) => payload,
        Err(error) => return (false, format!("response was not valid JSON: {error}")),
    };
    if test == "chat" {
        let passed = payload
            .pointer("/choices/0/message")
            .is_some_and(Value::is_object);
        return (
            passed,
            if passed {
                "received an assistant message".into()
            } else {
                "assistant message was missing".into()
            },
        );
    }
    if test == "tools" {
        let calls = payload
            .pointer("/choices/0/message/tool_calls")
            .and_then(Value::as_array);
        let passed = calls.is_some_and(|calls| {
            calls.iter().any(|call| {
                call.pointer("/function/name").and_then(Value::as_str) == Some("infernaut_probe")
                    && call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
                        .and_then(|arguments| arguments.get("value").cloned())
                        == Some(json!("ok"))
            })
        });
        return (
            passed,
            if passed {
                "received valid tool-call arguments".into()
            } else {
                "valid tool call was not produced".into()
            },
        );
    }

    let content = payload
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str);
    let passed = content
        .and_then(|content| serde_json::from_str::<Value>(content).ok())
        .and_then(|content| content.get("ok").cloned())
        == Some(json!(true));
    (
        passed,
        if passed {
            "received schema-conformant JSON".into()
        } else {
            "schema-conformant JSON was not produced".into()
        },
    )
}

fn model_fingerprint(model: &DiscoveredModel) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    PROBE_VERSION.hash(&mut hasher);
    model.id.hash(&mut hasher);
    model.provider.hash(&mut hasher);
    model.upstream_id.hash(&mut hasher);
    model.management_id.hash(&mut hasher);
    model.provider_metadata.to_string().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_probe_requires_sse_and_done_marker() {
        assert!(evaluate_probe("streaming", "text/event-stream", b"data: [DONE]\n").0);
        assert!(!evaluate_probe("streaming", "application/json", b"data: [DONE]\n").0);
    }

    #[test]
    fn tool_probe_validates_name_and_json_arguments() {
        let body = br#"{"choices":[{"message":{"tool_calls":[{"function":{"name":"infernaut_probe","arguments":"{\"value\":\"ok\"}"}}]}}]}"#;
        assert!(evaluate_probe("tools", "application/json", body).0);
    }

    #[test]
    fn structured_probe_validates_returned_content() {
        let body = br#"{"choices":[{"message":{"content":"{\"ok\":true}"}}]}"#;
        assert!(evaluate_probe("structured_output", "application/json", body).0);
    }
}
