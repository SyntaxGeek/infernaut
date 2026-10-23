use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use reqwest::{Client, Method};
use serde_json::{Value, json};

use crate::{
    auth::AuthManager,
    catalog::Catalog,
    config::Config,
    config_editor::{
        ModelProfileState, doctor, loaded_model_preference_name, privacy_mode_name,
        routing_mode_name,
    },
    routing::RoutingEngine,
};

pub fn print_help() {
    println!(
        "Infernaut — a unified local and cloud model gateway\n\n\
usage:\n  infernaut [CONFIG]\n  infernaut configure [CONFIG] [--offline]\n  infernaut observe [CONFIG] [--url URL]\n  infernaut config validate [CONFIG]\n  infernaut config doctor [CONFIG] [--json]\n  infernaut routing explain ROUTE --prompt TEXT [--config CONFIG]\n  infernaut auth COMMAND PROVIDER [CONFIG]\n  infernaut probe COMMAND"
    );
}

pub fn print_observe_help() {
    println!(
        "Infernaut Mission Control — read-only live inference topology\n\n\
usage:\n  infernaut observe [CONFIG] [--url URL] [--reduced-motion] [--sound]\n\n\
Connects to a running Infernaut event feed. The observer cannot modify configuration.\n\
The URL defaults to server.listen from CONFIG; --url is useful for a remote router.\n\
Use --reduced-motion to disable trajectory animation at startup.\n\
Use --sound to enable optional 8-bit lifecycle cues; press 8 to toggle them live."
    );
}

pub fn print_config_help() {
    println!(
        "Guided configuration\n\n\
usage:\n  infernaut configure [CONFIG] [--offline]\n  infernaut config validate [CONFIG]\n  infernaut config doctor [CONFIG] [--json]\n\n\
The configurator performs read-only provider discovery and no model inference.\n\
Credentials are loaded from .env beside CONFIG unless already exported.\n\
Use --offline to edit without provider connectivity, then press r to discover."
    );
}

pub async fn run_config(arguments: &[String]) -> Result<()> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return config_usage();
    };
    if matches!(command, "--help" | "-h" | "help") {
        print_config_help();
        return Ok(());
    }
    let config_path = arguments
        .iter()
        .skip(1)
        .find(|argument| !argument.starts_with('-'))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("INFERNAUT_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("infernaut.toml"));
    match command {
        "validate" => {
            let config = Config::load(&config_path)?;
            println!(
                "Valid configuration: {} ({} providers, {} routes, {} aliases, {} routing profiles, {} named clients, {} recipes; privacy={}, routing={}, loaded_preference={})",
                config_path.display(),
                config.providers.len(),
                config.routes.len(),
                config.aliases.len(),
                config.routing.models.len(),
                config
                    .clients
                    .iter()
                    .filter(|client| !client.disabled)
                    .count(),
                config
                    .recipes
                    .iter()
                    .filter(|recipe| !recipe.disabled)
                    .count(),
                privacy_mode_name(config.privacy_lock.mode),
                routing_mode_name(config.routing.mode),
                loaded_model_preference_name(config.routing.loaded_model_preference),
            );
        }
        "doctor" => {
            let (_config, _catalog, report) = doctor(&config_path).await?;
            if arguments.iter().any(|argument| argument == "--json") {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("Configuration doctor: {}", report.config);
                for provider in &report.providers {
                    if provider.available {
                        println!(
                            "  ✓ {:<24} {} model(s) [{}]",
                            provider.id, provider.model_count, provider.realm
                        );
                    } else {
                        println!(
                            "  ✗ {:<24} {}",
                            provider.id,
                            provider.error.as_deref().unwrap_or("unavailable")
                        );
                    }
                }
                let exact = report
                    .models
                    .iter()
                    .filter(|model| model.state == ModelProfileState::Exact)
                    .count();
                let wildcard = report
                    .models
                    .iter()
                    .filter(|model| model.state == ModelProfileState::Wildcard)
                    .count();
                let unclassified = report
                    .models
                    .iter()
                    .filter(|model| model.state == ModelProfileState::Unclassified)
                    .count();
                let dormant = report
                    .models
                    .iter()
                    .filter(|model| model.state == ModelProfileState::Dormant)
                    .count();
                println!(
                    "Models: {exact} exact, {wildcard} wildcard-managed, {unclassified} unclassified, {dormant} dormant"
                );
                for issue in &report.issues {
                    println!(
                        "  {} {}: {}",
                        issue.severity.to_uppercase(),
                        issue.code,
                        issue.message
                    );
                }
                if report.issues.is_empty() {
                    println!("No issues found.");
                }
            }
        }
        _ => return config_usage(),
    }
    Ok(())
}

