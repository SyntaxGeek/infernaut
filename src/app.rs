use std::{
    convert::Infallible,
    error::Error,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures_util::{Stream, TryStreamExt};
use http_body_util::{BodyExt, Full, Limited, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Frame, Incoming},
    header::{
        AUTHORIZATION, CONNECTION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HOST,
        PROXY_AUTHORIZATION,
    },
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

use crate::{
    auth::AuthManager,
    catalog::{CODEX_CLIENT_VERSION, Catalog, ResolveError},
    codex_adapter,
    config::{
        AudioService, Config, PrivacyMode, ProviderConfig, ProviderKind, RecipeConfig, RecipeKind,
        RecipeProtocol, RecipeReasoningEffort, RecipeSlotConfig, RouteStrategy, RoutingMode,
    },
    diagnostics,
    metrics::Metrics,
    plugins::{EventContext, PluginManager},
    probes::{InferenceGuard, ProbeError, ProbeManager, ProbeRequest, ResourceCoordinator},
    routing::RoutingEngine,
    telemetry::{ClientIdentity, RequestTelemetry, TelemetryHub, TelemetryStage},
};

type BoxError = Box<dyn Error + Send + Sync>;
type ResponseBody = UnsyncBoxBody<Bytes, BoxError>;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub catalog: Arc<RwLock<Catalog>>,
    pub client: reqwest::Client,
    pub auth: AuthManager,
    pub metrics: Metrics,
    pub model_load_lock: Arc<Mutex<()>>,
    pub resources: ResourceCoordinator,
    pub probes: ProbeManager,
    pub plugins: PluginManager,
    pub routing: RoutingEngine,
    pub telemetry: TelemetryHub,
}

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub async fn handle_from_peer(
    request: Request<Incoming>,
    state: AppState,
    peer: SocketAddr,
) -> Result<Response<ResponseBody>, Infallible> {
    handle_inner(request, state, Some(peer)).await
}

async fn handle_inner(
    request: Request<Incoming>,
    state: AppState,
    peer: Option<SocketAddr>,
) -> Result<Response<ResponseBody>, Infallible> {
    let path = request.uri().path().to_owned();
    let authenticated_as = if is_public_unauthenticated_request(request.method(), &path) {
        AuthenticatedAs::Admin
    } else {
        let Some(authenticated) = authenticate_request(&request, &state.config) else {
            warn!(
                method = %request.method(),
                endpoint = path,
                "request rejected by Infernaut authentication"
            );
            return Ok(api_error_for_path(
                &path,
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "invalid_api_key",
                "A valid Infernaut API key is required.",
                json!({}),
            ));
        };
        if matches!(authenticated, AuthenticatedAs::Client(_)) && !is_named_client_endpoint(&path) {
            warn!(
                method = %request.method(),
                endpoint = path,
                "named inference client denied administrative endpoint"
            );
            return Ok(api_error_for_path(
                &path,
                StatusCode::FORBIDDEN,
                "authorization_error",
                "client_key_scope_denied",
                "Named client keys may access model and inference endpoints only.",
                json!({}),
            ));
        }
        authenticated
    };

    let method = request.method().clone();
    let response = if method == Method::GET && path.starts_with("/infernaut/v1/probes/") {
        let id = path.trim_start_matches("/infernaut/v1/probes/");
        probe_result(state.probes.get(id).await)
    } else if method == Method::DELETE && path.starts_with("/infernaut/v1/probes/") {
        let id = path.trim_start_matches("/infernaut/v1/probes/");
        probe_result(state.probes.cancel(id).await)
    } else if method == Method::GET
        && path.starts_with("/infernaut/v1/models/")
        && path.ends_with("/capabilities")
    {
        let model = path
            .trim_start_matches("/infernaut/v1/models/")
            .trim_end_matches("/capabilities");
        match state.probes.evidence_for_model(model).await {
            Ok(evidence) => json_response(
                StatusCode::OK,
                &json!({
                    "model": model,
                    "evidence": evidence,
                }),
            ),
            Err(error) => probe_error_response(error),
        }
    } else if method == Method::GET && path.starts_with("/v1/models/") {
        let requested = path.trim_start_matches("/v1/models/");
        let catalog = state.catalog.read().await;
        let exclusive_recipe = authenticated_as.exclusive_recipe(&state.config);
        let selectable_recipes = authenticated_as.selectable_recipes(&state.config);
        let model = exclusive_recipe
            .and_then(|recipe| catalog.recipe_model_object(&state.config, recipe, requested))
            .or_else(|| {
                if exclusive_recipe.is_some() {
                    None
                } else if selectable_recipes.is_empty() {
                    catalog.model_object(&state.config, requested)
                } else {
                    catalog
                        .catalog_with_selectable_recipes(&state.config, &selectable_recipes)
                        .data
                        .into_iter()
                        .find(|model| model.id == requested)
                }
            });
        match model {
            Some(model) => json_response(StatusCode::OK, &model),
            None => api_error(
                StatusCode::NOT_FOUND,
                "not_found_error",
                "model_not_found",
                "The requested model does not exist in the Infernaut catalog.",
                json!({ "model": requested }),
            ),
        }
    } else {
        match (method, path.as_str()) {
            (Method::HEAD, "/api/hello") => Response::builder()
                .status(StatusCode::OK)
                .body(full_body(Bytes::new()))
                .expect("valid Claude Code preflight response"),
            (Method::GET, "/") => json_response(
                StatusCode::OK,
                &json!({
                    "name": "Infernaut",
                    "version": env!("CARGO_PKG_VERSION"),
                    "models": "/v1/models",
                    "messages": "/v1/messages",
                    "audio_transcriptions": "/v1/audio/transcriptions",
                    "audio_speech": "/v1/audio/speech",
                    "audio_speech_stream": "/v1/audio/speech/stream",
                    "audio_voices": "/v1/audio/voices",
                    "health": "/health"
                }),
            ),
            (Method::GET, "/health") => health_response(&state).await,
            (Method::GET, "/v1/models") => {
                let catalog = state.catalog.read().await;
                if let Some(recipe) = authenticated_as.exclusive_recipe(&state.config) {
                    json_response(
                        StatusCode::OK,
                        &catalog.recipe_model_list(&state.config, recipe),
                    )
                } else {
                    let recipes = authenticated_as.selectable_recipes(&state.config);
                    json_response(
                        StatusCode::OK,
                        &catalog.catalog_with_selectable_recipes(&state.config, &recipes),
                    )
                }
            }
            (Method::GET, "/infernaut/v1/catalog") => {
                let catalog = state.catalog.read().await;
                json_response(StatusCode::OK, &*catalog)
            }
            (Method::GET, "/infernaut/v1/metrics") => {
                json_response(StatusCode::OK, &state.metrics.snapshot())
            }
            (Method::GET, "/infernaut/v1/events") => Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .header("x-accel-buffering", "no")
                .body(state.telemetry.sse_body())
                .expect("valid telemetry event response"),
            (Method::POST, "/infernaut/v1/probes/plan") => {
                match parse_json::<ProbeRequest>(request).await {
                    Ok(probe) => match state.probes.plan(&probe.model).await {
                        Ok(plan) => json_response(StatusCode::OK, &plan),
                        Err(error) => probe_error_response(error),
                    },
                    Err(response) => *response,
                }
            }
            (Method::POST, "/infernaut/v1/probes") => {
                match parse_json::<ProbeRequest>(request).await {
                    Ok(probe) => match state.probes.submit(probe).await {
                        Ok(job) => json_response(StatusCode::ACCEPTED, &job),
                        Err(error) => probe_error_response(error),
                    },
                    Err(response) => *response,
                }
            }
            (Method::POST, "/infernaut/v1/catalog/refresh") => {
                let refreshed = Catalog::discover(
                    state.config.clone(),
                    state.client.clone(),
                    state.auth.clone(),
                )
                .await;
                *state.catalog.write().await = refreshed.clone();
                json_response(StatusCode::OK, &refreshed)
            }
            (Method::POST, "/v1/chat/completions")
            | (Method::POST, "/v1/responses")
            | (Method::POST, "/v1/embeddings")
            | (Method::POST, "/v1/messages")
            | (Method::POST, "/v1/messages/count_tokens") => {
                proxy_inference_request(request, &path, state, peer, authenticated_as.client())
                    .await
            }
            (Method::POST, "/v1/audio/transcriptions") => {
                proxy_transcription_request(request, &path, state, peer, authenticated_as.client())
                    .await
            }
            (Method::POST, "/v1/audio/speech") | (Method::POST, "/v1/audio/speech/stream") => {
                proxy_speech_request(request, &path, state, peer, authenticated_as.client()).await
            }
            (Method::GET, "/v1/audio/voices") => {
                proxy_voice_catalog_request(request, &path, state, peer, authenticated_as.client())
                    .await
            }
            _ => api_error(
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "route_not_found",
                "The requested Infernaut endpoint does not exist.",
                json!({ "path": path }),
            ),
        }
    };

    Ok(response)
}

async fn health_response(state: &AppState) -> Response<ResponseBody> {
    let catalog = state.catalog.read().await;
    let available = catalog
        .providers
        .iter()
        .filter(|provider| provider.available)
        .count();
    let status = if available > 0 { "ok" } else { "degraded" };
    let provider_models = catalog
        .providers
        .iter()
        .map(|provider| (provider.id.clone(), provider.model_count))
        .collect::<std::collections::BTreeMap<_, _>>();
    let unavailable_providers = catalog
        .providers
        .iter()
        .filter(|provider| !provider.available)
        .map(|provider| provider.id.clone())
        .collect::<Vec<_>>();
    json_response(
        StatusCode::OK,
        &json!({
            "status": status,
            "available_providers": available,
            "configured_providers": catalog.providers.len(),
            "models": catalog.models.len(),
            "named_clients": state.config.clients.iter()
                .filter(|client| !client.disabled)
                .count(),
            "recipes": state.config.recipes.iter()
                .filter(|recipe| !recipe.disabled)
                .count(),
            "provider_models": provider_models,
            "unavailable_providers": unavailable_providers,
            "catalog_refreshed_at": catalog.refreshed_at,
            "plugins": state.plugins.status_map(),
            "routing": {
                "mode": state.config.routing.mode,
                "loaded_model_preference": state.config.routing.loaded_model_preference,
                "adaptive_routes": state.config.routes.iter()
                    .filter(|route| route.strategy == RouteStrategy::Adaptive)
                    .count(),
            },
        }),
    )
}

const MAX_TRANSCRIPTION_BODY_BYTES: usize = 100 * 1024 * 1024;

