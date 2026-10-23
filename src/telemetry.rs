use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures_util::Stream;
use http_body_util::{StreamBody, combinators::UnsyncBoxBody};
use hyper::body::Frame;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

const DEFAULT_HISTORY_CAPACITY: usize = 1_024;
const DEFAULT_LIVE_CAPACITY: usize = 256;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type TelemetryBody = UnsyncBoxBody<Bytes, BoxError>;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ClientIdentity {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryStage {
    Accepted,
    Classified,
    Loading,
    Forwarding,
    Headers,
    Streaming,
    Completed,
    Failed,
    Disconnected,
}

impl TelemetryStage {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Disconnected)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TelemetryEvent {
    pub sequence: u64,
    pub timestamp_unix_ms: u64,
    pub request_id: String,
    pub stage: TelemetryStage,
    pub endpoint: String,
    pub client: ClientIdentity,
    pub requested_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub compression: bool,
    pub streaming: bool,
    pub request_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_headers_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_first_byte_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_last_byte_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RequestTelemetry {
    pub request_id: String,
    pub endpoint: String,
    pub client: ClientIdentity,
    pub requested_model: String,
    pub route: Option<String>,
    pub provider: Option<String>,
    pub resolved_model: Option<String>,
    pub realm: Option<String>,
    pub task: Option<String>,
    pub operation: Option<String>,
    pub effort: Option<String>,
    pub compression: bool,
    pub streaming: bool,
    pub request_bytes: u64,
    pub estimated_tokens: Option<u64>,
    pub activation_ms: Option<u64>,
}

impl RequestTelemetry {
    pub fn event(&self, stage: TelemetryStage) -> TelemetryEvent {
        TelemetryEvent {
            sequence: 0,
            timestamp_unix_ms: 0,
            request_id: self.request_id.clone(),
            stage,
            endpoint: self.endpoint.clone(),
            client: self.client.clone(),
            requested_model: self.requested_model.clone(),
            route: self.route.clone(),
            provider: self.provider.clone(),
            resolved_model: self.resolved_model.clone(),
            realm: self.realm.clone(),
            task: self.task.clone(),
            operation: self.operation.clone(),
            effort: self.effort.clone(),
            compression: self.compression,
            streaming: self.streaming,
            request_bytes: self.request_bytes,
            estimated_tokens: self.estimated_tokens,
            response_bytes: None,
            status: None,
            time_to_headers_ms: None,
            time_to_first_byte_ms: None,
            time_to_last_byte_ms: None,
            activation_ms: self.activation_ms,
            elapsed_ms: None,
            detail: None,
        }
    }
}

#[derive(Clone)]
pub struct TelemetryHub {
    inner: Arc<TelemetryHubInner>,
}

struct TelemetryHubInner {
    sequence: AtomicU64,
    history: Mutex<VecDeque<Arc<TelemetryEvent>>>,
    active: Mutex<HashMap<String, Arc<TelemetryEvent>>>,
    history_capacity: usize,
    sender: broadcast::Sender<Arc<TelemetryEvent>>,
}

impl Default for TelemetryHub {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY_CAPACITY, DEFAULT_LIVE_CAPACITY)
    }
}

impl TelemetryHub {
    pub fn new(history_capacity: usize, live_capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(live_capacity.max(1));
        Self {
            inner: Arc::new(TelemetryHubInner {
                sequence: AtomicU64::new(1),
                history: Mutex::new(VecDeque::with_capacity(history_capacity)),
                active: Mutex::new(HashMap::new()),
                history_capacity,
                sender,
            }),
        }
    }

