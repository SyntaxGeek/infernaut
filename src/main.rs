mod app;
mod auth;
mod catalog;
mod cli;
mod codex_adapter;
mod config;
mod config_editor;
mod config_tui;
mod diagnostics;
mod environment;
mod metrics;
mod mission_audio;
mod observe_tui;
mod plugins;
mod probes;
mod routing;
mod storage;
mod telemetry;

use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use hyper::{Request, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock},
    time,
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::plugins::PluginManager;
use crate::probes::{ProbeManager, ResourceCoordinator};
use crate::routing::RoutingEngine;
use crate::{app::AppState, auth::AuthManager, catalog::Catalog, config::Config, metrics::Metrics};

fn main() -> Result<()> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let config_hint = config_path_hint(&arguments);
    let dotenv = environment::load(Some(&config_hint))?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "infernaut=info".into()),
        )
        .with_target(false)
        .compact()
        .init();

    if let Some(report) = dotenv
        && arguments
            .first()
            .is_none_or(|argument| argument != "configure")
    {
        info!(
            path = %report.path.display(),
            loaded = report.loaded,
            preserved = report.preserved,
            "environment file loaded"
        );
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create async runtime")?
        .block_on(async_main(arguments))
}

async fn async_main(arguments: Vec<String>) -> Result<()> {
    if arguments
        .first()
        .is_some_and(|argument| matches!(argument.as_str(), "--help" | "-h" | "help"))
    {
        cli::print_help();
        return Ok(());
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "probe")
    {
        return cli::run(&arguments[1..]).await;
    }
    if arguments.first().is_some_and(|argument| argument == "auth") {
        return cli::run_auth(&arguments[1..]).await;
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "configure")
    {
        if arguments
            .iter()
            .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
        {
            cli::print_config_help();
            return Ok(());
        }
        let offline = arguments.iter().any(|argument| argument == "--offline");
        let config_path = arguments
            .iter()
            .skip(1)
            .find(|argument| !argument.starts_with('-'))
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("INFERNAUT_CONFIG").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("infernaut.toml"));
        return config_tui::run(config_path, offline).await;
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "observe")
    {
        if arguments
            .iter()
            .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
        {
            cli::print_observe_help();
            return Ok(());
        }
        let mut config_argument = None;
        let mut url = std::env::var("INFERNAUT_URL")
            .ok()
            .filter(|url| !url.is_empty());
        let mut reduced_motion = false;
        let mut sound = std::env::var("INFERNAUT_OBSERVER_SOUND").is_ok_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        });
        let mut index = 1;
        while index < arguments.len() {
            if arguments[index] == "--url" {
                url = arguments.get(index + 1).cloned();
                index += 2;
            } else if arguments[index] == "--reduced-motion" {
                reduced_motion = true;
                index += 1;
            } else if arguments[index] == "--sound" {
                sound = true;
                index += 1;
            } else if !arguments[index].starts_with('-') && config_argument.is_none() {
                config_argument = Some(PathBuf::from(&arguments[index]));
                index += 1;
            } else {
                index += 1;
            }
        }
        let config_path = config_argument
            .or_else(|| std::env::var_os("INFERNAUT_CONFIG").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("infernaut.toml"));
        return observe_tui::run(config_path, url, reduced_motion, sound).await;
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "config")
    {
        return cli::run_config(&arguments[1..]).await;
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "routing")
    {
        return cli::run_routing(&arguments[1..]).await;
    }

    let config_path = arguments
        .first()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("INFERNAUT_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("infernaut.toml"));
    let config = Arc::new(Config::load(&config_path)?);

    if let Some(environment_name) = &config.server.api_key_env
        && std::env::var(environment_name).map_or(true, |value| value.is_empty())
    {
        bail!(
            "server.api_key_env names '{}', but that environment variable is missing or empty",
            environment_name
        );
    }
    let mut configured_keys = BTreeSet::new();
    if let Some(environment_name) = &config.server.api_key_env
        && let Ok(value) = std::env::var(environment_name)
    {
        configured_keys.insert(value);
    }
    for client in config.clients.iter().filter(|client| !client.disabled) {
        let value = std::env::var(&client.api_key_env).with_context(|| {
            format!(
                "client '{}' api_key_env names '{}', but that environment variable is missing",
                client.id, client.api_key_env
            )
        })?;
        if value.is_empty() {
            bail!(
                "client '{}' api_key_env '{}' is empty",
                client.id,
                client.api_key_env
            );
        }
        if !configured_keys.insert(value) {
            bail!(
                "client '{}' resolves to an API key already used by the server or another client",
                client.id
            );
        }
    }

    let client = reqwest::Client::builder()
        // Provider redirects must never replay private prompts to another destination.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .user_agent(format!("infernaut/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build HTTP client")?;
    let auth = AuthManager::new()?;
    let plugins = PluginManager::start(&config.plugins)?;

    let initial_catalog = Catalog::discover(config.clone(), client.clone(), auth.clone()).await;
    for provider in &initial_catalog.providers {
        if provider.available {
            info!(
                provider = provider.id,
                models = provider.model_count,
                "provider discovered"
            );
        } else {
            warn!(
                provider = provider.id,
                error = provider.error,
                "provider unavailable"
            );
        }
    }

    let catalog = Arc::new(RwLock::new(initial_catalog));
    let resources = ResourceCoordinator::new();
    let probes = ProbeManager::new(
        config.clone(),
        catalog.clone(),
        client.clone(),
        resources.clone(),
    );
    let state = AppState {
        config: config.clone(),
        catalog,
        client,
        auth,
        metrics: Metrics::default(),
        model_load_lock: Arc::new(Mutex::new(())),
        resources,
        probes,
        plugins,
        routing: RoutingEngine::default(),
        telemetry: telemetry::TelemetryHub::default(),
    };
    spawn_catalog_refresh(state.clone());

    let listener = TcpListener::bind(config.server.listen)
        .await
        .with_context(|| format!("failed to bind {}", config.server.listen))?;
    info!(address = %config.server.listen, config = %config_path.display(), "Infernaut listening");

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut connections = tokio::task::JoinSet::new();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let (stream, peer) = tokio::select! {
            _ = &mut shutdown => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => continue,
            accepted = listener.accept() => accepted?,
        };
        let io = TokioIo::new(stream);
        let connection_state = state.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        connections.spawn(async move {
            let service = service_fn(move |request: Request<Incoming>| {
                app::handle_from_peer(request, connection_state.clone(), peer)
            });
            let builder = http1::Builder::new();
            let connection = builder.serve_connection(io, service);
            tokio::pin!(connection);
            let result = tokio::select! {
                result = &mut connection => result,
                _ = shutdown_rx.changed() => {
                    connection.as_mut().graceful_shutdown();
                    connection.await
                }
            };
            if let Err(error) = result {
                warn!(%peer, %error, "client connection failed");
            }
        });
    }
    drop(listener);
    let _ = shutdown_tx.send(true);
    let grace = std::env::var("INFERNAUT_SHUTDOWN_GRACE_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(900);
    info!(
        grace_seconds = grace,
        "shutdown requested; draining connections"
    );
    if time::timeout(Duration::from_secs(grace), async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        warn!("shutdown grace expired; closing remaining connections");
        connections.abort_all();
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn config_path_hint(arguments: &[String]) -> PathBuf {
    let explicit = match arguments.first().map(String::as_str) {
        Some("configure" | "observe") => arguments
            .iter()
            .skip(1)
            .find(|argument| !argument.starts_with('-'))
            .map(PathBuf::from),
        Some("config") => arguments
            .iter()
            .skip(2)
            .find(|argument| !argument.starts_with('-'))
            .map(PathBuf::from),
        Some("routing") => arguments
            .iter()
            .position(|argument| argument == "--config")
            .and_then(|index| arguments.get(index + 1))
            .map(PathBuf::from),
        Some("auth") => arguments.get(3).map(PathBuf::from),
        Some("probe" | "--help" | "-h" | "help") | None => None,
        Some(_) => arguments.first().map(PathBuf::from),
    };
    explicit
        .or_else(|| std::env::var_os("INFERNAUT_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("infernaut.toml"))
}

fn spawn_catalog_refresh(state: AppState) {
    let seconds = state.config.server.catalog_refresh_seconds.max(1);
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(seconds));
        interval.tick().await;
        loop {
            interval.tick().await;
            let refreshed = Catalog::discover(
                state.config.clone(),
                state.client.clone(),
                state.auth.clone(),
            )
            .await;
            let previous_ids = state
                .catalog
                .read()
                .await
                .models
                .iter()
                .map(|model| model.id.clone())
                .collect::<BTreeSet<_>>();
            let refreshed_ids = refreshed
                .models
                .iter()
                .map(|model| model.id.clone())
                .collect::<BTreeSet<_>>();
            let added = refreshed_ids
                .difference(&previous_ids)
                .cloned()
                .collect::<Vec<_>>();
            let removed = previous_ids
                .difference(&refreshed_ids)
                .cloned()
                .collect::<Vec<_>>();
            if !added.is_empty() || !removed.is_empty() {
                info!(
                    added_count = added.len(),
                    removed_count = removed.len(),
                    added = ?limited_ids(&added),
                    removed = ?limited_ids(&removed),
                    "provider model catalog changed"
                );
                let reconciliation = config_editor::reconcile_models(&state.config, &refreshed);
                let unclassified = reconciliation
                    .iter()
                    .filter(|model| {
                        model.state == config_editor::ModelProfileState::Unclassified
                            && added.contains(&model.model)
                    })
                    .map(|model| model.model.clone())
                    .collect::<Vec<_>>();
                if !unclassified.is_empty() {
                    warn!(
                        count = unclassified.len(),
                        models = ?limited_ids(&unclassified),
                        "new models are unclassified; run `infernaut configure` or `infernaut config doctor`"
                    );
                }
            }
            *state.catalog.write().await = refreshed;
        }
    });
}

fn limited_ids(ids: &[String]) -> Vec<&str> {
    ids.iter().map(String::as_str).take(20).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn dotenv_path_hint_tracks_each_config_accepting_command() {
        let cases = [
            (&["configs/server.toml"][..], "configs/server.toml"),
            (
                &["configure", "configs/tui.toml", "--offline"][..],
                "configs/tui.toml",
            ),
            (
                &["observe", "configs/observer.toml", "--sound"][..],
                "configs/observer.toml",
            ),
            (
                &["config", "doctor", "configs/doctor.toml", "--json"][..],
                "configs/doctor.toml",
            ),
            (
                &[
                    "routing",
                    "explain",
                    "route:private:auto",
                    "--config",
                    "configs/routing.toml",
                ][..],
                "configs/routing.toml",
            ),
            (
                &["auth", "status", "provider", "configs/auth.toml"][..],
                "configs/auth.toml",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                config_path_hint(&arguments(input)),
                PathBuf::from(expected),
                "arguments={input:?}"
            );
        }
    }
}