async fn proxy_transcription_request(
    request: Request<Incoming>,
    path: &str,
    state: AppState,
    peer: Option<SocketAddr>,
    authenticated_client: Option<AuthenticatedClient>,
) -> Response<ResponseBody> {
    let request_started = Instant::now();
    let incoming_headers = request.headers().clone();
    let client = client_identity(
        &incoming_headers,
        peer,
        authenticated_client
            .as_ref()
            .map(|client| client.label.clone()),
    );
    let request_privacy = match incoming_headers
        .get("x-infernaut-privacy")
        .and_then(|value| value.to_str().ok())
    {
        Some("local_only") => true,
        Some(value) => {
            return api_error_for_path(
                path,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_privacy_policy",
                "x-infernaut-privacy must be 'local_only' when supplied.",
                json!({ "value": value }),
            );
        }
        None => false,
    };
    if incoming_headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_TRANSCRIPTION_BODY_BYTES)
    {
        return api_error_for_path(
            path,
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "audio_request_too_large",
            "Transcription request bodies are limited to 100 MiB.",
            json!({ "maximum_bytes": MAX_TRANSCRIPTION_BODY_BYTES }),
        );
    }
    let content_type = match incoming_headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value)
            if value
                .to_ascii_lowercase()
                .starts_with("multipart/form-data") =>
        {
            value.to_owned()
        }
        _ => {
            return api_error_for_path(
                path,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "multipart_required",
                "The transcription endpoint requires multipart/form-data with 'file' and 'model' fields.",
                json!({}),
            );
        }
    };
    let body = match Limited::new(request.into_body(), MAX_TRANSCRIPTION_BODY_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return api_error_for_path(
                path,
                body_error_status(error.as_ref()),
                "invalid_request_error",
                "request_body_error",
                &format!("Unable to read transcription request body: {error}"),
                json!({}),
            );
        }
    };
    if body.len() > MAX_TRANSCRIPTION_BODY_BYTES {
        return api_error_for_path(
            path,
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "audio_request_too_large",
            "Transcription request bodies are limited to 100 MiB.",
            json!({ "maximum_bytes": MAX_TRANSCRIPTION_BODY_BYTES }),
        );
    }
    let boundary = match multipart_boundary(&content_type) {
        Ok(boundary) => boundary,
        Err(message) => {
            return api_error_for_path(
                path,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "multipart_boundary_invalid",
                message,
                json!({}),
            );
        }
    };
    let (requested_model, model_span) = match multipart_model_field(&body, &boundary) {
        Ok(value) => value,
        Err(message) => {
            return api_error_for_path(
                path,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "model_required",
                message,
                json!({}),
            );
        }
    };

    let request_id = next_request_id();
    let route = state
        .config
        .requested_route(&requested_model)
        .map(|route| route.qualified_id());
    let mut request_telemetry = RequestTelemetry {
        request_id: request_id.clone(),
        endpoint: path.to_owned(),
        client,
        requested_model: requested_model.clone(),
        route,
        provider: None,
        resolved_model: None,
        realm: None,
        task: Some("transcription".to_owned()),
        operation: Some("transcribe".to_owned()),
        effort: None,
        compression: false,
        streaming: false,
        request_bytes: body.len() as u64,
        estimated_tokens: None,
        activation_ms: None,
    };
    state
        .telemetry
        .emit(request_telemetry.event(TelemetryStage::Accepted));
    let mut pre_response_guard = state
        .telemetry
        .pre_response_guard(&request_id, request_started);
    let resolved = match state
        .catalog
        .read()
        .await
        .resolve_transcription(&state.config, &requested_model)
    {
        Ok(resolved) => resolved,
        Err(error) => {
            emit_telemetry_failure(
                &state,
                &request_telemetry,
                StatusCode::SERVICE_UNAVAILABLE,
                error.to_string(),
            );
            pre_response_guard.disarm();
            return resolve_error_response(path, error);
        }
    };
    let Some(provider) = state.config.provider(&resolved.provider).cloned() else {
        pre_response_guard.disarm();
        return api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "provider_not_found",
            "The resolved transcription provider is no longer configured.",
            json!({ "provider": resolved.provider }),
        );
    };
    if !provider.is_audio_provider() {
        pre_response_guard.disarm();
        return api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "unsupported_endpoint",
            "The resolved provider is not configured for OpenAI-compatible transcription.",
            json!({ "provider": provider.id }),
        );
    }
    let local_only = state.config.privacy_lock.mode == PrivacyMode::LocalOnly || request_privacy;
    if local_only && resolved.realm != "local" {
        emit_telemetry_failure(
            &state,
            &request_telemetry,
            StatusCode::FORBIDDEN,
            "privacy policy denied cloud transcription".to_owned(),
        );
        pre_response_guard.disarm();
        return api_error_for_path(
            path,
            StatusCode::FORBIDDEN,
            "routing_policy_violation",
            "privacy_lock_denied",
            "The transcription request requires a local provider.",
            json!({ "provider": provider.id, "realm": resolved.realm }),
        );
    }
    request_telemetry.provider = Some(resolved.provider.clone());
    request_telemetry.resolved_model = Some(resolved.qualified_id.clone());
    request_telemetry.realm = Some(resolved.realm.clone());
    state
        .telemetry
        .emit(request_telemetry.event(TelemetryStage::Classified));

    let rewritten_body = replace_bytes(&body, model_span, resolved.upstream_id.as_bytes());
    let upstream_url = format!(
        "{}/audio/transcriptions",
        provider.effective_base_url().trim_end_matches('/')
    );
    let mut upstream_request = state
        .client
        .post(upstream_url)
        .header(CONTENT_TYPE, &content_type)
        .body(rewritten_body);
    for (name, value) in &incoming_headers {
        if !is_hop_or_auth_header(name) {
            upstream_request = upstream_request.header(name, value);
        }
    }
    if let Some(api_key) = provider.api_key() {
        upstream_request = upstream_request.bearer_auth(api_key);
    }

    let _inference_guard = state.resources.begin_inference();
    let event_context = EventContext {
        request_id: request_id.clone(),
        endpoint: path.to_owned(),
        provider: resolved.provider.clone(),
        model: resolved.qualified_id.clone(),
        realm: resolved.realm.clone(),
    };
    // Emit metadata without ever copying the binary audio payload into the
    // plugin event stream. Response text remains observable under the normal
    // opt-in plugin policy.
    state.plugins.emit(
        infernaut::plugin_protocol::PluginEventKind::RequestContent,
        &event_context,
        Some("multipart/form-data"),
        None,
        None,
    );
    state
        .telemetry
        .emit(request_telemetry.event(TelemetryStage::Forwarding));
    let upstream_started = Instant::now();
    let upstream = match send_upstream(upstream_request).await {
        Ok(response) => response,
        Err(error) => {
            let elapsed = upstream_started.elapsed().as_millis() as u64;
            state.metrics.record(
                &requested_model,
                &resolved.qualified_id,
                &resolved.provider,
                StatusCode::BAD_GATEWAY.as_u16(),
                elapsed,
            );
            emit_telemetry_failure(
                &state,
                &request_telemetry,
                StatusCode::BAD_GATEWAY,
                error.to_string(),
            );
            state.plugins.emit(
                infernaut::plugin_protocol::PluginEventKind::ProviderError,
                &event_context,
                Some("text/plain"),
                Some(error.to_string()),
                None,
            );
            pre_response_guard.disarm();
            return api_error_for_path(
                path,
                StatusCode::BAD_GATEWAY,
                "provider_error",
                "upstream_unavailable",
                &format!(
                    "Transcription provider '{}' could not serve the request: {error}",
                    provider.id
                ),
                json!({ "provider": provider.id, "fallback_attempted": false }),
            );
        }
    };
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let time_to_headers = upstream_started.elapsed().as_millis() as u64;
    state.metrics.record(
        &requested_model,
        &resolved.qualified_id,
        &resolved.provider,
        status.as_u16(),
        time_to_headers,
    );
    let response_body = match bounded_upstream_body(upstream).await {
        Ok(body) => body,
        Err(error) => {
            emit_telemetry_failure(
                &state,
                &request_telemetry,
                StatusCode::BAD_GATEWAY,
                error.to_string(),
            );
            pre_response_guard.disarm();
            return api_error_for_path(
                path,
                StatusCode::BAD_GATEWAY,
                "provider_error",
                "invalid_upstream_response",
                &format!("Unable to read the transcription response: {error}"),
                json!({ "provider": provider.id }),
            );
        }
    };
    info!(
        endpoint = path,
        requested_model,
        resolved_model = resolved.qualified_id,
        provider = resolved.provider,
        realm = resolved.realm,
        audio_request_bytes = body.len(),
        status = status.as_u16(),
        time_to_headers_ms = time_to_headers,
        "transcription request proxied"
    );
    let mut event = request_telemetry.event(if status.is_success() {
        TelemetryStage::Completed
    } else {
        TelemetryStage::Failed
    });
    event.status = Some(status.as_u16());
    event.time_to_headers_ms = Some(time_to_headers);
    event.elapsed_ms = Some(request_started.elapsed().as_millis() as u64);
    event.response_bytes = Some(response_body.len() as u64);
    state.telemetry.emit(event);
    state.plugins.emit(
        infernaut::plugin_protocol::PluginEventKind::ResponseDelta,
        &event_context,
        upstream_headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        String::from_utf8(response_body.to_vec()).ok(),
        None,
    );
    state.plugins.emit(
        infernaut::plugin_protocol::PluginEventKind::ResponseCompleted,
        &event_context,
        None,
        None,
        None,
    );
    let mut builder = Response::builder()
        .status(status)
        .header("x-infernaut-requested-model", &requested_model)
        .header("x-infernaut-resolved-model", &resolved.qualified_id)
        .header("x-infernaut-provider", &resolved.provider)
        .header("x-infernaut-realm", &resolved.realm)
        .header("x-infernaut-request-id", &request_id)
        .header(
            "x-infernaut-privacy-policy",
            if local_only {
                "local_only"
            } else {
                "selective"
            },
        )
        .header(
            "x-infernaut-time-to-headers-ms",
            time_to_headers.to_string(),
        );
    for (name, value) in &upstream_headers {
        if !is_hop_or_length_header(name) {
            builder = builder.header(name, value);
        }
    }
    pre_response_guard.disarm();
    builder
        .body(full_body(response_body))
        .expect("valid transcription response")
}

const MAX_SPEECH_BODY_BYTES: usize = 16 * 1024 * 1024;

async fn proxy_speech_request(
    request: Request<Incoming>,
    path: &str,
    state: AppState,
    peer: Option<SocketAddr>,
    authenticated_client: Option<AuthenticatedClient>,
) -> Response<ResponseBody> {
    let request_started = Instant::now();
    let incoming_headers = request.headers().clone();
    let client = client_identity(
        &incoming_headers,
        peer,
        authenticated_client
            .as_ref()
            .map(|client| client.label.clone()),
    );
    let request_privacy = match incoming_headers
        .get("x-infernaut-privacy")
        .and_then(|value| value.to_str().ok())
    {
        Some("local_only") => true,
        Some(value) => {
            return api_error_for_path(
                path,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_privacy_policy",
                "x-infernaut-privacy must be 'local_only' when supplied.",
                json!({ "value": value }),
            );
        }
        None => false,
    };
    if incoming_headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_SPEECH_BODY_BYTES)
    {
        return api_error_for_path(
            path,
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "speech_request_too_large",
            "Speech synthesis request bodies are limited to 16 MiB.",
            json!({ "maximum_bytes": MAX_SPEECH_BODY_BYTES }),
        );
    }
    let body = match Limited::new(request.into_body(), MAX_SPEECH_BODY_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return api_error_for_path(
                path,
                body_error_status(error.as_ref()),
                "invalid_request_error",
                "request_body_error",
                &format!("Unable to read speech synthesis request body: {error}"),
                json!({}),
            );
        }
    };
    if body.len() > MAX_SPEECH_BODY_BYTES {
        return api_error_for_path(
            path,
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "speech_request_too_large",
            "Speech synthesis request bodies are limited to 16 MiB.",
            json!({ "maximum_bytes": MAX_SPEECH_BODY_BYTES }),
        );
    }
    let mut payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(error) => {
            return api_error_for_path(
                path,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_json",
                &format!("Speech synthesis request body is not valid JSON: {error}"),
                json!({}),
            );
        }
    };
    let Some(requested_model) = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "model_required",
            "A speech model or route ID is required.",
            json!({}),
        );
    };

    let request_id = next_request_id();
    let route = state
        .config
        .requested_route(&requested_model)
        .map(|route| route.qualified_id());
    let mut request_telemetry = RequestTelemetry {
        request_id: request_id.clone(),
        endpoint: path.to_owned(),
        client,
        requested_model: requested_model.clone(),
        route,
        provider: None,
        resolved_model: None,
        realm: None,
        task: Some("speech".to_owned()),
        operation: Some("synthesize".to_owned()),
        effort: None,
        compression: false,
        streaming: path.ends_with("/stream"),
        request_bytes: body.len() as u64,
        estimated_tokens: None,
        activation_ms: None,
    };
    state
        .telemetry
        .emit(request_telemetry.event(TelemetryStage::Accepted));
    let mut pre_response_guard = state
        .telemetry
        .pre_response_guard(&request_id, request_started);
    let resolved = match state.catalog.read().await.resolve_audio(
        &state.config,
        &requested_model,
        AudioService::Speech,
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            emit_telemetry_failure(
                &state,
                &request_telemetry,
                StatusCode::SERVICE_UNAVAILABLE,
                error.to_string(),
            );
            pre_response_guard.disarm();
            return resolve_error_response(path, error);
        }
    };
    let Some(provider) = state.config.provider(&resolved.provider).cloned() else {
        pre_response_guard.disarm();
        return api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "provider_not_found",
            "The resolved speech provider is no longer configured.",
            json!({ "provider": resolved.provider }),
        );
    };
    if !provider.is_audio_provider() {
        pre_response_guard.disarm();
        return api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "unsupported_endpoint",
            "The resolved provider is not configured for OpenAI-compatible speech synthesis.",
            json!({ "provider": provider.id }),
        );
    }
    let local_only = state.config.privacy_lock.mode == PrivacyMode::LocalOnly || request_privacy;
    if local_only && resolved.realm != "local" {
        emit_telemetry_failure(
            &state,
            &request_telemetry,
            StatusCode::FORBIDDEN,
            "privacy policy denied cloud speech synthesis".to_owned(),
        );
        pre_response_guard.disarm();
        return api_error_for_path(
            path,
            StatusCode::FORBIDDEN,
            "routing_policy_violation",
            "privacy_lock_denied",
            "The speech synthesis request requires a local provider.",
            json!({ "provider": provider.id, "realm": resolved.realm }),
        );
    }
    request_telemetry.provider = Some(resolved.provider.clone());
    request_telemetry.resolved_model = Some(resolved.qualified_id.clone());
    request_telemetry.realm = Some(resolved.realm.clone());
    state
        .telemetry
        .emit(request_telemetry.event(TelemetryStage::Classified));
    payload["model"] = Value::String(resolved.upstream_id.clone());

    let relative_path = path.trim_start_matches("/v1/");
    let upstream_url = format!(
        "{}/{}",
        provider.effective_base_url().trim_end_matches('/'),
        relative_path
    );
    let mut upstream_request = state.client.post(upstream_url).json(&payload);
    for (name, value) in &incoming_headers {
        if !is_hop_or_auth_header(name) {
            upstream_request = upstream_request.header(name, value);
        }
    }
    if let Some(api_key) = provider.api_key() {
        upstream_request = upstream_request.bearer_auth(api_key);
    }

    let inference_guard = state.resources.begin_inference();
    let event_context = EventContext {
        request_id: request_id.clone(),
        endpoint: path.to_owned(),
        provider: resolved.provider.clone(),
        model: resolved.qualified_id.clone(),
        realm: resolved.realm.clone(),
    };
    state.plugins.emit(
        infernaut::plugin_protocol::PluginEventKind::RequestContent,
        &event_context,
        Some("application/json"),
        serde_json::to_string(&payload).ok(),
        None,
    );
    state
        .telemetry
        .emit(request_telemetry.event(TelemetryStage::Forwarding));
    let upstream_started = Instant::now();
    let upstream = match send_upstream(upstream_request).await {
        Ok(response) => response,
        Err(error) => {
            let elapsed = upstream_started.elapsed().as_millis() as u64;
            state.metrics.record(
                &requested_model,
                &resolved.qualified_id,
                &resolved.provider,
                StatusCode::BAD_GATEWAY.as_u16(),
                elapsed,
            );
            emit_telemetry_failure(
                &state,
                &request_telemetry,
                StatusCode::BAD_GATEWAY,
                error.to_string(),
            );
            state.plugins.emit(
                infernaut::plugin_protocol::PluginEventKind::ProviderError,
                &event_context,
                Some("text/plain"),
                Some(error.to_string()),
                None,
            );
            pre_response_guard.disarm();
            return api_error_for_path(
                path,
                StatusCode::BAD_GATEWAY,
                "provider_error",
                "upstream_unavailable",
                &format!(
                    "Speech provider '{}' could not serve the request: {error}",
                    provider.id
                ),
                json!({ "provider": provider.id, "fallback_attempted": false }),
            );
        }
    };
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let time_to_headers = upstream_started.elapsed().as_millis() as u64;
    state.metrics.record(
        &requested_model,
        &resolved.qualified_id,
        &resolved.provider,
        status.as_u16(),
        time_to_headers,
    );
    info!(
        endpoint = path,
        requested_model,
        resolved_model = resolved.qualified_id,
        provider = resolved.provider,
        realm = resolved.realm,
        status = status.as_u16(),
        time_to_headers_ms = time_to_headers,
        "speech synthesis request proxied"
    );
    let mut response_event = request_telemetry.event(if status.is_success() {
        TelemetryStage::Headers
    } else {
        TelemetryStage::Failed
    });
    response_event.status = Some(status.as_u16());
    response_event.time_to_headers_ms = Some(time_to_headers);
    if !status.is_success() {
        response_event.detail = Some(format!("provider returned HTTP {}", status.as_u16()));
    }
    state.telemetry.emit(response_event);

    // Audio is forwarded byte-for-byte and never copied into the text plugin
    // pipeline. This preserves streaming latency and avoids binary payloads in
    // security/logging plugins.
    let observed = state.telemetry.observe_stream(
        upstream.bytes_stream(),
        request_telemetry,
        request_started,
        status.as_u16(),
        time_to_headers,
    );
    let stream = observed
        .map_ok(Frame::data)
        .map_err(|error| -> BoxError { Box::new(error) });
    let response_body = StreamBody::new(TrackedStream {
        inner: Box::pin(stream),
        _guard: inference_guard,
    })
    .boxed_unsync();
    pre_response_guard.disarm();
    let mut builder = Response::builder()
        .status(status)
        .header("x-infernaut-requested-model", &requested_model)
        .header("x-infernaut-resolved-model", &resolved.qualified_id)
        .header("x-infernaut-provider", &resolved.provider)
        .header("x-infernaut-realm", &resolved.realm)
        .header("x-infernaut-request-id", &request_id)
        .header(
            "x-infernaut-privacy-policy",
            if local_only {
                "local_only"
            } else {
                "selective"
            },
        )
        .header(
            "x-infernaut-time-to-headers-ms",
            time_to_headers.to_string(),
        );
    for (name, value) in &upstream_headers {
        if !is_hop_or_length_header(name) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(response_body)
        .expect("valid speech synthesis response")
}