    pub fn emit(&self, mut event: TelemetryEvent) {
        event.sequence = self.inner.sequence.fetch_add(1, Ordering::Relaxed);
        event.timestamp_unix_ms = now_unix_ms();
        let event = Arc::new(event);
        {
            let mut active = self
                .inner
                .active
                .lock()
                .expect("telemetry active registry poisoned");
            if event.stage.is_terminal() {
                active.remove(&event.request_id);
            } else {
                active.insert(event.request_id.clone(), event.clone());
            }
        }
        if self.inner.history_capacity > 0 {
            let mut history = self
                .inner
                .history
                .lock()
                .expect("telemetry history poisoned");
            while history.len() >= self.inner.history_capacity {
                history.pop_front();
            }
            history.push_back(event.clone());
        }
        let _ = self.inner.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<TelemetryEvent>> {
        self.inner.sender.subscribe()
    }

    pub fn pre_response_guard(&self, request_id: &str, started: Instant) -> PreResponseGuard {
        PreResponseGuard {
            hub: self.clone(),
            request_id: request_id.to_owned(),
            started,
            armed: true,
        }
    }

    fn disconnect_if_active(&self, request_id: &str, elapsed_ms: u64) {
        let latest = self
            .inner
            .active
            .lock()
            .expect("telemetry active registry poisoned")
            .remove(request_id);
        if let Some(latest) = latest {
            let mut event = (*latest).clone();
            event.stage = TelemetryStage::Disconnected;
            event.status = None;
            event.elapsed_ms = Some(elapsed_ms);
            event.detail = Some("client disconnected before a response was returned".to_owned());
            self.emit(event);
        }
    }

    pub fn history(&self) -> Vec<Arc<TelemetryEvent>> {
        self.inner
            .history
            .lock()
            .expect("telemetry history poisoned")
            .iter()
            .cloned()
            .collect()
    }

    pub fn sse_body(&self) -> TelemetryBody {
        let history = self.history();
        let mut receiver = self.subscribe();
        let stream = async_stream::stream! {
            for event in history {
                yield Ok::<Frame<Bytes>, Infallible>(Frame::data(encode_sse(&event)));
            }
            loop {
                match receiver.recv().await {
                    Ok(event) => yield Ok(Frame::data(encode_sse(&event))),
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        let notice = format!("event: lagged\ndata: {{\"skipped\":{skipped}}}\n\n");
                        yield Ok(Frame::data(Bytes::from(notice)));
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };
        use http_body_util::BodyExt;
        StreamBody::new(stream)
            .map_err(|never| -> BoxError { match never {} })
            .boxed_unsync()
    }

    pub fn observe_stream<S, E>(
        &self,
        upstream: S,
        request: RequestTelemetry,
        started: Instant,
        status: u16,
        time_to_headers_ms: u64,
    ) -> ObservedByteStream<S>
    where
        S: Stream<Item = Result<Bytes, E>>,
    {
        ObservedByteStream {
            inner: Box::pin(upstream),
            hub: self.clone(),
            request,
            started,
            status,
            time_to_headers_ms,
            response_bytes: 0,
            first_byte_emitted: false,
            first_byte_ms: None,
            last_byte_ms: None,
            finished: false,
        }
    }
}

pub struct ObservedByteStream<S>
where
    S: Stream,
{
    inner: Pin<Box<S>>,
    hub: TelemetryHub,
    request: RequestTelemetry,
    started: Instant,
    status: u16,
    time_to_headers_ms: u64,
    response_bytes: u64,
    first_byte_emitted: bool,
    first_byte_ms: Option<u64>,
    last_byte_ms: Option<u64>,
    finished: bool,
}

impl<S, E> Stream for ObservedByteStream<S>
where
    S: Stream<Item = Result<Bytes, E>>,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.response_bytes += bytes.len() as u64;
                let byte_elapsed_ms = self.started.elapsed().as_millis() as u64;
                self.last_byte_ms = Some(byte_elapsed_ms);
                if !self.first_byte_emitted {
                    self.first_byte_emitted = true;
                    let first_byte_ms = byte_elapsed_ms;
                    self.first_byte_ms = Some(first_byte_ms);
                    let mut event = self.request.event(TelemetryStage::Streaming);
                    event.status = Some(self.status);
                    event.time_to_headers_ms = Some(self.time_to_headers_ms);
                    event.time_to_first_byte_ms = Some(first_byte_ms);
                    event.response_bytes = Some(self.response_bytes);
                    self.hub.emit(event);
                }
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.finish(
                    TelemetryStage::Failed,
                    Some("provider stream transport error"),
                );
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.finish(TelemetryStage::Completed, None);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> ObservedByteStream<S>
where
    S: Stream,
{
    fn finish(&mut self, stage: TelemetryStage, detail: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut event = self.request.event(stage);
        event.status = Some(self.status);
        event.time_to_headers_ms = Some(self.time_to_headers_ms);
        event.time_to_first_byte_ms = self.first_byte_ms;
        event.time_to_last_byte_ms = self.last_byte_ms;
        event.elapsed_ms = Some(self.started.elapsed().as_millis() as u64);
        event.response_bytes = Some(self.response_bytes);
        event.detail = detail.map(str::to_owned);
        self.hub.emit(event);
    }
}

pub struct PreResponseGuard {
    hub: TelemetryHub,
    request_id: String,
    started: Instant,
    armed: bool,
}

impl PreResponseGuard {
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PreResponseGuard {
    fn drop(&mut self) {
        if self.armed {
            self.hub
                .disconnect_if_active(&self.request_id, self.started.elapsed().as_millis() as u64);
        }
    }
}

impl<S> Drop for ObservedByteStream<S>
where
    S: Stream,
{
    fn drop(&mut self) {
        self.finish(
            TelemetryStage::Disconnected,
            Some("client stopped consuming the response stream"),
        );
    }
}

fn encode_sse(event: &TelemetryEvent) -> Bytes {
    let encoded = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_owned());
    Bytes::from(format!("id: {}\ndata: {encoded}\n\n", event.sequence))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_event(stage: TelemetryStage) -> TelemetryEvent {
        RequestTelemetry {
            request_id: "inf_test".into(),
            endpoint: "/v1/chat/completions".into(),
            client: ClientIdentity {
                label: "Hermes".into(),
                ..ClientIdentity::default()
            },
            requested_model: "route:private:auto".into(),
            route: Some("route:private:auto".into()),
            provider: None,
            resolved_model: None,
            realm: None,
            task: None,
            operation: None,
            effort: None,
            compression: false,
            streaming: true,
            request_bytes: 42,
            estimated_tokens: None,
            activation_ms: None,
        }
        .event(stage)
    }

    #[test]
    fn history_is_bounded_and_sequenced() {
        let hub = TelemetryHub::new(2, 4);
        hub.emit(test_event(TelemetryStage::Accepted));
        hub.emit(test_event(TelemetryStage::Classified));
        hub.emit(test_event(TelemetryStage::Forwarding));
        let history = hub.history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].sequence, 2);
        assert_eq!(history[1].sequence, 3);
    }

    #[test]
    fn pre_response_guard_closes_abandoned_requests_once() {
        let hub = TelemetryHub::new(8, 8);
        let accepted = test_event(TelemetryStage::Accepted);
        hub.emit(accepted.clone());
        {
            let _guard = hub.pre_response_guard(&accepted.request_id, Instant::now());
        }
        let history = hub.history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].stage, TelemetryStage::Disconnected);