pub async fn run_routing(arguments: &[String]) -> Result<()> {
    if arguments
        .first()
        .is_some_and(|command| matches!(command.as_str(), "--help" | "-h" | "help"))
    {
        println!(
            "usage: infernaut routing explain ROUTE --prompt TEXT [--config CONFIG]\n\
Runs classification and scoring against current discovery without provider inference."
        );
        return Ok(());
    }
    if arguments.first().map(String::as_str) != Some("explain") {
        return routing_usage();
    }
    let Some(requested_route) = arguments.get(1) else {
        return routing_usage();
    };
    let prompt = option_value(arguments, "--prompt")
        .context("routing explain requires --prompt TEXT")?
        .to_owned();
    let config_path = option_value(arguments, "--config")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("INFERNAUT_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("infernaut.toml"));
    let config = Config::load(&config_path)?;
    let route = config
        .requested_route(requested_route)
        .with_context(|| format!("route '{requested_route}' is not configured"))?
        .clone();
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .user_agent(format!(
            "infernaut-routing-explain/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .build()?;
    let catalog = Catalog::discover(Arc::new(config.clone()), client, AuthManager::new()?).await;
    let payload = json!({
        "model": route.qualified_id(),
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": prompt}]
    });
    let outcome = RoutingEngine::default()
        .evaluate(&config, &catalog, &route, &payload, false)
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    println!("{}", serde_json::to_string_pretty(&outcome.decision)?);
    Ok(())
}

fn option_value<'a>(arguments: &'a [String], name: &str) -> Option<&'a str> {
    arguments
        .iter()
        .position(|argument| argument == name)
        .and_then(|position| arguments.get(position + 1))
        .map(String::as_str)
}

pub async fn run_auth(arguments: &[String]) -> Result<()> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return auth_usage();
    };
    let Some(provider_id) = arguments.get(1) else {
        return auth_usage();
    };
    let config_path = arguments
        .get(2)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("INFERNAUT_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("infernaut.toml"));
    let config = Config::load(&config_path)?;
    let provider = config
        .providers
        .iter()
        .find(|provider| provider.id == *provider_id)
        .with_context(|| format!("provider '{provider_id}' is not configured"))?;
    let auth = AuthManager::new()?;
    let client = Client::builder()
        .user_agent(format!("infernaut/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    match command {
        "login" => {
            auth.login(provider, &client).await?;
            println!(
                "Authenticated provider '{}' and saved credentials to {}",
                provider.id,
                auth.store_path().display()
            );
        }
        "status" => println!("{}", serde_json::to_string_pretty(&auth.status(provider)?)?),
        "logout" => println!(
            "{}",
            if auth.logout(provider)? {
                "Credentials removed."
            } else {
                "Provider was not authenticated."
            }
        ),
        _ => return auth_usage(),
    }
    Ok(())
}

pub async fn run(arguments: &[String]) -> Result<()> {
    let base_url =
        std::env::var("INFERNAUT_URL").unwrap_or_else(|_| "http://127.0.0.1:4040".to_owned());
    let client = Client::new();
    let Some(command) = arguments.first().map(String::as_str) else {
        return usage();
    };

    let response = match command {
        "status" => {
            let Some(id) = arguments.get(1) else {
                return usage();
            };
            request(
                &client,
                Method::GET,
                &format!("{base_url}/infernaut/v1/probes/{id}"),
                None,
            )
            .await?
        }
        "cancel" => {
            let Some(id) = arguments.get(1) else {
                return usage();
            };
            request(
                &client,
                Method::DELETE,
                &format!("{base_url}/infernaut/v1/probes/{id}"),
                None,
            )
            .await?
        }
        model => {
            let dry_run = arguments.iter().any(|argument| argument == "--dry-run");
            let allow_cold_start = arguments
                .iter()
                .any(|argument| argument == "--allow-cold-start");
            let endpoint = if dry_run {
                "/infernaut/v1/probes/plan"
            } else {
                "/infernaut/v1/probes"
            };
            request(
                &client,
                Method::POST,
                &format!("{base_url}{endpoint}"),
                Some(json!({
                    "model": model,
                    "allow_cold_start": allow_cold_start,
                })),
            )
            .await?
        }
    };
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn request(client: &Client, method: Method, url: &str, body: Option<Value>) -> Result<Value> {
    let mut request = client.request(method, url);
    if let Ok(api_key) = std::env::var("INFERNAUT_API_KEY") {
        request = request.bearer_auth(api_key);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await?;
    let status = response.status();
    let payload: Value = response.json().await?;
    if !status.is_success() {
        bail!("Infernaut returned HTTP {status}: {payload}");
    }
    Ok(payload)
}

fn usage<T>() -> Result<T> {
    bail!(
        "usage: infernaut probe MODEL [--dry-run] [--allow-cold-start]\n       infernaut probe status JOB_ID\n       infernaut probe cancel JOB_ID"
    )
}

fn auth_usage<T>() -> Result<T> {
    bail!(
        "usage: infernaut auth login PROVIDER [CONFIG]\n       infernaut auth status PROVIDER [CONFIG]\n       infernaut auth logout PROVIDER [CONFIG]"
    )
}

fn config_usage<T>() -> Result<T> {
    bail!(
        "usage: infernaut configure [CONFIG] [--offline]\n       infernaut config validate [CONFIG]\n       infernaut config doctor [CONFIG] [--json]"
    )
}

fn routing_usage<T>() -> Result<T> {
    bail!(
        "usage: infernaut routing explain ROUTE --prompt TEXT [--config CONFIG]\nexample: infernaut routing explain route:private:auto --prompt \"Implement a new subsystem\""
    )
}