async fn proxy_voice_catalog_request(
    request: Request<Incoming>,
    path: &str,
    state: AppState,
    peer: Option<SocketAddr>,
    authenticated_client: Option<AuthenticatedClient>,
) -> Response<ResponseBody> {
    let request_started = Instant::now();
    let incoming_headers = request.headers().clone();
    let client = client_identity(
        &incoming_headers,
        peer,
        authenticated_client
            .as_ref()
            .map(|client| client.label.clone()),
    );
    let request_privacy = match incoming_headers
        .get("x-infernaut-privacy")
        .and_then(|value| value.to_str().ok())
    {
        Some("local_only") => true,
        Some(value) => {
            return api_error_for_path(
                path,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_privacy_policy",
                "x-infernaut-privacy must be 'local_only' when supplied.",
                json!({ "value": value }),
            );
        }
        None => false,
    };
    let request_id = next_request_id();
    let resolved = match state
        .catalog
        .read()
        .await
        .resolve_default_audio(&state.config, AudioService::Speech)
    {
        Ok(resolved) => resolved,
        Err(error) => return resolve_error_response(path, error),
    };
    let requested_model = resolved.requested_id.clone();
    let route = state
        .config
        .requested_route(&requested_model)
        .map(|route| route.qualified_id());
    let request_telemetry = RequestTelemetry {
        request_id: request_id.clone(),
        endpoint: path.to_owned(),
        client,
        requested_model: requested_model.clone(),
        route,
        provider: Some(resolved.provider.clone()),
        resolved_model: Some(resolved.qualified_id.clone()),
        realm: Some(resolved.realm.clone()),
        task: Some("speech".to_owned()),
        operation: Some("list_voices".to_owned()),
        effort: None,
        compression: false,
        streaming: false,
        request_bytes: 0,
        estimated_tokens: None,
        activation_ms: None,
    };
    state
        .telemetry
        .emit(request_telemetry.event(TelemetryStage::Accepted));
    let mut pre_response_guard = state
        .telemetry
        .pre_response_guard(&request_id, request_started);
    let Some(provider) = state.config.provider(&resolved.provider).cloned() else {
        pre_response_guard.disarm();
        return api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "provider_not_found",
            "The resolved speech provider is no longer configured.",
            json!({ "provider": resolved.provider }),
        );
    };
    let local_only = state.config.privacy_lock.mode == PrivacyMode::LocalOnly || request_privacy;
    if local_only && resolved.realm != "local" {
        pre_response_guard.disarm();
        return api_error_for_path(
            path,
            StatusCode::FORBIDDEN,
            "routing_policy_violation",
            "privacy_lock_denied",
            "The voice catalog request requires a local provider.",
            json!({ "provider": provider.id, "realm": resolved.realm }),
        );
    }
    state
        .telemetry
        .emit(request_telemetry.event(TelemetryStage::Forwarding));
    let upstream_url = format!(
        "{}/audio/voices",
        provider.effective_base_url().trim_end_matches('/')
    );
    let mut upstream_request = state.client.get(upstream_url);
    for (name, value) in &incoming_headers {
        if !is_hop_or_auth_header(name) {
            upstream_request = upstream_request.header(name, value);
        }
    }
    if let Some(api_key) = provider.api_key() {
        upstream_request = upstream_request.bearer_auth(api_key);
    }
    let _inference_guard = state.resources.begin_inference();
    let upstream_started = Instant::now();
    let upstream = match send_upstream(upstream_request).await {
        Ok(response) => response,
        Err(error) => {
            emit_telemetry_failure(
                &state,
                &request_telemetry,
                StatusCode::BAD_GATEWAY,
                error.to_string(),
            );
            pre_response_guard.disarm();
            return api_error_for_path(
                path,
                StatusCode::BAD_GATEWAY,
                "provider_error",
                "upstream_unavailable",
                &format!(
                    "Speech provider '{}' could not list voices: {error}",
                    provider.id
                ),
                json!({ "provider": provider.id, "fallback_attempted": false }),
            );
        }
    };
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let time_to_headers = upstream_started.elapsed().as_millis() as u64;
    state.metrics.record(
        &requested_model,
        &resolved.qualified_id,
        &resolved.provider,
        status.as_u16(),
        time_to_headers,
    );
    let response_body = match bounded_upstream_body(upstream).await {
        Ok(body) => body,
        Err(error) => {
            emit_telemetry_failure(
                &state,
                &request_telemetry,
                StatusCode::BAD_GATEWAY,
                error.to_string(),
            );
            pre_response_guard.disarm();
            return api_error_for_path(
                path,
                StatusCode::BAD_GATEWAY,
                "provider_error",
                "invalid_upstream_response",
                &format!("Unable to read the voice catalog response: {error}"),
                json!({ "provider": provider.id }),
            );
        }
    };
    let mut event = request_telemetry.event(if status.is_success() {
        TelemetryStage::Completed
    } else {
        TelemetryStage::Failed
    });
    event.status = Some(status.as_u16());
    event.time_to_headers_ms = Some(time_to_headers);
    event.elapsed_ms = Some(request_started.elapsed().as_millis() as u64);
    event.response_bytes = Some(response_body.len() as u64);
    state.telemetry.emit(event);
    info!(
        endpoint = path,
        requested_model,
        resolved_model = resolved.qualified_id,
        provider = resolved.provider,
        realm = resolved.realm,
        status = status.as_u16(),
        time_to_headers_ms = time_to_headers,
        "voice catalog request proxied"
    );
    let mut builder = Response::builder()
        .status(status)
        .header("x-infernaut-requested-model", &requested_model)
        .header("x-infernaut-resolved-model", &resolved.qualified_id)
        .header("x-infernaut-provider", &resolved.provider)
        .header("x-infernaut-realm", &resolved.realm)
        .header("x-infernaut-request-id", &request_id)
        .header(
            "x-infernaut-privacy-policy",
            if local_only {
                "local_only"
            } else {
                "selective"
            },
        )
        .header(
            "x-infernaut-time-to-headers-ms",
            time_to_headers.to_string(),
        );
    for (name, value) in &upstream_headers {
        if !is_hop_or_length_header(name) {
            builder = builder.header(name, value);
        }
    }
    pre_response_guard.disarm();
    builder
        .body(full_body(response_body))
        .expect("valid voice catalog response")
}

fn multipart_boundary(content_type: &str) -> Result<String, &'static str> {
    content_type
        .split(';')
        .map(str::trim)
        .find_map(|parameter| {
            let (name, value) = parameter.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("boundary")
                .then_some(value.trim())
        })
        .map(|boundary| boundary.trim_matches('"').to_owned())
        .filter(|boundary| !boundary.is_empty())
        .ok_or("multipart/form-data is missing a valid boundary parameter")
}

fn multipart_model_field(
    body: &[u8],
    boundary: &str,
) -> Result<(String, std::ops::Range<usize>), &'static str> {
    let delimiter = format!("--{boundary}").into_bytes();
    let next_delimiter = format!("\r\n--{boundary}").into_bytes();
    let mut cursor = 0;
    while let Some(boundary_start) = find_bytes(body, &delimiter, cursor) {
        let headers_start = boundary_start + delimiter.len();
        if body.get(headers_start..headers_start + 2) == Some(b"--") {
            break;
        }
        let headers_start = if body.get(headers_start..headers_start + 2) == Some(b"\r\n") {
            headers_start + 2
        } else {
            return Err("multipart body contains a malformed part boundary");
        };
        let Some(headers_end) = find_bytes(body, b"\r\n\r\n", headers_start) else {
            return Err("multipart body contains malformed part headers");
        };
        let content_start = headers_end + 4;
        let Some(content_end) = find_bytes(body, &next_delimiter, content_start) else {
            return Err("multipart body contains an unterminated part");
        };
        let headers = &body[headers_start..headers_end];
        if find_bytes(headers, b"name=\"model\"", 0).is_some() {
            let model = std::str::from_utf8(&body[content_start..content_end])
                .map_err(|_| "multipart model field is not valid UTF-8")?
                .trim()
                .to_owned();
            if model.is_empty() {
                return Err("multipart model field is empty");
            }
            return Ok((model, content_start..content_end));
        }
        cursor = content_end;
    }
    Err(
        "A provider-qualified transcription model or route ID is required in the multipart model field.",
    )
}

fn find_bytes(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    if needle.is_empty() || start > haystack.len() {
        return None;
    }
    haystack[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| start + position)
}

fn replace_bytes(body: &[u8], range: std::ops::Range<usize>, replacement: &[u8]) -> Vec<u8> {
    let mut rewritten = Vec::with_capacity(body.len() - range.len() + replacement.len());
    rewritten.extend_from_slice(&body[..range.start]);
    rewritten.extend_from_slice(replacement);
    rewritten.extend_from_slice(&body[range.end..]);
    rewritten
}