        let hub = TelemetryHub::new(8, 8);
        hub.emit(accepted.clone());
        {
            let _guard = hub.pre_response_guard(&accepted.request_id, Instant::now());
            hub.emit(test_event(TelemetryStage::Completed));
        }
        let history = hub.history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].stage, TelemetryStage::Completed);
    }

    #[tokio::test]
    async fn first_stream_bytes_are_distinct_from_headers_and_completion() {
        use futures_util::{StreamExt, stream};

        let hub = TelemetryHub::new(8, 8);
        let request = RequestTelemetry {
            request_id: "inf_stream".into(),
            endpoint: "/v1/messages".into(),
            client: ClientIdentity {
                label: "Claude Code".into(),
                ..ClientIdentity::default()
            },
            requested_model: "route:private:auto".into(),
            route: Some("route:private:auto".into()),
            provider: Some("agamotto".into()),
            resolved_model: Some("agamotto/model".into()),
            realm: Some("local".into()),
            task: Some("chat".into()),
            operation: Some("converse".into()),
            effort: Some("standard".into()),
            compression: false,
            streaming: true,
            request_bytes: 100,
            estimated_tokens: Some(40),
            activation_ms: None,
        };
        hub.emit(request.event(TelemetryStage::Headers));
        let upstream = stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from_static(b"data"))]);
        let output = hub
            .observe_stream(upstream, request, Instant::now(), 200, 5)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(output.len(), 1);
        let stages = hub
            .history()
            .iter()
            .map(|event| event.stage)
            .collect::<Vec<_>>();
        assert_eq!(
            stages,
            vec![
                TelemetryStage::Headers,
                TelemetryStage::Streaming,
                TelemetryStage::Completed
            ]
        );
    }
}
