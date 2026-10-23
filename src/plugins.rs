use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use infernaut::plugin_protocol::{PLUGIN_EVENT_SCHEMA_V1, PluginEvent, PluginEventKind};
use serde::Serialize;
use std::pin::Pin;
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::mpsc::{self, Sender, error::TrySendError},
};
use tracing::{info, warn};

use crate::config::PluginConfig;

#[derive(Clone, Default)]
pub struct PluginManager {
    inner: Arc<PluginManagerInner>,
}

#[derive(Default)]
struct PluginManagerInner {
    observers: Vec<Observer>,
    sequence: AtomicU64,
}

struct Observer {
    id: String,
    events: Vec<PluginEventKind>,
    sender: Sender<Arc<PluginEvent>>,
    counters: Arc<PluginCounters>,
}

#[derive(Default)]
struct PluginCounters {
    delivered: AtomicU64,
    dropped: AtomicU64,
    write_failures: AtomicU64,
    running: AtomicBool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginStatus {
    pub id: String,
    pub mode: &'static str,
    pub running: bool,
    pub delivered_events: u64,
    pub dropped_events: u64,
    pub write_failures: u64,
}

#[derive(Clone, Debug)]
pub struct EventContext {
    pub request_id: String,
    pub endpoint: String,
    pub provider: String,
    pub model: String,
    pub realm: String,
}

impl PluginManager {
    pub fn start(configs: &[PluginConfig]) -> Result<Self> {
        let mut observers = Vec::new();
        for config in configs.iter().filter(|plugin| !plugin.disabled) {
            let (sender, receiver) = mpsc::channel(config.queue_capacity);
            let counters = Arc::new(PluginCounters::default());
            let mut command = Command::new(&config.command);
            command
                .args(&config.args)
                .stdin(Stdio::piped())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .kill_on_drop(true);
            let mut child = command.spawn().with_context(|| {
                format!(
                    "failed to start observer plugin '{}' using {}",
                    config.id, config.command
                )
            })?;
            let stdin = child.stdin.take().context("plugin stdin was not piped")?;
            counters.running.store(true, Ordering::Relaxed);
            tokio::spawn(plugin_writer(
                config.id.clone(),
                receiver,
                stdin,
                child,
                counters.clone(),
            ));
            info!(
                plugin = config.id,
                command = config.command,
                queue_capacity = config.queue_capacity,
                "observer plugin started"
            );
            observers.push(Observer {
                id: config.id.clone(),
                events: config.events.clone(),
                sender,
                counters,
            });
        }
        Ok(Self {
            inner: Arc::new(PluginManagerInner {
                observers,
                sequence: AtomicU64::new(0),
            }),
        })
    }

    pub fn emit(
        &self,
        kind: PluginEventKind,
        context: &EventContext,
        content_type: Option<&str>,
        content: Option<String>,
        diagnostic_code: Option<&str>,
    ) {
        if self.inner.observers.is_empty() {
            return;
        }
        let event = Arc::new(PluginEvent {
            schema: PLUGIN_EVENT_SCHEMA_V1.to_owned(),
            event: kind,
            request_id: context.request_id.to_owned(),
            sequence: self.inner.sequence.fetch_add(1, Ordering::Relaxed),
            timestamp_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            endpoint: context.endpoint.to_owned(),
            provider: context.provider.to_owned(),
            model: context.model.to_owned(),
            realm: context.realm.to_owned(),
            content_type: content_type.map(str::to_owned),
            content,
            diagnostic_code: diagnostic_code.map(str::to_owned),
        });
        for observer in &self.inner.observers {
            if !observer.events.contains(&kind) {
                continue;
            }
            match observer.sender.try_send(event.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    let dropped = observer.counters.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                    if dropped == 1 || dropped.is_power_of_two() {
                        warn!(
                            plugin = observer.id,
                            dropped_events = dropped,
                            "observer plugin queue is full; events are being dropped"
                        );
                    }
                }
                Err(TrySendError::Closed(_)) => {
                    observer.counters.dropped.fetch_add(1, Ordering::Relaxed);
                    observer.counters.running.store(false, Ordering::Relaxed);
                }
            }
        }
    }

    pub fn statuses(&self) -> Vec<PluginStatus> {
        self.inner
            .observers
            .iter()
            .map(|observer| PluginStatus {
                id: observer.id.clone(),
                mode: "observe",
                running: observer.counters.running.load(Ordering::Relaxed),
                delivered_events: observer.counters.delivered.load(Ordering::Relaxed),
                dropped_events: observer.counters.dropped.load(Ordering::Relaxed),
                write_failures: observer.counters.write_failures.load(Ordering::Relaxed),
            })
            .collect()
    }

    pub fn status_map(&self) -> HashMap<String, PluginStatus> {
        self.statuses()
            .into_iter()
            .map(|status| (status.id.clone(), status))
            .collect()
    }

    pub fn observe_stream<S, E>(
        &self,
        upstream: S,
        context: EventContext,
        content_type: String,
    ) -> Pin<Box<dyn Stream<Item = Result<Bytes, E>> + Send>>
    where
        S: Stream<Item = Result<Bytes, E>> + Send + 'static,
        E: Send + 'static,
    {
        let manager = self.clone();
        let stream = async_stream::stream! {
            let mut upstream = Box::pin(upstream);
            while let Some(item) = upstream.next().await {
                match item {
                    Ok(chunk) => {
                        manager.emit(
                            PluginEventKind::ResponseDelta,
                            &context,
                            Some(&content_type),
                            Some(String::from_utf8_lossy(&chunk).into_owned()),
                            None,
                        );
                        yield Ok(chunk);
                    }
                    Err(error) => {
                        manager.emit(
                            PluginEventKind::ProviderError,
                            &context,
                            Some("text/plain"),
                            Some("Provider response stream ended with a transport error.".into()),
                            None,
                        );
                        yield Err(error);
                        return;
                    }
                }
            }
            manager.emit(
                PluginEventKind::ResponseCompleted,
                &context,
                None,
                None,
                None,
            );
        };
        Box::pin(stream)
    }
}

async fn plugin_writer(
    id: String,
    mut receiver: mpsc::Receiver<Arc<PluginEvent>>,
    mut stdin: tokio::process::ChildStdin,
    mut child: tokio::process::Child,
    counters: Arc<PluginCounters>,
) {
    while let Some(event) = receiver.recv().await {
        let mut encoded = match serde_json::to_vec(event.as_ref()) {
            Ok(encoded) => encoded,
            Err(error) => {
                counters.write_failures.fetch_add(1, Ordering::Relaxed);
                warn!(plugin = id, %error, "failed to serialize plugin event");
                continue;
            }
        };
        encoded.push(b'\n');
        if let Err(error) = stdin.write_all(&encoded).await {
            counters.write_failures.fetch_add(1, Ordering::Relaxed);
            warn!(plugin = id, %error, "observer plugin stopped accepting events");
            break;
        }
        counters.delivered.fetch_add(1, Ordering::Relaxed);
    }
    drop(stdin);
    match child.wait().await {
        Ok(status) if status.success() => info!(plugin = id, %status, "observer plugin exited"),
        Ok(status) => warn!(plugin = id, %status, "observer plugin exited unsuccessfully"),
        Err(error) => warn!(plugin = id, %error, "failed waiting for observer plugin"),
    }
    counters.running.store(false, Ordering::Relaxed);
}