async fn proxy_inference_request(
    request: Request<Incoming>,
    path: &str,
    state: AppState,
    peer: Option<SocketAddr>,
    authenticated_client: Option<AuthenticatedClient>,
) -> Response<ResponseBody> {
    let request_started = Instant::now();
    let method = request.method().clone();
    let query = request.uri().query().map(str::to_owned);
    let incoming_headers = request.headers().clone();
    let client = client_identity(
        &incoming_headers,
        peer,
        authenticated_client
            .as_ref()
            .map(|client| client.label.clone()),
    );
    let request_privacy = match incoming_headers
        .get("x-infernaut-privacy")
        .and_then(|value| value.to_str().ok())
    {
        Some("local_only") => true,
        Some(value) => {
            return api_error_for_path(
                path,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_privacy_policy",
                "x-infernaut-privacy must be 'local_only' when supplied.",
                json!({ "value": value }),
            );
        }
        None => false,
    };
    let body = match Limited::new(request.into_body(), 16 * 1024 * 1024)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return api_error_for_path(
                path,
                body_error_status(error.as_ref()),
                "invalid_request_error",
                "request_body_error",
                &format!("Unable to read request body: {error}"),
                json!({}),
            );
        }
    };

    if body.len() > 16 * 1024 * 1024 {
        return api_error_for_path(
            path,
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "request_too_large",
            "Request bodies are limited to 16 MiB in V1.",
            json!({}),
        );
    }

    let mut payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(error) => {
            return api_error_for_path(
                path,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_json",
                &format!("Request body is not valid JSON: {error}"),
                json!({}),
            );
        }
    };
    let requested_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Some(requested_model) = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "model_required",
            "A provider-qualified model or route ID is required.",
            json!({}),
        );
    };
    let recipe_request_model = state
        .config
        .requested_alias(&requested_model)
        .filter(|alias| alias.target.starts_with("recipe/"))
        .map(|alias| alias.target.as_str())
        .unwrap_or(&requested_model);
    let bound_recipe = authenticated_client
        .as_ref()
        .and_then(|client| client.recipe_for_request(&state.config, recipe_request_model))
        .cloned();
    if bound_recipe.is_none()
        && authenticated_client.is_some()
        && is_recipe_model_id(recipe_request_model)
    {
        return api_error_for_path(
            path,
            StatusCode::FORBIDDEN,
            "authorization_error",
            "recipe_scope_denied",
            "This client key is not authorized to select the requested recipe.",
            json!({
                "client": authenticated_client.as_ref().map(|client| &client.id),
                "requested_model": requested_model,
            }),
        );
    }
    let mut recipe_slot: Option<RecipeSlotConfig> = None;
    let mut recipe_compression_detected = false;
    let mut dispatch_model = requested_model.clone();
    if let Some(recipe) = &bound_recipe {
        let protocol_allowed = match recipe.protocol {
            RecipeProtocol::Any => true,
            RecipeProtocol::Anthropic => is_anthropic_path(path),
            RecipeProtocol::Openai => !is_anthropic_path(path),
        };
        if !protocol_allowed {
            return api_error_for_path(
                path,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "recipe_protocol_mismatch",
                "The client recipe is not configured for this inference protocol.",
                json!({
                    "client": authenticated_client.as_ref().map(|client| &client.id),
                    "recipe": recipe.id,
                    "protocol": recipe.protocol,
                    "endpoint": path,
                }),
            );
        }
        let Some(requested_slot) = recipe.slot_for_request(recipe_request_model) else {
            return api_error_for_path(
                path,
                StatusCode::FORBIDDEN,
                "authorization_error",
                "recipe_slot_scope_denied",
                "This client key may request only slots from its bound recipe.",
                json!({
                    "client": authenticated_client.as_ref().map(|client| &client.id),
                    "recipe": recipe.id,
                    "requested_model": requested_model,
                }),
            );
        };
        if requested_slot.trigger != crate::config::RecipeSlotTrigger::Model {
            return api_error_for_path(
                path,
                StatusCode::FORBIDDEN,
                "authorization_error",
                "recipe_slot_internal",
                "Classifier, delegation, and compression recipe slots are internal dispatch targets and cannot be selected directly.",
                json!({
                    "client": authenticated_client.as_ref().map(|client| &client.id),
                    "recipe": recipe.id,
                    "slot": requested_slot.id,
                    "requested_model": requested_model,
                }),
            );
        }
        if recipe_request_model.ends_with("[1m]") {
            let context_length = state
                .catalog
                .read()
                .await
                .target_context_length(&state.config, &requested_slot.target);
            if recipe.kind != crate::config::RecipeKind::ClaudeCode
                || !crate::catalog::supports_claude_code_1m(context_length)
            {
                return api_error_for_path(
                    path,
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "recipe_context_variant_unsupported",
                    "The requested recipe slot does not resolve to a verified 1M-context target.",
                    json!({
                        "recipe": recipe.id,
                        "slot": requested_slot.id,
                        "target": requested_slot.target,
                        "context_length": context_length,
                        "required_context_length": crate::catalog::CLAUDE_CODE_1M_CONTEXT_MIN_TOKENS,
                    }),
                );
            }
        }
        let (
            selected_slot,
            compression_detected,
            classifier_detected,
            delegation_detected,
            vision_detected,
        ) = effective_recipe_slot(recipe, requested_slot, path, &payload);
        recipe_compression_detected = compression_detected;
        dispatch_model = selected_slot.target.clone();
        recipe_slot = Some(selected_slot.clone());
        if let Some(limit) = selected_slot.max_output_tokens
            && let Some((requested, effective)) =
                crate::routing::cap_requested_output(&mut payload, limit)
            && effective < requested
        {
            info!(
                recipe = recipe.id,
                slot = selected_slot.id,
                requested_output_tokens = requested,
                effective_output_tokens = effective,
                "request output capped by recipe slot policy"
            );
        }
        if let Some((requested, effective)) =
            apply_recipe_reasoning_effort(&mut payload, selected_slot.reasoning_effort)
        {
            info!(
                recipe = recipe.id,
                slot = selected_slot.id,
                requested_reasoning_effort = ?requested,
                effective_reasoning_effort = effective,
                "request reasoning effort overridden by recipe slot policy"
            );
        }
        info!(
            client = authenticated_client
                .as_ref()
                .map(|client| client.id.as_str()),
            recipe = recipe.id,
            requested_slot = requested_slot.id,
            effective_slot = selected_slot.id,
            target = dispatch_model,
            compression_detected,
            classifier_detected,
            delegation_detected,
            vision_detected,
            "recipe request dispatched"
        );
    }
    let request_id = next_request_id();
    let route_id = bound_recipe
        .as_ref()
        .map(|recipe| format!("recipe/{}", recipe.id))
        .or_else(|| {
            state
                .config
                .requested_route(&dispatch_model)
                .map(|_| requested_model.clone())
        });
    let mut request_telemetry = RequestTelemetry {
        request_id: request_id.clone(),
        endpoint: path.to_owned(),
        client,
        requested_model: requested_model.clone(),
        route: route_id,
        provider: None,
        resolved_model: None,
        realm: None,
        task: None,
        operation: None,
        effort: None,
        compression: recipe_compression_detected,
        streaming: requested_stream,
        request_bytes: body.len() as u64,
        estimated_tokens: None,
        activation_ms: None,
    };
    state
        .telemetry
        .emit(request_telemetry.event(TelemetryStage::Accepted));
    let mut pre_response_guard = state
        .telemetry
        .pre_response_guard(&request_id, request_started);

    let messages = payload.get("messages").and_then(Value::as_array);
    let message_count = messages.map_or(0, |messages| messages.len());
    let message_bytes = messages.map_or(0, |messages| {
        messages.iter().map(serialized_json_len).sum::<usize>()
    });
    let system_messages = messages
        .into_iter()
        .flatten()
        .filter(|message| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        })
        .collect::<Vec<_>>();
    let message_system_bytes = system_messages
        .iter()
        .filter_map(|message| message.get("content"))
        .map(serialized_json_len)
        .sum::<usize>();
    let top_level_system_bytes = payload
        .get("system")
        .map(serialized_json_len)
        .unwrap_or_default();
    let system_message_count = system_messages.len() + usize::from(payload.get("system").is_some());
    let system_bytes = message_system_bytes + top_level_system_bytes;
    let tools = payload.get("tools").and_then(Value::as_array);
    let tool_count = tools.map_or(0, |tools| tools.len());
    let tool_bytes = tools.map_or(0, |tools| serialized_json_len(&Value::Array(tools.clone())));
    let content_array_messages = messages.map_or(0, |messages| {
        messages
            .iter()
            .filter(|message| message.get("content").is_some_and(Value::is_array))
            .count()
    });
    let image_parts = messages.map_or(0, |messages| {
        messages
            .iter()
            .filter_map(|message| message.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("image_url" | "input_image" | "image")
                )
            })
            .count()
    });
    let max_output_tokens = payload
        .get("max_completion_tokens")
        .or_else(|| payload.get("max_tokens"))
        .and_then(Value::as_u64);
    info!(
        endpoint = path,
        requested_model,
        body_bytes = body.len(),
        message_count,
        message_bytes,
        system_message_count,
        system_bytes,
        tool_count,
        tool_bytes,
        content_array_messages,
        image_parts,
        stream = requested_stream,
        max_output_tokens = ?max_output_tokens,
        temperature = ?payload.get("temperature").and_then(|value| value.as_f64()),
        requested_reasoning_effort = ?payload.get("reasoning_effort").and_then(|value| value.as_str()),
        has_tool_choice = payload.get("tool_choice").is_some(),
        tool_requirement = ?crate::routing::tool_requirement(&payload),
        has_response_format = payload.get("response_format").is_some(),
        "inference request shape"
    );

    let local_only = state.config.privacy_lock.mode == PrivacyMode::LocalOnly || request_privacy;
    let requested_alias = state.config.requested_alias(&requested_model);
    if let Some(alias) = requested_alias
        && let Some(context_length) = alias.context_length
    {
        let estimate_ratio = alias
            .target
            .strip_prefix("route/")
            .and_then(|route_id| state.config.route(route_id))
            .and_then(|route| route.token_estimate_bytes_per_token);
        let required_tokens =
            crate::routing::estimated_required_tokens_with_ratio(&payload, estimate_ratio);
        if required_tokens > context_length {
            request_telemetry.estimated_tokens = Some(required_tokens);
            emit_telemetry_failure(
                &state,
                &request_telemetry,
                StatusCode::BAD_REQUEST,
                format!(
                    "alias context exceeded: required~{required_tokens}, contract={context_length}"
                ),
            );
            return resolve_error_response(
                path,
                ResolveError::AliasContextExceeded {
                    alias: alias.id.clone(),
                    required_tokens,
                    context_length,
                },
            );
        }
    }
    let mut effective_route = state.config.requested_route(&dispatch_model).cloned();
    if let (Some(alias), Some(route)) = (requested_alias, effective_route.as_mut())
        && let Some(alias_contract) = alias.context_length
    {
        route.context_length = Some(
            route
                .context_length
                .map_or(alias_contract, |route_contract| {
                    route_contract.max(alias_contract)
                }),
        );
    }
    let mut recipe_route_realm_conflict = false;
    if let (Some(recipe), Some(route)) = (&bound_recipe, effective_route.as_mut())
        && !recipe.allowed_realms.is_empty()
    {
        if route.allowed_realms.is_empty() {
            route.allowed_realms = recipe.allowed_realms.clone();
        } else {
            route
                .allowed_realms
                .retain(|realm| recipe.allowed_realms.contains(realm));
            recipe_route_realm_conflict = route.allowed_realms.is_empty();
        }
    }
    if recipe_route_realm_conflict {
        let recipe = bound_recipe.as_ref().expect("conflict requires a recipe");
        return api_error_for_path(
            path,
            StatusCode::FORBIDDEN,
            "routing_policy_violation",
            "recipe_route_realm_conflict",
            "The recipe and selected route have no provider realm in common.",
            json!({
                "recipe": recipe.id,
                "target": dispatch_model,
                "recipe_allowed_realms": recipe.allowed_realms,
            }),
        );
    }
    let requested_route = effective_route.as_ref();
    if let Some(route) = requested_route
        && (route.strategy != RouteStrategy::Adaptive
            || state.config.routing.mode == RoutingMode::Off)
        && let Some(context_length) = route.context_length
    {
        let required_tokens = crate::routing::estimated_required_tokens_with_ratio(
            &payload,
            route.token_estimate_bytes_per_token,
        );
        if required_tokens > context_length {
            request_telemetry.estimated_tokens = Some(required_tokens);
            emit_telemetry_failure(
                &state,
                &request_telemetry,
                StatusCode::BAD_REQUEST,
                format!(
                    "route context exceeded: required~{required_tokens}, contract={context_length}"
                ),
            );
            return resolve_error_response(
                path,
                ResolveError::RouteContextExceeded {
                    route: route.id.clone(),
                    required_tokens,
                    context_length,
                },
            );
        }
    }
    if requested_route.is_none()
        && let (Some(recipe), Some(slot)) = (&bound_recipe, &recipe_slot)
    {
        let context_length = state
            .catalog
            .read()
            .await
            .target_context_length(&state.config, &slot.target);
        if let Some(context_length) = context_length {
            let required_tokens =
                crate::routing::estimated_required_tokens_with_ratio(&payload, None);
            if required_tokens > context_length {
                request_telemetry.estimated_tokens = Some(required_tokens);
                emit_telemetry_failure(
                    &state,
                    &request_telemetry,
                    StatusCode::BAD_REQUEST,
                    format!(
                        "recipe slot context exceeded: required~{required_tokens}, limit={context_length}"
                    ),
                );
                return api_error_for_path(
                    path,
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "recipe_slot_context_length_exceeded",
                    "Prompt is too long for the model assigned to this recipe slot.",
                    json!({
                        "recipe": recipe.id,
                        "slot": slot.id,
                        "target": slot.target,
                        "estimated_required_tokens": required_tokens,
                        "context_length": context_length,
                        "fallback_attempted": false,
                    }),
                );
            }
        }
    }
    let adaptive_route = requested_route.filter(|route| route.strategy == RouteStrategy::Adaptive);
    let client_scope = authenticated_client
        .as_ref()
        .map(|client| format!("client:{}", client.id))
        .unwrap_or_else(|| format!("peer:{:?}", peer.map(|peer| peer.ip())));
    let routing = state.routing.scoped(
        &client_scope,
        incoming_headers
            .get("x-infernaut-session-id")
            .and_then(|value| value.to_str().ok()),
    );
    let resolved = if let Some(route) = adaptive_route
        && state.config.routing.mode != RoutingMode::Off
    {
        let catalog = state.catalog.read().await.clone();
        let outcome_result = if path == "/v1/messages" {
            routing
                .evaluate_for_anthropic_messages(
                    &state.config,
                    &catalog,
                    route,
                    &payload,
                    local_only,
                )
                .await
        } else if recipe_compression_detected {
            routing
                .evaluate_with_compression_hint(
                    &state.config,
                    &catalog,
                    route,
                    &payload,
                    local_only,
                    true,
                )
                .await
        } else {
            routing
                .evaluate(&state.config, &catalog, route, &payload, local_only)
                .await
        };
        let outcome = match outcome_result {
            Ok(outcome) => outcome,
            Err(error) => {
                let mut event = request_telemetry.event(TelemetryStage::Failed);
                event.detail = Some(error.to_string());
                state.telemetry.emit(event);
                return resolve_error_response(path, error);
            }
        };
        request_telemetry.task = Some(outcome.decision.classification.task.as_str().to_owned());
        request_telemetry.operation = Some(
            outcome
                .decision
                .classification
                .operation
                .as_str()
                .to_owned(),
        );
        request_telemetry.effort = Some(outcome.decision.classification.effort.as_str().to_owned());
        request_telemetry.compression = outcome.decision.classification.compression_request;
        request_telemetry.estimated_tokens =
            Some(outcome.decision.classification.estimated_required_tokens);
        if outcome.decision.classification.compression_request
            && let Some(limit) = route.compression_max_output_tokens
            && let Some((requested, effective)) =
                crate::routing::cap_requested_output(&mut payload, limit)
            && effective < requested
        {
            info!(
                route = route.id,
                requested_output_tokens = requested,
                effective_output_tokens = effective,
                "compression request output capped by route policy"
            );
        }
        let recommended_model = outcome.selected.id.clone();
        let resolved = if state.config.routing.mode == RoutingMode::Active {
            crate::catalog::ResolvedModel::from_discovered(&requested_model, &outcome.selected)
        } else {
            match catalog.resolve(&state.config, &dispatch_model) {
                Ok(resolved) => resolved,
                Err(error) => {
                    emit_telemetry_failure(
                        &state,
                        &request_telemetry,
                        StatusCode::BAD_REQUEST,
                        error.to_string(),
                    );
                    return resolve_error_response(path, error);
                }
            }
        };
        let alternatives = outcome
            .decision
            .candidates
            .iter()
            .take(4)
            .map(|candidate| format!("{}={:.2}", candidate.model, candidate.score))
            .collect::<Vec<_>>();
        let rejected = outcome
            .decision
            .rejected
            .iter()
            .take(4)
            .map(|candidate| format!("{}={}", candidate.model, candidate.reasons.join("+")))
            .collect::<Vec<_>>();
        info!(
            routing_mode = ?state.config.routing.mode,
            route = outcome.decision.route,
            task = outcome.decision.classification.task.as_str(),
            operation = outcome.decision.classification.operation.as_str(),
            effort = outcome.decision.classification.effort.as_str(),
            confidence = outcome.decision.classification.confidence,
            vision_input = outcome.decision.classification.vision_input,
            compression_request = outcome.decision.classification.compression_request,
            intent_bytes = outcome.decision.classification.intent_bytes,
            scaffold_bytes = outcome.decision.classification.scaffold_bytes,
            estimated_input_tokens =
                outcome.decision.classification.estimated_input_tokens,
            requested_output_tokens =
                outcome.decision.classification.requested_output_tokens,
            reserved_output_tokens =
                outcome.decision.classification.reserved_output_tokens,
            estimated_required_tokens =
                outcome.decision.classification.estimated_required_tokens,
            signals = ?outcome.decision.classification.signals,
            recommended_model,
            actual_model = resolved.qualified_id,
            selection_reasons = ?outcome.decision.selection_reasons,
            alternatives = ?alternatives,
            rejected = ?rejected,
            tool_loop_pinned = outcome.decision.tool_loop_pinned,
            "adaptive route decision"
        );
        request_telemetry.provider = Some(resolved.provider.clone());
        request_telemetry.resolved_model = Some(resolved.qualified_id.clone());
        request_telemetry.realm = Some(resolved.realm.clone());
        state
            .telemetry
            .emit(request_telemetry.event(TelemetryStage::Classified));
        resolved
    } else {
        let catalog = state.catalog.read().await;
        match catalog.resolve(&state.config, &dispatch_model) {
            Ok(resolved) => resolved,
            Err(error) => {
                emit_telemetry_failure(
                    &state,
                    &request_telemetry,
                    StatusCode::BAD_REQUEST,
                    error.to_string(),
                );
                return resolve_error_response(path, error);
            }
        }
    };
    request_telemetry.provider = Some(resolved.provider.clone());
    request_telemetry.resolved_model = Some(resolved.qualified_id.clone());
    request_telemetry.realm = Some(resolved.realm.clone());
    if let Some(recipe) = &bound_recipe
        && !recipe.allowed_realms.is_empty()
        && !recipe.allowed_realms.contains(&resolved.realm)
    {
        emit_telemetry_failure(
            &state,
            &request_telemetry,
            StatusCode::FORBIDDEN,
            "recipe denied the resolved provider realm".to_owned(),
        );
        return api_error_for_path(
            path,
            StatusCode::FORBIDDEN,
            "routing_policy_violation",
            "recipe_realm_denied",
            "The client recipe forbids the resolved provider realm.",
            json!({
                "recipe": recipe.id,
                "slot": recipe_slot.as_ref().map(|slot| &slot.id),
                "target": dispatch_model,
                "resolved_model": resolved.qualified_id,
                "resolved_realm": resolved.realm,
                "allowed_realms": recipe.allowed_realms,
            }),
        );
    }
    if local_only && resolved.realm != "local" {
        emit_telemetry_failure(
            &state,
            &request_telemetry,
            StatusCode::FORBIDDEN,
            "privacy lock denied the resolved non-local provider".to_owned(),
        );
        return api_error_for_path(
            path,
            StatusCode::FORBIDDEN,
            "routing_policy_violation",
            "privacy_lock_denied",
            "The effective privacy policy forbids routing this request to a non-local model.",
            json!({
                "requested_model": requested_model,
                "resolved_model": resolved.qualified_id,
                "resolved_realm": resolved.realm,
                "privacy_policy": "local_only",
                "fallback_attempted": false,
            }),
        );
    }
    let Some(provider) = state.config.provider(&resolved.provider).cloned() else {
        emit_telemetry_failure(
            &state,
            &request_telemetry,
            StatusCode::BAD_GATEWAY,
            "resolved provider is no longer configured".to_owned(),
        );
        return api_error_for_path(
            path,
            StatusCode::BAD_GATEWAY,
            "routing_error",
            "provider_unavailable",
            "The resolved provider is no longer configured.",
            json!({ "provider": resolved.provider }),
        );
    };
    if is_anthropic_path(path) && !provider.supports_anthropic_messages() {
        emit_telemetry_failure(
            &state,
            &request_telemetry,
            StatusCode::BAD_REQUEST,
            "provider does not support native Anthropic Messages".to_owned(),
        );
        return api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "unsupported_endpoint",
            "The resolved provider is not configured for native Anthropic Messages compatibility.",
            json!({
                "provider": provider.id,
                "requested_model": requested_model,
                "resolved_model": resolved.qualified_id,
                "configuration": "Use a native kind (lm_studio, llama_cpp, or vllm), or set anthropic_compat = true only when the provider natively serves POST /v1/messages.",
            }),
        );
    }
    if path == "/v1/messages/count_tokens" && !provider.supports_native_anthropic_token_counting() {
        let input_tokens = estimate_anthropic_input_tokens(&payload);
        let event_context = EventContext {
            request_id: request_id.clone(),
            endpoint: path.to_owned(),
            provider: resolved.provider.clone(),
            model: resolved.qualified_id.clone(),
            realm: resolved.realm.clone(),
        };
        state.plugins.emit(
            infernaut::plugin_protocol::PluginEventKind::RequestContent,
            &event_context,
            Some("application/json"),
            serde_json::to_string(&payload).ok(),
            None,
        );
        let response = json!({ "input_tokens": input_tokens });
        state.plugins.emit(
            infernaut::plugin_protocol::PluginEventKind::ResponseCompleted,
            &event_context,
            Some("application/json"),
            Some(response.to_string()),
            None,
        );
        let mut event = request_telemetry.event(TelemetryStage::Completed);
        event.status = Some(StatusCode::OK.as_u16());
        event.response_bytes = Some(response.to_string().len() as u64);
        event.detail = Some("token count estimated locally".to_owned());
        state.telemetry.emit(event);
        info!(
            endpoint = path,
            requested_model,
            resolved_model = resolved.qualified_id,
            provider = resolved.provider,
            realm = resolved.realm,
            input_tokens,
            method = "conservative_estimate",
            "Anthropic input token count completed locally"
        );
        return json_response_with_routing_headers(
            StatusCode::OK,
            &response,
            &request_id,
            &requested_model,
            &resolved.qualified_id,
            &resolved.provider,
            &resolved.realm,
            local_only,
            0,
        );
    }
    let inference_guard = state.resources.begin_inference();

    let delegated_activation = resolved.loaded == Some(false) && provider.jit_load;
    let activation_started =
        (resolved.loaded == Some(false) && provider.auto_load).then(Instant::now);
    if activation_started.is_some() {
        state
            .telemetry
            .emit(request_telemetry.event(TelemetryStage::Loading));
    }
    let upstream_model = match prepare_upstream_model(&state, &provider, &resolved, path).await {
        Ok(model) => model,
        Err(response) => {
            let mut event = request_telemetry.event(TelemetryStage::Failed);
            event.detail = Some("model activation failed".to_owned());
            state.telemetry.emit(event);
            return *response;
        }
    };
    request_telemetry.activation_ms =
        activation_started.map(|started| started.elapsed().as_millis() as u64);
    let codex_chat_adapter =
        provider.kind == ProviderKind::OpenaiCodex && path == "/v1/chat/completions";
    if provider.kind == ProviderKind::OpenaiCodex && path == "/v1/embeddings" {
        emit_telemetry_failure(
            &state,
            &request_telemetry,
            StatusCode::BAD_REQUEST,
            "ChatGPT Codex transport does not provide embeddings".to_owned(),
        );
        return api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "unsupported_endpoint",
            "ChatGPT Codex subscription transport does not provide embeddings.",
            json!({ "provider": provider.id }),
        );
    }
    if codex_chat_adapter {
        payload = match codex_adapter::chat_to_responses(&payload, &upstream_model) {
            Ok(value) => value,
            Err(error) => {
                emit_telemetry_failure(
                    &state,
                    &request_telemetry,
                    StatusCode::BAD_REQUEST,
                    error.clone(),
                );
                return api_error_for_path(
                    path,
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "codex_request_conversion_failed",
                    &error,
                    json!({}),
                );
            }
        };
    } else {
        payload["model"] = Value::String(upstream_model);
    }
    let relative_path = if codex_chat_adapter {
        "responses"
    } else {
        path.strip_prefix("/v1/")
            .unwrap_or(path.trim_start_matches('/'))
    };
    let mut upstream_url = format!(
        "{}/{}",
        provider.effective_base_url().trim_end_matches('/'),
        relative_path
    );
    if let Some(query) = query {
        upstream_url.push('?');
        upstream_url.push_str(&query);
    }

    let mut upstream_request = state.client.request(method, upstream_url).json(&payload);
    for (name, value) in &incoming_headers {
        if !is_hop_or_auth_header(name) {
            upstream_request = upstream_request.header(name, value);
        }
    }
    if provider.kind.uses_oauth() {
        match state.auth.credential(&provider, &state.client).await {
            Ok(credential) => {
                upstream_request = upstream_request.bearer_auth(&credential.access_token);
                if provider.kind == ProviderKind::OpenaiCodex {
                    upstream_request = upstream_request
                        .header("originator", "codex_cli_rs")
                        .header(
                            "user-agent",
                            format!(
                                "codex_cli_rs/{CODEX_CLIENT_VERSION} (Infernaut/{})",
                                env!("CARGO_PKG_VERSION")
                            ),
                        );
                    if let Some(account_id) = credential.account_id {
                        upstream_request =
                            upstream_request.header("ChatGPT-Account-Id", account_id);
                    }
                }
            }
            Err(error) => {
                emit_telemetry_failure(
                    &state,
                    &request_telemetry,
                    StatusCode::UNAUTHORIZED,
                    error.to_string(),
                );
                return api_error_for_path(
                    path,
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    "oauth_credentials_unavailable",
                    &error.to_string(),
                    json!({ "provider": provider.id }),
                );
            }
        }
    } else if let Some(api_key) = provider.api_key() {
        upstream_request = upstream_request.bearer_auth(api_key);
    }

    let upstream_started = Instant::now();
    let mut forwarding_event = request_telemetry.event(TelemetryStage::Forwarding);
    if delegated_activation {
        forwarding_event.detail =
            Some("provider-managed JIT activation may occur before headers".to_owned());
    }
    state.telemetry.emit(forwarding_event);
    let event_context = EventContext {
        request_id: request_id.clone(),
        endpoint: path.to_owned(),
        provider: resolved.provider.clone(),
        model: resolved.qualified_id.clone(),
        realm: resolved.realm.clone(),
    };
    state.plugins.emit(
        infernaut::plugin_protocol::PluginEventKind::RequestContent,
        &event_context,
        Some("application/json"),
        serde_json::to_string(&payload).ok(),
        None,
    );
    let upstream = match send_upstream(upstream_request).await {
        Ok(response) => response,
        Err(error) => {
            warn!(
                endpoint = path,
                requested_model = requested_model,
                resolved_model = resolved.qualified_id,
                provider = resolved.provider,
                realm = resolved.realm,
                elapsed_ms = upstream_started.elapsed().as_millis() as u64,
                error = %error,
                "inference request failed before response"
            );
            state.metrics.record(
                &requested_model,
                &resolved.qualified_id,
                &resolved.provider,
                502,
                upstream_started.elapsed().as_millis() as u64,
            );
            state.plugins.emit(
                infernaut::plugin_protocol::PluginEventKind::ProviderError,
                &event_context,
                Some("text/plain"),
                Some(error.to_string()),
                None,
            );
            let mut event = request_telemetry.event(TelemetryStage::Failed);
            event.elapsed_ms = Some(request_started.elapsed().as_millis() as u64);
            event.status = Some(StatusCode::BAD_GATEWAY.as_u16());
            event.detail = Some(error.to_string());
            state.telemetry.emit(event);
            return api_error_for_path(
                path,
                StatusCode::BAD_GATEWAY,
                "provider_error",
                "upstream_unavailable",
                &format!(
                    "Provider '{}' could not serve the request: {error}",
                    provider.id
                ),
                json!({
                    "provider": provider.id,
                    "requested_model": requested_model,
                    "resolved_model": resolved.qualified_id,
                    "fallback_attempted": false,
                }),
            );
        }
    };

    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let time_to_headers = upstream_started.elapsed().as_millis() as u64;
    state.metrics.record(
        &requested_model,
        &resolved.qualified_id,
        &resolved.provider,
        status.as_u16(),
        time_to_headers,
    );
    info!(
        endpoint = path,
        requested_model = requested_model,
        resolved_model = resolved.qualified_id,
        provider = resolved.provider,
        realm = resolved.realm,
        status = status.as_u16(),
        time_to_headers_ms = time_to_headers,
        "inference request proxied"
    );
    let mut response_event = request_telemetry.event(if status.is_success() {
        TelemetryStage::Headers
    } else {
        TelemetryStage::Failed
    });
    response_event.status = Some(status.as_u16());
    response_event.time_to_headers_ms = Some(time_to_headers);
    if !status.is_success() {
        response_event.detail = Some(format!("provider returned HTTP {}", status.as_u16()));
    }
    state.telemetry.emit(response_event);

    if codex_chat_adapter && status.is_success() {
        if requested_stream {
            let observed = state.telemetry.observe_stream(
                upstream.bytes_stream(),
                request_telemetry.clone(),
                request_started,
                status.as_u16(),
                time_to_headers,
            );
            let stream = codex_adapter::response_sse_to_chat(
                state.plugins.observe_stream(
                    observed,
                    event_context.clone(),
                    "text/event-stream".into(),
                ),
                requested_model.clone(),
            );
            let response_body = StreamBody::new(TrackedStream {
                inner: stream,
                _guard: inference_guard,
            })
            .boxed_unsync();
            pre_response_guard.disarm();
            return Response::builder()
                .status(status)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .header("x-infernaut-requested-model", &requested_model)
                .header("x-infernaut-resolved-model", &resolved.qualified_id)
                .header("x-infernaut-provider", &resolved.provider)
                .header("x-infernaut-realm", &resolved.realm)
                .header("x-infernaut-request-id", &request_id)
                .header(
                    "x-infernaut-privacy-policy",
                    if local_only {
                        "local_only"
                    } else {
                        "selective"
                    },
                )
                .header(
                    "x-infernaut-time-to-headers-ms",
                    time_to_headers.to_string(),
                )
                .body(response_body)
                .expect("valid Codex streaming response");
        }
        return match bounded_upstream_body(upstream).await {
            Ok(body) => {
                let mut event = request_telemetry.event(TelemetryStage::Completed);
                event.status = Some(status.as_u16());
                event.time_to_headers_ms = Some(time_to_headers);
                event.elapsed_ms = Some(request_started.elapsed().as_millis() as u64);
                event.response_bytes = Some(body.len() as u64);
                state.telemetry.emit(event);
                state.plugins.emit(
                    infernaut::plugin_protocol::PluginEventKind::ResponseDelta,
                    &event_context,
                    Some("text/event-stream"),
                    Some(String::from_utf8_lossy(&body).into_owned()),
                    None,
                );
                state.plugins.emit(
                    infernaut::plugin_protocol::PluginEventKind::ResponseCompleted,
                    &event_context,
                    None,
                    None,
                    None,
                );
                match codex_adapter::response_sse_to_chat_response(&body, &requested_model) {
                    Ok(value) => json_response_with_routing_headers(
                        status,
                        &value,
                        &request_id,
                        &requested_model,
                        &resolved.qualified_id,
                        &resolved.provider,
                        &resolved.realm,
                        local_only,
                        time_to_headers,
                    ),
                    Err(error) => api_error_for_path(
                        path,
                        StatusCode::BAD_GATEWAY,
                        "provider_error",
                        "invalid_codex_response",
                        &error,
                        json!({}),
                    ),
                }
            }
            Err(error) => {
                let mut event = request_telemetry.event(TelemetryStage::Failed);
                event.status = Some(StatusCode::BAD_GATEWAY.as_u16());
                event.elapsed_ms = Some(request_started.elapsed().as_millis() as u64);
                event.detail = Some(error.to_string());
                state.telemetry.emit(event);
                state.plugins.emit(
                    infernaut::plugin_protocol::PluginEventKind::ProviderError,
                    &event_context,
                    Some("text/plain"),
                    Some(error.to_string()),
                    None,
                );
                api_error_for_path(
                    path,
                    StatusCode::BAD_GATEWAY,
                    "provider_error",
                    "invalid_codex_response",
                    &format!("Unable to read the Codex event stream: {error}"),
                    json!({}),
                )
            }
        };
    }

    let upstream_is_sse = upstream_headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
    if !requested_stream || !status.is_success() || !upstream_is_sse {
        let upstream_body = match bounded_upstream_body(upstream).await {
            Ok(body) => body,
            Err(error) => {
                emit_telemetry_failure(
                    &state,
                    &request_telemetry,
                    StatusCode::BAD_GATEWAY,
                    error.to_string(),
                );
                state.plugins.emit(
                    infernaut::plugin_protocol::PluginEventKind::ProviderError,
                    &event_context,
                    Some("text/plain"),
                    Some(error.to_string()),
                    None,
                );
                return api_error_for_path(
                    path,
                    StatusCode::BAD_GATEWAY,
                    "provider_error",
                    "invalid_upstream_response",
                    &format!("Unable to read the provider response: {error}"),
                    json!({
                        "provider": resolved.provider,
                        "requested_model": requested_model,
                        "resolved_model": resolved.qualified_id,
                    }),
                );
            }
        };
        let (response_body, diagnostic) = diagnostics::annotate_json_body(&upstream_body);
        let mut event = request_telemetry.event(if status.is_success() {
            TelemetryStage::Completed
        } else {
            TelemetryStage::Failed
        });
        event.status = Some(status.as_u16());
        event.time_to_headers_ms = Some(time_to_headers);
        event.elapsed_ms = Some(request_started.elapsed().as_millis() as u64);
        event.response_bytes = Some(response_body.len() as u64);
        state.telemetry.emit(event);
        state.plugins.emit(
            infernaut::plugin_protocol::PluginEventKind::ResponseDelta,
            &event_context,
            upstream_headers
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(String::from_utf8_lossy(&response_body).into_owned()),
            None,
        );
        state.plugins.emit(
            infernaut::plugin_protocol::PluginEventKind::ResponseCompleted,
            &event_context,
            None,
            None,
            None,
        );
        if let Some(diagnostic) = diagnostic.as_ref() {
            warn!(
                endpoint = path,
                requested_model = requested_model,
                resolved_model = resolved.qualified_id,
                provider = resolved.provider,
                diagnostic_code = diagnostic.code,
                diagnostic_summary = diagnostic.summary,
                "provider response matched Infernaut diagnostic"
            );
            state.plugins.emit(
                infernaut::plugin_protocol::PluginEventKind::DiagnosticMatched,
                &event_context,
                Some("text/plain"),
                Some(diagnostic.display_message()),
                Some(diagnostic.code),
            );
        }
        let mut builder = Response::builder()
            .status(status)
            .header("x-infernaut-requested-model", &requested_model)
            .header("x-infernaut-resolved-model", &resolved.qualified_id)
            .header("x-infernaut-provider", &resolved.provider)
            .header("x-infernaut-realm", &resolved.realm)
            .header("x-infernaut-request-id", &request_id)
            .header(
                "x-infernaut-privacy-policy",
                if local_only {
                    "local_only"
                } else {
                    "selective"
                },
            )
            .header(
                "x-infernaut-time-to-headers-ms",
                time_to_headers.to_string(),
            );
        if let Some(diagnostic) = diagnostic {
            builder = builder.header("x-infernaut-diagnostic", diagnostic.code);
        }
        for (name, value) in &upstream_headers {
            if !is_hop_or_length_header(name) {
                builder = builder.header(name, value);
            }
        }
        return builder
            .body(full_body(response_body))
            .expect("valid buffered upstream response");
    }

    let diagnostic_plugins = state.plugins.clone();
    let diagnostic_context = event_context.clone();
    let observed = state.telemetry.observe_stream(
        upstream.bytes_stream(),
        request_telemetry,
        request_started,
        status.as_u16(),
        time_to_headers,
    );
    let stream = diagnostics::annotate_sse_with(
        state
            .plugins
            .observe_stream(observed, event_context, "text/event-stream".into()),
        move |diagnostic| {
            diagnostic_plugins.emit(
                infernaut::plugin_protocol::PluginEventKind::DiagnosticMatched,
                &diagnostic_context,
                Some("text/plain"),
                Some(diagnostic.display_message()),
                Some(diagnostic.code),
            );
        },
    )
    .map_ok(Frame::data);
    let response_body = StreamBody::new(TrackedStream {
        inner: Box::pin(stream),
        _guard: inference_guard,
    })
    .boxed_unsync();
    pre_response_guard.disarm();
    let mut builder = Response::builder()
        .status(status)
        .header("x-infernaut-requested-model", &requested_model)
        .header("x-infernaut-resolved-model", &resolved.qualified_id)
        .header("x-infernaut-provider", &resolved.provider)
        .header("x-infernaut-realm", &resolved.realm)
        .header("x-infernaut-request-id", &request_id)
        .header(
            "x-infernaut-privacy-policy",
            if local_only {
                "local_only"
            } else {
                "selective"
            },
        )
        .header(
            "x-infernaut-time-to-headers-ms",
            time_to_headers.to_string(),
        );

    for (name, value) in &upstream_headers {
        if !is_hop_or_length_header(name) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(response_body)
        .expect("valid upstream response")
}

fn effective_recipe_slot<'a>(
    recipe: &'a RecipeConfig,
    requested_slot: &'a RecipeSlotConfig,
    path: &str,
    payload: &Value,
) -> (&'a RecipeSlotConfig, bool, bool, bool, bool) {
    let compression_detected = match recipe.kind {
        RecipeKind::ClaudeCode => {
            is_anthropic_path(path) && crate::routing::is_claude_code_compression(payload)
        }
        RecipeKind::Hermes => {
            !is_anthropic_path(path) && crate::routing::is_hermes_compression(payload)
        }
        RecipeKind::Generic => false,
    };
    let classifier_detected = !compression_detected
        && recipe.kind == RecipeKind::ClaudeCode
        && is_anthropic_path(path)
        && crate::routing::is_claude_code_classifier(payload);
    let delegation_detected = !compression_detected
        && !classifier_detected
        && recipe.kind == RecipeKind::Hermes
        && !is_anthropic_path(path)
        && crate::routing::is_hermes_delegation(payload);
    let vision_detected = !compression_detected
        && !classifier_detected
        && !delegation_detected
        && recipe.kind == RecipeKind::Hermes
        && !is_anthropic_path(path)
        && crate::routing::is_hermes_vision_analyze(payload);
    (
        if compression_detected {
            recipe.compression_slot().unwrap_or(requested_slot)
        } else if classifier_detected {
            recipe.classifier_slot().unwrap_or(requested_slot)
        } else if delegation_detected {
            recipe.delegation_slot().unwrap_or(requested_slot)
        } else if vision_detected {
            recipe.vision_slot().unwrap_or(requested_slot)
        } else {
            requested_slot
        },
        compression_detected,
        classifier_detected,
        delegation_detected,
        vision_detected,
    )
}

fn apply_recipe_reasoning_effort(
    payload: &mut Value,
    policy: RecipeReasoningEffort,
) -> Option<(Option<String>, &'static str)> {
    let effective = policy.request_value()?;
    let requested = payload
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_owned);
    payload["reasoning_effort"] = Value::String(effective.to_owned());
    Some((requested, effective))
}

fn is_recipe_model_id(model: &str) -> bool {
    model.starts_with("recipe/") || model.starts_with("recipe:")
}

struct TrackedStream<T> {
    inner: Pin<Box<dyn Stream<Item = T> + Send>>,
    _guard: InferenceGuard,
}

impl<T> Stream for TrackedStream<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<T>> {
        self.inner.as_mut().poll_next(context)
    }
}

impl<T> Unpin for TrackedStream<T> {}

async fn send_upstream(request: reqwest::RequestBuilder) -> Result<reqwest::Response, BoxError> {
    // Long cold starts and compaction are expected; this only bounds time to headers.
    let seconds = std::env::var("INFERNAUT_UPSTREAM_HEADER_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(900);
    let response =
        tokio::time::timeout(std::time::Duration::from_secs(seconds), request.send()).await??;
    if response.status().is_redirection() {
        return Err(std::io::Error::other(
            "provider redirect rejected; configure its final URL explicitly",
        )
        .into());
    }
    Ok(response)
}

async fn bounded_upstream_body(mut response: reqwest::Response) -> Result<Bytes, BoxError> {
    const LIMIT: usize = 32 * 1024 * 1024;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > LIMIT.saturating_sub(body.len()) {
            return Err(std::io::Error::other("buffered provider response exceeds 32 MiB").into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

fn body_error_status(error: &(dyn Error + 'static)) -> StatusCode {
    if error.is::<http_body_util::LengthLimitError>() {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::BAD_REQUEST
    }
}

async fn parse_json<T: serde::de::DeserializeOwned>(
    request: Request<Incoming>,
) -> Result<T, Box<Response<ResponseBody>>> {
    let body = Limited::new(request.into_body(), 16 * 1024 * 1024)
        .collect()
        .await
        .map_err(|error| {
            api_error(
                body_error_status(error.as_ref()),
                "invalid_request_error",
                "request_body_error",
                &format!("Unable to read request body: {error}"),
                json!({}),
            )
        })?;
    serde_json::from_slice(&body.to_bytes()).map_err(|error| {
        Box::new(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_json",
            &format!("Request body is not valid JSON: {error}"),
            json!({}),
        ))
    })
}

fn probe_result(result: Result<crate::probes::ProbeJob, ProbeError>) -> Response<ResponseBody> {
    match result {
        Ok(job) => json_response(StatusCode::OK, &job),
        Err(error) => probe_error_response(error),
    }
}

fn serialized_json_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(0, |encoded| encoded.len())
}

fn emit_telemetry_failure(
    state: &AppState,
    request: &RequestTelemetry,
    status: StatusCode,
    detail: String,
) {
    let mut event = request.event(TelemetryStage::Failed);
    event.status = Some(status.as_u16());
    event.detail = Some(detail);
    state.telemetry.emit(event);
}

fn client_identity(
    headers: &hyper::HeaderMap,
    peer: Option<SocketAddr>,
    authenticated_client: Option<String>,
) -> ClientIdentity {
    let explicit = headers
        .get("x-infernaut-client")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let user_agent = headers
        .get(hyper::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let inferred = user_agent
        .as_deref()
        .and_then(infer_client_label)
        .map(str::to_owned);
    let peer_label = peer.map(|peer| peer.ip().to_string());
    let (label, source) = if let Some(label) = authenticated_client {
        (label, "client_key")
    } else if let Some(label) = explicit {
        (label.to_owned(), "header")
    } else if let Some(label) = inferred {
        (label, "user_agent")
    } else if let Some(label) = peer_label.clone() {
        (label, "network")
    } else {
        ("unknown".to_owned(), "unknown")
    };
    ClientIdentity {
        label,
        source: Some(source.to_owned()),
        peer: peer.map(|peer| peer.to_string()),
        user_agent,
    }
}

fn infer_client_label(user_agent: &str) -> Option<&'static str> {
    let normalized = user_agent.to_ascii_lowercase();
    if normalized.contains("claude") {
        Some("Claude Code")
    } else if normalized.contains("hermes") {
        Some("Hermes")
    } else if normalized.contains("opencode") {
        Some("OpenCode")
    } else if normalized.contains("openclaw") {
        Some("OpenClaw")
    } else if normalized.contains("pi-coding-agent") || normalized.contains("pi-agent") {
        Some("Pi")
    } else if normalized.contains("codex") {
        Some("Codex")
    } else {
        None
    }
}

fn estimate_anthropic_input_tokens(payload: &Value) -> u64 {
    // A tokenizer-independent, deliberately conservative fallback for providers
    // such as LM Studio that expose Messages but not count_tokens. Counting the
    // complete wire payload includes message/tool framing and uses three bytes
    // per token so context safety wins over optimistic precision.
    (serialized_json_len(payload) as u64).div_ceil(3).max(1)
}

fn next_request_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("inf_{millis:x}_{sequence:x}")
}

fn probe_error_response(error: ProbeError) -> Response<ResponseBody> {
    let (status, code) = match error {
        ProbeError::Disabled => (StatusCode::CONFLICT, "probes_disabled"),
        ProbeError::ColdStartDenied(_) => (StatusCode::CONFLICT, "cold_start_not_authorized"),
        ProbeError::Model(_) => (StatusCode::BAD_REQUEST, "model_not_found"),
        ProbeError::NotFound(_) => (StatusCode::NOT_FOUND, "probe_not_found"),
    };
    api_error(status, "probe_error", code, &error.to_string(), json!({}))
}

async fn prepare_upstream_model(
    state: &AppState,
    provider: &ProviderConfig,
    resolved: &crate::catalog::ResolvedModel,
    path: &str,
) -> Result<String, Box<Response<ResponseBody>>> {
    if !matches!(
        provider.kind,
        ProviderKind::LmStudio | ProviderKind::LlamaCpp
    ) || resolved.loaded != Some(false)
    {
        return Ok(resolved.upstream_id.clone());
    }
    if provider.jit_load {
        let Some(management_id) = resolved.management_id.as_ref() else {
            return Err(Box::new(api_error_for_path(
                path,
                StatusCode::INTERNAL_SERVER_ERROR,
                "routing_error",
                "management_id_missing",
                "Provider model metadata is missing its native management key.",
                json!({ "model": resolved.qualified_id }),
            )));
        };
        info!(
            model = resolved.qualified_id,
            management_id,
            provider = provider.id,
            runtime = ?provider.kind,
            "delegating unloaded model activation to provider autoload"
        );
        return Ok(management_id.clone());
    }
    if !provider.auto_load {
        return Err(Box::new(api_error_for_path(
            path,
            StatusCode::CONFLICT,
            "routing_error",
            "model_not_loaded",
            "The selected model is available but not loaded, and provider autoload is disabled.",
            json!({
                "model": resolved.qualified_id,
                "provider": provider.id,
            }),
        )));
    }

    let _load_guard = state.model_load_lock.lock().await;
    if let Some(model) = state
        .catalog
        .read()
        .await
        .models
        .iter()
        .find(|model| model.id == resolved.qualified_id && model.loaded == Some(true))
    {
        return Ok(model.upstream_id.clone());
    }

    let Some(management_id) = resolved.management_id.as_ref() else {
        return Err(Box::new(api_error_for_path(
            path,
            StatusCode::INTERNAL_SERVER_ERROR,
            "routing_error",
            "management_id_missing",
            "Provider model metadata is missing its native management key.",
            json!({ "model": resolved.qualified_id }),
        )));
    };
    let inference_base = provider.effective_base_url().trim_end_matches('/');
    let server_base = inference_base.strip_suffix("/v1").unwrap_or(inference_base);

    if provider.kind == ProviderKind::LlamaCpp {
        let url = format!("{server_base}/models/load");
        let mut request = state
            .client
            .post(url)
            .json(&json!({ "model": management_id }));
        if let Some(api_key) = provider.api_key() {
            request = request.bearer_auth(api_key);
        }
        let response = request.send().await.map_err(|error| {
            api_error_for_path(
                path,
                StatusCode::BAD_GATEWAY,
                "provider_error",
                "model_load_failed",
                &format!("llama.cpp could not load the selected model: {error}"),
                json!({
                    "model": resolved.qualified_id,
                    "fallback_attempted": false,
                }),
            )
        })?;
        let status = response.status();
        let response_payload: Value = response.json().await.unwrap_or_else(|error| {
            json!({ "error": { "message": format!("Invalid llama.cpp load response: {error}") } })
        });
        if !status.is_success()
            || response_payload.get("success").and_then(Value::as_bool) == Some(false)
        {
            return Err(Box::new(api_error_for_path(
                path,
                StatusCode::BAD_GATEWAY,
                "provider_error",
                "model_load_failed",
                "llama.cpp rejected the model load request.",
                json!({
                    "model": resolved.qualified_id,
                    "upstream_status": status.as_u16(),
                    "upstream_response": response_payload,
                    "fallback_attempted": false,
                }),
            )));
        }
        let upstream_id = management_id.clone();
        if let Some(model) = state
            .catalog
            .write()
            .await
            .models
            .iter_mut()
            .find(|model| model.id == resolved.qualified_id)
        {
            model.loaded = Some(true);
            model.provider_metadata["status"] = json!("loaded");
        }
        info!(
            model = resolved.qualified_id,
            provider = provider.id,
            "llama.cpp model explicitly loaded"
        );
        return Ok(upstream_id);
    }

    let url = format!("{server_base}/api/v1/models/load");
    let mut load_payload = json!({ "model": management_id });
    if let Some(context_length) = provider.load_context_length {
        load_payload["context_length"] = json!(context_length);
    }
    let mut request = state.client.post(url).json(&load_payload);
    if let Some(api_key) = provider.api_key() {
        request = request.bearer_auth(api_key);
    }

    let response = request.send().await.map_err(|error| {
        api_error_for_path(
            path,
            StatusCode::BAD_GATEWAY,
            "provider_error",
            "model_load_failed",
            &format!("LM Studio could not load the selected model: {error}"),
            json!({
                "model": resolved.qualified_id,
                "fallback_attempted": false,
            }),
        )
    })?;
    let status = response.status();
    let response_payload: Value = response.json().await.unwrap_or_else(|error| {
        json!({ "error": { "message": format!("Invalid LM Studio load response: {error}") } })
    });
    if !status.is_success() {
        return Err(Box::new(api_error_for_path(
            path,
            StatusCode::BAD_GATEWAY,
            "provider_error",
            "model_load_failed",
            "LM Studio rejected the model load request.",
            json!({
                "model": resolved.qualified_id,
                "upstream_status": status.as_u16(),
                "upstream_response": response_payload,
                "fallback_attempted": false,
            }),
        )));
    }
    let Some(instance_id) = response_payload.get("instance_id").and_then(Value::as_str) else {
        return Err(Box::new(api_error_for_path(
            path,
            StatusCode::BAD_GATEWAY,
            "provider_error",
            "invalid_model_load_response",
            "LM Studio loaded the model but did not return an instance_id.",
            json!({ "model": resolved.qualified_id }),
        )));
    };
    let instance_id = instance_id.to_owned();
    info!(
        model = resolved.qualified_id,
        instance_id,
        provider = provider.id,
        "LM Studio model explicitly loaded"
    );

    if let Some(model) = state
        .catalog
        .write()
        .await
        .models
        .iter_mut()
        .find(|model| model.id == resolved.qualified_id)
    {
        model.upstream_id = instance_id.clone();
        model.loaded = Some(true);
        model.provider_metadata["loaded"] = json!(true);
        model.provider_metadata["loaded_instance_ids"] = json!([instance_id.clone()]);
    }

    Ok(instance_id)
}

#[derive(Clone, Debug)]
struct AuthenticatedClient {
    id: String,
    label: String,
    exclusive_recipe: Option<String>,
    selectable_recipes: Vec<String>,
}

impl AuthenticatedClient {
    fn exclusive_recipe<'a>(&self, config: &'a Config) -> Option<&'a RecipeConfig> {
        self.exclusive_recipe
            .as_deref()
            .and_then(|id| config.recipe(id))
    }

    fn selectable_recipe_configs<'a>(&self, config: &'a Config) -> Vec<&'a RecipeConfig> {
        self.selectable_recipes
            .iter()
            .filter_map(|id| config.recipe(id))
            .collect()
    }

    fn recipe_for_request<'a>(
        &self,
        config: &'a Config,
        requested_model: &str,
    ) -> Option<&'a RecipeConfig> {
        self.exclusive_recipe(config).or_else(|| {
            self.selectable_recipes
                .iter()
                .filter_map(|id| config.recipe(id))
                .find(|recipe| recipe.slot_for_request(requested_model).is_some())
        })
    }
}

enum AuthenticatedAs {
    Admin,
    Client(AuthenticatedClient),
}

impl AuthenticatedAs {
    fn client(&self) -> Option<AuthenticatedClient> {
        match self {
            Self::Admin => None,
            Self::Client(client) => Some(client.clone()),
        }
    }

    fn exclusive_recipe<'a>(&self, config: &'a Config) -> Option<&'a RecipeConfig> {
        match self {
            Self::Admin => None,
            Self::Client(client) => client.exclusive_recipe(config),
        }
    }

    fn selectable_recipes<'a>(&self, config: &'a Config) -> Vec<&'a RecipeConfig> {
        match self {
            Self::Admin => Vec::new(),
            Self::Client(client) => client.selectable_recipe_configs(config),
        }
    }
}

fn authenticate_request(request: &Request<Incoming>, config: &Config) -> Option<AuthenticatedAs> {
    let bearer = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let api_key = request
        .headers()
        .get("x-api-key")
        .and_then(|value| value.to_str().ok());
    if let Some(environment_name) = &config.server.api_key_env
        && let Ok(expected) = std::env::var(environment_name)
        && (bearer == Some(expected.as_str()) || api_key == Some(expected.as_str()))
    {
        return Some(AuthenticatedAs::Admin);
    }
    for client in config.clients.iter().filter(|client| !client.disabled) {
        if let Ok(expected) = std::env::var(&client.api_key_env)
            && (bearer == Some(expected.as_str()) || api_key == Some(expected.as_str()))
        {
            return Some(AuthenticatedAs::Client(AuthenticatedClient {
                id: client.id.clone(),
                label: client.display_name().to_owned(),
                exclusive_recipe: client.recipe.clone(),
                selectable_recipes: client.recipes.clone(),
            }));
        }
    }
    if config.server.api_key_env.is_none() && config.clients.iter().all(|client| client.disabled) {
        Some(AuthenticatedAs::Admin)
    } else {
        None
    }
}

fn is_named_client_endpoint(path: &str) -> bool {
    path == "/" || path.starts_with("/v1/")
}

fn is_hop_or_auth_header(name: &hyper::header::HeaderName) -> bool {
    is_hop_or_length_header(name)
        || matches!(
            *name,
            AUTHORIZATION | PROXY_AUTHORIZATION | HOST | CONTENT_TYPE | CONTENT_ENCODING
        )
        || name.as_str().eq_ignore_ascii_case("x-api-key")
        || name.as_str().starts_with("x-infernaut-")
}

fn is_hop_or_length_header(name: &hyper::header::HeaderName) -> bool {
    matches!(*name, CONNECTION | CONTENT_LENGTH)
        || name.as_str().eq_ignore_ascii_case("transfer-encoding")
        || name.as_str().eq_ignore_ascii_case("keep-alive")
        || name.as_str().eq_ignore_ascii_case("upgrade")
}

fn resolve_error_response(path: &str, error: ResolveError) -> Response<ResponseBody> {
    match error {
        ResolveError::RouteContextExceeded {
            route,
            required_tokens,
            context_length,
        } => api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "route_context_length_exceeded",
            "Prompt is too long: input length and `max_tokens` exceed context limit.",
            json!({
                "route": route,
                "estimated_required_tokens": required_tokens,
                "context_length": context_length,
                "fallback_attempted": false,
            }),
        ),
        ResolveError::AliasContextExceeded {
            alias,
            required_tokens,
            context_length,
        } => api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "alias_context_length_exceeded",
            "Prompt is too long: input length and `max_tokens` exceed the alias context contract.",
            json!({
                "alias": alias,
                "estimated_required_tokens": required_tokens,
                "context_length": context_length,
                "fallback_attempted": false,
            }),
        ),
        ResolveError::AliasUnavailable { alias, target } => api_error_for_path(
            path,
            StatusCode::SERVICE_UNAVAILABLE,
            "routing_policy_violation",
            "alias_target_unavailable",
            "The alias's exact target is unavailable or does not satisfy its context contract.",
            json!({
                "alias": alias,
                "target": target,
                "fallback_attempted": false,
            }),
        ),
        ResolveError::NoEligibleModel {
            route,
            allowed_realms,
        } => api_error_for_path(
            path,
            StatusCode::SERVICE_UNAVAILABLE,
            "routing_policy_violation",
            "no_eligible_model",
            "No available model satisfies the route's hard constraints.",
            json!({
                "route": route,
                "allowed_realms": allowed_realms,
                "fallback_attempted": false,
            }),
        ),
        ResolveError::AudioRouteRequired {
            service,
            eligible_routes,
        } => api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "audio_route_required",
            "The audio endpoint cannot choose between multiple eligible targets without an explicit route.",
            json!({
                "service": service,
                "eligible_routes": eligible_routes,
                "fallback_attempted": false,
            }),
        ),
        other => api_error_for_path(
            path,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "model_not_found",
            &other.to_string(),
            json!({}),
        ),
    }
}

fn api_error(
    status: StatusCode,
    error_type: &str,
    code: &str,
    message: &str,
    details: Value,
) -> Response<ResponseBody> {
    json_response(
        status,
        &json!({
            "error": {
                "type": error_type,
                "code": code,
                "message": message,
                "details": details,
            }
        }),
    )
}

fn api_error_for_path(
    path: &str,
    status: StatusCode,
    error_type: &str,
    code: &str,
    message: &str,
    details: Value,
) -> Response<ResponseBody> {
    if is_anthropic_path(path) {
        return json_response(
            status,
            &json!({
                "type": "error",
                "error": {
                    "type": error_type,
                    "code": code,
                    "message": message,
                    "details": details,
                }
            }),
        );
    }
    api_error(status, error_type, code, message, details)
}

fn is_anthropic_path(path: &str) -> bool {
    matches!(path, "/v1/messages" | "/v1/messages/count_tokens")
}

fn is_public_unauthenticated_request(method: &Method, path: &str) -> bool {
    path == "/health" || (*method == Method::HEAD && path == "/api/hello")
}

fn json_response(status: StatusCode, value: &impl serde::Serialize) -> Response<ResponseBody> {
    match serde_json::to_vec(value) {
        Ok(payload) => Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(full_body(payload))
            .expect("valid JSON response"),
        Err(error) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header("content-type", "application/json")
            .body(full_body(format!(
                r#"{{"error":{{"message":"serialization error: {error}"}}}}"#
            )))
            .expect("valid serialization error response"),
    }
}

#[allow(clippy::too_many_arguments)]
fn json_response_with_routing_headers(
    status: StatusCode,
    value: &impl serde::Serialize,
    request_id: &str,
    requested_model: &str,
    resolved_model: &str,
    provider: &str,
    realm: &str,
    local_only: bool,
    time_to_headers: u64,
) -> Response<ResponseBody> {
    match serde_json::to_vec(value) {
        Ok(payload) => Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .header("x-infernaut-request-id", request_id)
            .header("x-infernaut-requested-model", requested_model)
            .header("x-infernaut-resolved-model", resolved_model)
            .header("x-infernaut-provider", provider)
            .header("x-infernaut-realm", realm)
            .header(
                "x-infernaut-privacy-policy",
                if local_only {
                    "local_only"
                } else {
                    "selective"
                },
            )
            .header(
                "x-infernaut-time-to-headers-ms",
                time_to_headers.to_string(),
            )
            .body(full_body(payload))
            .expect("valid JSON response"),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "serialization_error",
            &format!("Unable to serialize the adapted provider response: {error}"),
            json!({}),
        ),
    }
}

fn full_body(body: impl Into<Bytes>) -> ResponseBody {
    Full::new(body.into())
        .map_err(|never| match never {})
        .boxed_unsync()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn chunked_bodies_are_limited_before_collection() {
        let frames = futures_util::stream::iter([
            Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"1234"))),
            Ok(Frame::data(Bytes::from_static(b"5678"))),
        ]);
        let error = Limited::new(StreamBody::new(frames), 7)
            .collect()
            .await
            .unwrap_err();
        assert_eq!(
            body_error_status(error.as_ref()),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn regenerated_json_representation_headers_are_not_forwarded() {
        assert!(is_hop_or_auth_header(&CONTENT_TYPE));
        assert!(is_hop_or_auth_header(&CONTENT_ENCODING));
        assert!(is_hop_or_auth_header(
            &hyper::header::HeaderName::from_static("x-api-key")
        ));
    }

    #[test]
    fn transcription_multipart_rewrites_only_the_model_field() {
        let boundary = "infernaut-boundary";
        let body = b"--infernaut-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"sample.wav\"\r\nContent-Type: audio/wav\r\n\r\n\x00\x01route:private:transcription\xff\r\n--infernaut-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nroute:private:transcription\r\n--infernaut-boundary--\r\n";
        let (model, span) = multipart_model_field(body, boundary).unwrap();
        assert_eq!(model, "route:private:transcription");

        let rewritten = replace_bytes(body, span, b"parakeet");
        assert_eq!(
            multipart_model_field(&rewritten, boundary).unwrap().0,
            "parakeet"
        );
        assert!(
            rewritten
                .windows(b"\x00\x01route:private:transcription\xff".len())
                .any(|window| window == b"\x00\x01route:private:transcription\xff")
        );
    }

    #[test]
    fn transcription_boundary_accepts_quoted_parameter() {
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=\"abc-123\"").unwrap(),
            "abc-123"
        );
    }

    #[test]
    fn claude_code_connectivity_preflight_is_public_but_narrowly_scoped() {
        assert!(is_public_unauthenticated_request(
            &Method::HEAD,
            "/api/hello"
        ));
        assert!(!is_public_unauthenticated_request(
            &Method::GET,
            "/api/hello"
        ));
        assert!(!is_public_unauthenticated_request(
            &Method::HEAD,
            "/v1/models"
        ));
    }

    #[test]
    fn anthropic_token_estimate_is_nonzero_and_conservative() {
        let payload = json!({
            "model": "route:private:auto",
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "Hello world"}],
            "tools": [{"name": "read", "input_schema": {"type": "object"}}]
        });
        let encoded_bytes = serialized_json_len(&payload) as u64;
        assert_eq!(
            estimate_anthropic_input_tokens(&payload),
            encoded_bytes.div_ceil(3)
        );
    }

    #[test]
    fn recipe_reasoning_policy_preserves_inherit_and_overrides_explicit_effort() {
        let mut inherited = json!({ "reasoning_effort": "medium" });
        assert_eq!(
            apply_recipe_reasoning_effort(&mut inherited, RecipeReasoningEffort::Inherit),
            None
        );
        assert_eq!(inherited["reasoning_effort"], "medium");

        let mut overridden = json!({ "reasoning_effort": "medium" });
        assert_eq!(
            apply_recipe_reasoning_effort(&mut overridden, RecipeReasoningEffort::High),
            Some((Some("medium".to_owned()), "high"))
        );
        assert_eq!(overridden["reasoning_effort"], "high");

        let mut disabled = json!({});
        assert_eq!(
            apply_recipe_reasoning_effort(&mut disabled, RecipeReasoningEffort::None),
            Some((None, "none"))
        );
        assert_eq!(disabled["reasoning_effort"], "none");
    }

    #[test]
    fn claude_recipe_dispatches_only_narrow_internal_signatures() {
        let recipe = RecipeConfig {
            id: "claude-lab".to_owned(),
            title: String::new(),
            kind: RecipeKind::ClaudeCode,
            protocol: RecipeProtocol::Anthropic,
            allowed_realms: vec!["local".to_owned()],
            slots: vec![
                RecipeSlotConfig {
                    id: "primary".to_owned(),
                    title: String::new(),
                    target: "agamotto/main".to_owned(),
                    trigger: crate::config::RecipeSlotTrigger::Model,
                    max_output_tokens: None,
                    reasoning_effort: crate::config::RecipeReasoningEffort::Inherit,
                },
                RecipeSlotConfig {
                    id: "classifier".to_owned(),
                    title: String::new(),
                    target: "agamotto/classifier".to_owned(),
                    trigger: crate::config::RecipeSlotTrigger::Classifier,
                    max_output_tokens: Some(1024),
                    reasoning_effort: crate::config::RecipeReasoningEffort::Inherit,
                },
                RecipeSlotConfig {
                    id: "compression".to_owned(),
                    title: String::new(),
                    target: "agamotto/compressor".to_owned(),
                    trigger: crate::config::RecipeSlotTrigger::Compression,
                    max_output_tokens: Some(16_384),
                    reasoning_effort: crate::config::RecipeReasoningEffort::Inherit,
                },
            ],
            disabled: false,
        };
        let primary = &recipe.slots[0];
        let ordinary = json!({"messages": [{"role": "user", "content": "hello"}]});
        let (selected, compression, classifier, delegation, vision) =
            effective_recipe_slot(&recipe, primary, "/v1/messages", &ordinary);
        assert_eq!(selected.id, "primary");
        assert!(!compression);
        assert!(!classifier);
        assert!(!delegation);
        assert!(!vision);

        let classifier_request = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "<transcript>\nuser asked to edit a file\nErr on the side of blocking. Your ENTIRE response MUST begin with <block>. Do NOT output any analysis, reasoning, or commentary before <block>. No preamble."
                }]
            }]
        });
        let (selected, compression, classifier, delegation, vision) =
            effective_recipe_slot(&recipe, primary, "/v1/messages", &classifier_request);
        assert_eq!(selected.id, "classifier");
        assert!(!compression);
        assert!(classifier);
        assert!(!delegation);
        assert!(!vision);

        let classifier_discussion = json!({
            "messages": [{"role": "user", "content": "Explain the auto mode classifier and <block> output."}]
        });
        let (selected, compression, classifier, delegation, vision) =
            effective_recipe_slot(&recipe, primary, "/v1/messages", &classifier_discussion);
        assert_eq!(selected.id, "primary");
        assert!(!compression);
        assert!(!classifier);
        assert!(!delegation);
        assert!(!vision);

        let compact = json!({
            "messages": [{
                "role": "user",
                "content": "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools. Your entire response must be plain text: an <analysis> block followed by a <summary> block. Your task is to create a detailed summary of the conversation so far"
            }]
        });
        let (selected, compression, classifier, delegation, vision) =
            effective_recipe_slot(&recipe, primary, "/v1/messages", &compact);
        assert_eq!(selected.id, "compression");
        assert!(compression);
        assert!(!classifier);
        assert!(!delegation);
        assert!(!vision);
    }

    #[test]
    fn hermes_recipe_dispatches_structured_compression_but_not_user_summaries() {
        let recipe = RecipeConfig {
            id: "hermes-private".to_owned(),
            title: "Hermes private".to_owned(),
            kind: RecipeKind::Hermes,
            protocol: RecipeProtocol::Openai,
            allowed_realms: vec!["local".to_owned()],
            slots: vec![
                RecipeSlotConfig {
                    id: "primary".to_owned(),
                    title: "Primary".to_owned(),
                    target: "agamotto/main".to_owned(),
                    trigger: crate::config::RecipeSlotTrigger::Model,
                    max_output_tokens: None,
                    reasoning_effort: crate::config::RecipeReasoningEffort::Inherit,
                },
                RecipeSlotConfig {
                    id: "delegation".to_owned(),
                    title: "Delegation".to_owned(),
                    target: "agamotto/worker".to_owned(),
                    trigger: crate::config::RecipeSlotTrigger::Delegation,
                    max_output_tokens: None,
                    reasoning_effort: crate::config::RecipeReasoningEffort::None,
                },
                RecipeSlotConfig {
                    id: "vision".to_owned(),
                    title: "Vision".to_owned(),
                    target: "agamotto/vision".to_owned(),
                    trigger: crate::config::RecipeSlotTrigger::Vision,
                    max_output_tokens: None,
                    reasoning_effort: crate::config::RecipeReasoningEffort::None,
                },
                RecipeSlotConfig {
                    id: "compression".to_owned(),
                    title: "Compression".to_owned(),
                    target: "agamotto/compressor".to_owned(),
                    trigger: crate::config::RecipeSlotTrigger::Compression,
                    max_output_tokens: Some(12_000),
                    reasoning_effort: crate::config::RecipeReasoningEffort::None,
                },
            ],
            disabled: false,
        };
        let primary = &recipe.slots[0];
        let ordinary = json!({
            "messages": [{"role": "user", "content": "Summarize this report."}]
        });
        let (selected, compression, classifier, delegation, vision) =
            effective_recipe_slot(&recipe, primary, "/v1/chat/completions", &ordinary);
        assert_eq!(selected.id, "primary");
        assert!(!compression);
        assert!(!classifier);
        assert!(!delegation);
        assert!(!vision);

        let delegated = json!({
            "messages": [
                {
                    "role": "system",
                    "content": "You are a focused subagent working on a specific delegated task.\n\nYOUR TASK:\nInspect the router.\n\nComplete this task using the tools available to you. Your response is returned to the parent agent as a summary."
                },
                {"role": "user", "content": "Inspect the router."}
            ]
        });
        let (selected, compression, classifier, delegation, vision) =
            effective_recipe_slot(&recipe, primary, "/v1/chat/completions", &delegated);
        assert_eq!(selected.id, "delegation");
        assert!(!compression);
        assert!(!classifier);
        assert!(delegation);
        assert!(!vision);

        let vision_request = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Describe the layout and answer the user's question."},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}}
                ]
            }],
            "temperature": 0.1
        });
        let (selected, compression, classifier, delegation, vision) =
            effective_recipe_slot(&recipe, primary, "/v1/chat/completions", &vision_request);
        assert_eq!(selected.id, "vision");
        assert!(!compression);
        assert!(!classifier);
        assert!(!delegation);
        assert!(vision);

        let ordinary_image_chat = json!({
            "messages": [
                {"role": "system", "content": "You are Hermes, a helpful agent."},
                {"role": "user", "content": [
                    {"type": "text", "text": "What is in this image?"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}}
                ]}
            ],
            "tools": [{"type": "function", "function": {"name": "read_file"}}],
            "max_tokens": 2000
        });
        let (selected, compression, classifier, delegation, vision) = effective_recipe_slot(
            &recipe,
            primary,
            "/v1/chat/completions",
            &ordinary_image_chat,
        );
        assert_eq!(selected.id, "primary");
        assert!(!compression);
        assert!(!classifier);
        assert!(!delegation);
        assert!(!vision);

        let compact = json!({
            "messages": [{
                "role": "user",
                "content": "You are a summarization agent creating a context checkpoint. Produce only the structured summary.\nTURNS TO SUMMARIZE:\n[history]\nUse this exact structure:\n## Historical Task Snapshot"
            }]
        });
        let (selected, compression, classifier, delegation, vision) =
            effective_recipe_slot(&recipe, primary, "/v1/chat/completions", &compact);
        assert_eq!(selected.id, "compression");
        assert!(compression);
        assert!(!classifier);
        assert!(!delegation);
        assert!(!vision);

        let delegated_compact = json!({
            "messages": [
                {
                    "role": "system",
                    "content": "You are a focused subagent working on a specific delegated task.\n\nYOUR TASK:\nInspect the router.\n\nComplete this task using the tools available to you. Your response is returned to the parent agent as a summary."
                },
                {
                    "role": "user",
                    "content": "You are a summarization agent creating a context checkpoint. Produce only the structured summary.\nTURNS TO SUMMARIZE:\n[history]\nUse this exact structure:\n## Historical Task Snapshot"
                }
            ]
        });
        let (selected, compression, classifier, delegation, vision) =
            effective_recipe_slot(&recipe, primary, "/v1/chat/completions", &delegated_compact);
        assert_eq!(selected.id, "compression");
        assert!(compression);
        assert!(!classifier);
        assert!(!delegation);
        assert!(!vision);
    }

    #[tokio::test]
    async fn anthropic_context_errors_trigger_compaction_instead_of_retry() {
        let response = resolve_error_response(
            "/v1/messages",
            ResolveError::RouteContextExceeded {
                route: "private/auto".to_owned(),
                required_tokens: 262_253,
                context_length: 262_144,
            },
        );
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let payload: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["type"], "error");
        assert_eq!(payload["error"]["type"], "invalid_request_error");
        assert_eq!(payload["error"]["code"], "route_context_length_exceeded");
        assert!(
            payload["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Prompt is too long")
        );
        assert_eq!(
            payload["error"]["details"]["estimated_required_tokens"],
            262_253
        );
        assert_eq!(payload["error"]["details"]["context_length"], 262_144);
    }
}
