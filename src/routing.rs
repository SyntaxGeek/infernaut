use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::warn;

use crate::{
    catalog::{Catalog, DiscoveredModel, ResolveError},
    config::{
        Config, RouteConfig, RoutingEffort, RoutingModelConfig, RoutingModelRole, RoutingOperation,
        RoutingRuleConfig, RoutingTask,
    },
};

#[derive(Clone, Debug, Serialize)]
pub struct Classification {
    pub task: RoutingTask,
    pub operation: RoutingOperation,
    pub effort: RoutingEffort,
    pub confidence: f64,
    pub vision_input: bool,
    pub compression_request: bool,
    pub intent_bytes: usize,
    pub scaffold_bytes: usize,
    pub estimated_input_tokens: u64,
    pub requested_output_tokens: u64,
    pub reserved_output_tokens: u64,
    pub estimated_required_tokens: u64,
    pub signals: Vec<String>,
}

impl Classification {
    pub fn strength_key(&self) -> String {
        format!("{}.{}", self.task.as_str(), self.operation.as_str())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CandidateScore {
    pub model: String,
    pub score: f64,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RejectedCandidate {
    pub model: String,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RoutingDecision {
    pub route: String,
    pub classification: Classification,
    pub selected_model: String,
    pub selection_reasons: Vec<String>,
    pub candidates: Vec<CandidateScore>,
    pub rejected: Vec<RejectedCandidate>,
    pub tool_loop_pinned: bool,
}

#[derive(Clone, Debug)]
pub struct RoutingOutcome {
    pub selected: DiscoveredModel,
    pub decision: RoutingDecision,
}

#[derive(Clone)]
pub struct RoutingEngine {
    sessions: Arc<Mutex<HashMap<String, SessionSelection>>>,
    scope: String,
}

impl Default for RoutingEngine {
    fn default() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            scope: String::new(),
        }
    }
}

#[derive(Clone)]
struct SessionSelection {
    model: String,
    task: RoutingTask,
    operation: RoutingOperation,
    effort: RoutingEffort,
    seen_at: Instant,
}

#[derive(Clone, Copy, Default)]
struct EvaluationConstraints {
    require_anthropic_messages: bool,
    compression_hint: bool,
}

impl RoutingEngine {
    pub fn scoped(&self, client: &str, session: Option<&str>) -> Self {
        Self {
            sessions: self.sessions.clone(),
            scope: serde_json::to_string(&(client, session)).expect("string tuple serializes"),
        }
    }

    pub async fn evaluate(
        &self,
        config: &Config,
        catalog: &Catalog,
        route: &RouteConfig,
        payload: &Value,
        force_local: bool,
    ) -> Result<RoutingOutcome, ResolveError> {
        self.evaluate_with_compression_hint(config, catalog, route, payload, force_local, false)
            .await
    }

    pub async fn evaluate_with_compression_hint(
        &self,
        config: &Config,
        catalog: &Catalog,
        route: &RouteConfig,
        payload: &Value,
        force_local: bool,
        compression_hint: bool,
    ) -> Result<RoutingOutcome, ResolveError> {
        self.evaluate_with_protocol(
            config,
            catalog,
            route,
            payload,
            force_local,
            EvaluationConstraints {
                compression_hint,
                ..EvaluationConstraints::default()
            },
        )
        .await
    }

    pub async fn evaluate_for_anthropic_messages(
        &self,
        config: &Config,
        catalog: &Catalog,
        route: &RouteConfig,
        payload: &Value,
        force_local: bool,
    ) -> Result<RoutingOutcome, ResolveError> {
        self.evaluate_for_anthropic_messages_with_compression_hint(
            config,
            catalog,
            route,
            payload,
            force_local,
            false,
        )
        .await
    }

    pub async fn evaluate_for_anthropic_messages_with_compression_hint(
        &self,
        config: &Config,
        catalog: &Catalog,
        route: &RouteConfig,
        payload: &Value,
        force_local: bool,
        compression_hint: bool,
    ) -> Result<RoutingOutcome, ResolveError> {
        self.evaluate_with_protocol(
            config,
            catalog,
            route,
            payload,
            force_local,
            EvaluationConstraints {
                require_anthropic_messages: true,
                compression_hint,
            },
        )
        .await
    }

    async fn evaluate_with_protocol(
        &self,
        config: &Config,
        catalog: &Catalog,
        route: &RouteConfig,
        payload: &Value,
        force_local: bool,
        constraints: EvaluationConstraints,
    ) -> Result<RoutingOutcome, ResolveError> {
        let classification = classify_with_token_estimate(
            payload,
            route.task,
            route.effort,
            route.token_estimate_bytes_per_token,
            route.compression_max_output_tokens,
            constraints.compression_hint,
        );
        if let Some(context_length) = route.context_length
            && classification.estimated_required_tokens > context_length
        {
            return Err(ResolveError::RouteContextExceeded {
                route: route.id.clone(),
                required_tokens: classification.estimated_required_tokens,
                context_length,
            });
        }
        let (mut eligible, rejected, context_ceiling) = eligible_models(
            config,
            catalog,
            route,
            payload,
            &classification,
            force_local,
            constraints.require_anthropic_messages,
        );
        if eligible.is_empty() {
            if let Some(context_length) = context_ceiling {
                warn!(
                    route = route.id,
                    compression_request = classification.compression_request,
                    estimated_required_tokens = classification.estimated_required_tokens,
                    context_length,
                    allowed_realms = ?effective_realms(route, force_local),
                    "adaptive route request exceeds every otherwise eligible model context limit"
                );
                return Err(ResolveError::RouteContextExceeded {
                    route: route.id.clone(),
                    required_tokens: classification.estimated_required_tokens,
                    context_length,
                });
            }
            let rejected_preview = rejected
                .iter()
                .take(8)
                .map(|candidate| format!("{}={}", candidate.model, candidate.reasons.join("+")))
                .collect::<Vec<_>>();
            warn!(
                route = route.id,
                compression_request = classification.compression_request,
                estimated_required_tokens = classification.estimated_required_tokens,
                allowed_realms = ?effective_realms(route, force_local),
                rejected = ?rejected_preview,
                rejected_count = rejected.len(),
                "adaptive route rejected all candidate models"
            );
            return Err(ResolveError::NoEligibleModel {
                route: route.id.clone(),
                allowed_realms: effective_realms(route, force_local),
            });
        }

        let session_key = format!("{}:{}", self.scope, conversation_key(&route.id, payload));
        let tool_loop = tool_loop_active(payload);
        let mut tool_loop_escalated = false;
        if tool_loop {
            let ttl = Duration::from_secs(config.routing.session_ttl_seconds);
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, selection| selection.seen_at.elapsed() <= ttl);
            if let Some(selection) = sessions.get_mut(&session_key)
                && let Some(model) = eligible
                    .iter()
                    .find(|model| model.id == selection.model)
                    .cloned()
            {
                selection.seen_at = Instant::now();
                if tool_loop_selection_should_escalate(selection, &classification) {
                    tool_loop_escalated = true;
                } else {
                    let score = CandidateScore {
                        model: model.id.clone(),
                        score: 1000.0,
                        reasons: vec!["tool_loop_pin".to_owned()],
                    };
                    return Ok(RoutingOutcome {
                        selected: model.clone(),
                        decision: RoutingDecision {
                            route: route.id.clone(),
                            classification,
                            selected_model: model.id,
                            selection_reasons: vec!["tool_loop_pin".to_owned()],
                            candidates: vec![score],
                            rejected,
                            tool_loop_pinned: true,
                        },
                    });
                }
            }
        }

        if let Some(rule) = matching_rule(&config.routing.rules, route, &classification)
            && let Some((position, selected)) =
                rule.prefer
                    .iter()
                    .enumerate()
                    .find_map(|(position, preferred)| {
                        eligible
                            .iter()
                            .find(|model| model.id == *preferred)
                            .cloned()
                            .map(|model| (position, model))
                    })
        {
            let mut reasons = vec![
                "explicit_rule".to_owned(),
                format!("rule_preference={}", position + 1),
            ];
            if tool_loop_escalated {
                reasons.push("tool_loop_escalation".to_owned());
            }
            if !classification.compression_request {
                remember_selection(
                    &self.sessions,
                    session_key,
                    selected.id.clone(),
                    &classification,
                    config.routing.session_ttl_seconds,
                )
                .await;
            }
            return Ok(RoutingOutcome {
                selected: selected.clone(),
                decision: RoutingDecision {
                    route: route.id.clone(),
                    classification,
                    selected_model: selected.id.clone(),
                    selection_reasons: reasons.clone(),
                    candidates: vec![CandidateScore {
                        model: selected.id,
                        score: 900.0 - position as f64,
                        reasons,
                    }],
                    rejected,
                    tool_loop_pinned: false,
                },
            });
        }

        let mut scores = eligible
            .drain(..)
            .map(|model| {
                let score = score_model(config, route, &classification, &model);
                (model, score)
            })
            .collect::<Vec<_>>();
        scores.sort_by(|left, right| {
            right
                .1
                .score
                .total_cmp(&left.1.score)
                .then_with(|| left.0.id.cmp(&right.0.id))
        });

        let selected = scores
            .first()
            .expect("eligible models produced a score")
            .0
            .clone();
        let mut selection_reasons = scores[0].1.reasons.clone();
        let mut candidate_scores = scores
            .into_iter()
            .map(|(_, score)| score)
            .collect::<Vec<_>>();
        if tool_loop_escalated {
            selection_reasons.push("tool_loop_escalation".to_owned());
            if let Some(selected_score) = candidate_scores.first_mut() {
                selected_score
                    .reasons
                    .push("tool_loop_escalation".to_owned());
            }
        }
        if !classification.compression_request {
            remember_selection(
                &self.sessions,
                session_key,
                selected.id.clone(),
                &classification,
                config.routing.session_ttl_seconds,
            )
            .await;
        }

        Ok(RoutingOutcome {
            selected: selected.clone(),
            decision: RoutingDecision {
                route: route.id.clone(),
                classification,
                selected_model: selected.id,
                selection_reasons,
                candidates: candidate_scores,
                rejected,
                tool_loop_pinned: false,
            },
        })
    }
}

async fn remember_selection(
    sessions: &Mutex<HashMap<String, SessionSelection>>,
    key: String,
    model: String,
    classification: &Classification,
    ttl_seconds: u64,
) {
    let ttl = Duration::from_secs(ttl_seconds);
    let mut sessions = sessions.lock().await;
    sessions.retain(|_, selection| selection.seen_at.elapsed() <= ttl);
    if sessions.len() >= 4096
        && !sessions.contains_key(&key)
        && let Some(oldest) = sessions
            .iter()
            .min_by_key(|(_, selection)| selection.seen_at)
            .map(|(key, _)| key.clone())
    {
        sessions.remove(&oldest);
    }
    sessions.insert(
        key,
        SessionSelection {
            model,
            task: classification.task,
            operation: classification.operation,
            effort: classification.effort,
            seen_at: Instant::now(),
        },
    );
}

fn tool_loop_selection_should_escalate(
    selection: &SessionSelection,
    classification: &Classification,
) -> bool {
    let active_code_mutation = classification
        .signals
        .iter()
        .any(|signal| signal == "active_code_mutation");
    if !active_code_mutation {
        return false;
    }

    let task_escalated =
        selection.task != RoutingTask::Code && classification.task == RoutingTask::Code;
    let operation_escalated = !matches!(
        selection.operation,
        RoutingOperation::Modify
            | RoutingOperation::Implement
            | RoutingOperation::Architect
            | RoutingOperation::Migrate
    ) && matches!(
        classification.operation,
        RoutingOperation::Modify | RoutingOperation::Implement
    );
    let effort_escalated =
        routing_effort_rank(classification.effort) > routing_effort_rank(selection.effort);

    task_escalated || operation_escalated || effort_escalated
}

fn routing_effort_rank(effort: RoutingEffort) -> u8 {
    match effort {
        RoutingEffort::Auto => 0,
        RoutingEffort::Light => 1,
        RoutingEffort::Standard => 2,
        RoutingEffort::Heavy => 3,
    }
}

fn eligible_models(
    config: &Config,
    catalog: &Catalog,
    route: &RouteConfig,
    payload: &Value,
    classification: &Classification,
    force_local: bool,
    require_anthropic_messages: bool,
) -> (Vec<DiscoveredModel>, Vec<RejectedCandidate>, Option<u64>) {
    let allowed = |model: &&DiscoveredModel| {
        (!force_local || model.realm == "local")
            && (route.allowed_realms.is_empty()
                || route
                    .allowed_realms
                    .iter()
                    .any(|realm| realm == &model.realm))
            && config.provider(&model.provider).is_some()
    };

    let realm_eligible = if route.candidates.is_empty() {
        catalog
            .models
            .iter()
            .filter(allowed)
            .cloned()
            .collect::<Vec<_>>()
    } else {
        route
            .candidates
            .iter()
            .filter_map(|candidate| {
                catalog
                    .models
                    .iter()
                    .find(|model| model.id == *candidate && allowed(model))
                    .cloned()
            })
            .collect::<Vec<_>>()
    };

    let compression_profiles_configured = classification.compression_request
        && realm_eligible.iter().any(|model| {
            routing_profile(config, model)
                .is_some_and(|profile| profile.roles.contains(&RoutingModelRole::Compression))
        });
    let mut eligible = Vec::new();
    let mut rejected = Vec::new();
    let mut context_ceiling = None;
    for model in realm_eligible {
        if model.is_audio_service() {
            rejected.push(RejectedCandidate {
                model: model.id,
                reasons: vec!["audio_service_not_chat_model".to_owned()],
            });
            continue;
        }
        let profile = routing_profile(config, &model);
        let compression_model =
            profile.is_some_and(|profile| profile.roles.contains(&RoutingModelRole::Compression));
        if compression_profiles_configured && !compression_model {
            rejected.push(RejectedCandidate {
                model: model.id,
                reasons: vec!["compression_role_required".to_owned()],
            });
            continue;
        }
        if let Some(profile) = profile
            && !profile.auto_routing
            && !(classification.compression_request && compression_model)
        {
            rejected.push(RejectedCandidate {
                model: model.id,
                reasons: vec![format!("auto_routing_disabled(profile={})", profile.model)],
            });
            continue;
        }
        let mut reasons = Vec::new();
        if require_anthropic_messages
            && !config
                .provider(&model.provider)
                .is_some_and(|provider| provider.supports_anthropic_messages())
        {
            reasons.push(format!(
                "anthropic_messages_unsupported(provider={})",
                model.provider
            ));
        }
        reasons.extend(capability_rejections(
            &model,
            payload,
            classification.estimated_required_tokens,
            route.context_length,
        ));
        if reasons.is_empty() {
            eligible.push(model);
        } else {
            if reasons.len() == 1
                && reasons[0].starts_with("context_too_small(")
                && let Some(limit) = model.context_length().filter(|limit| *limit > 0)
            {
                context_ceiling =
                    Some(context_ceiling.map_or(limit, |ceiling: u64| ceiling.max(limit)));
            }
            rejected.push(RejectedCandidate {
                model: model.id,
                reasons,
            });
        }
    }
    (eligible, rejected, context_ceiling)
}

fn routing_profile<'a>(
    config: &'a Config,
    model: &DiscoveredModel,
) -> Option<&'a RoutingModelConfig> {
    config
        .routing
        .models
        .iter()
        .find(|profile| !profile.disabled && wildcard_matches(&profile.model, &model.id))
}

fn effective_realms(route: &RouteConfig, force_local: bool) -> Vec<String> {
    if force_local {
        vec!["local".to_owned()]
    } else {
        route.allowed_realms.clone()
    }
}

fn capability_rejections(
    model: &DiscoveredModel,
    payload: &Value,
    required_tokens: u64,
    route_context_length: Option<u64>,
) -> Vec<String> {
    let mut reasons = Vec::new();
    let has_images = request_has_images(payload);
    if has_images && capability_support(model, "vision") == Some("unsupported") {
        reasons.push("vision_unsupported".to_owned());
    }

    if let Some(requirement) = tool_requirement(payload)
        && capability_support(model, "tools") == Some("unsupported")
    {
        reasons.push(format!("tools_unsupported(required_by={requirement})"));
    }

    if payload.get("response_format").is_some()
        && capability_support(model, "structured_output") == Some("unsupported")
    {
        reasons.push("structured_output_unsupported".to_owned());
    }

    let context_limit = model.context_length();
    match (route_context_length, context_limit) {
        (Some(contract), Some(limit)) if limit < contract => {
            reasons.push(format!(
                "context_below_route_contract(limit={limit},contract={contract})"
            ));
        }
        (Some(contract), None) => {
            reasons.push(format!("context_unknown(route_contract={contract})"));
        }
        _ => {}
    }
    if let Some(context_limit) = context_limit.filter(|limit| *limit > 0)
        && required_tokens > context_limit
    {
        reasons.push(format!(
            "context_too_small(required~{},limit={context_limit})",
            required_tokens
        ));
    }
    reasons
}

fn capability_support<'a>(model: &'a DiscoveredModel, capability: &str) -> Option<&'a str> {
    model
        .capabilities
        .get(capability)
        .and_then(|value| value.get("support"))
        .and_then(Value::as_str)
}

fn matching_rule<'a>(
    rules: &'a [RoutingRuleConfig],
    route: &RouteConfig,
    classification: &Classification,
) -> Option<&'a RoutingRuleConfig> {
    rules.iter().find(|rule| {
        rule.route.as_ref().is_none_or(|id| id == &route.id)
            && rule.task.is_none_or(|task| task == classification.task)
            && rule
                .operation
                .is_none_or(|operation| operation == classification.operation)
            && rule
                .effort
                .is_none_or(|effort| effort == classification.effort)
    })
}

fn score_model(
    config: &Config,
    route: &RouteConfig,
    classification: &Classification,
    model: &DiscoveredModel,
) -> CandidateScore {
    let profile = routing_profile(config, model);
    let mut score = 50.0;
    let mut reasons = Vec::new();

    if let Some(profile) = profile {
        let (strength, key) = model_strength(profile, classification);
        score = strength * 100.0;
        reasons.push(format!("strength:{key}={strength:.2}"));

        if profile
            .traits
            .preferred_effort
            .contains(&classification.effort)
        {
            score += 12.0;
            reasons.push("preferred_effort".to_owned());
        }
        if classification.effort == RoutingEffort::Light {
            score -= f64::from(profile.traits.latency_tier) * 4.0;
            score -= f64::from(profile.traits.cost_tier) * 2.0;
            if profile.traits.latency_tier <= 1 {
                reasons.push("low_latency_tier".to_owned());
            }
        } else if classification.effort == RoutingEffort::Heavy {
            score -= f64::from(profile.traits.cost_tier);
        }
        if !profile.traits.architecture.is_empty() {
            reasons.push(format!("architecture={}", profile.traits.architecture));
        }
        if classification.vision_input && profile.roles.contains(&RoutingModelRole::Vision) {
            score += 15.0;
            reasons.push("vision_role".to_owned());
        }
        if classification.compression_request
            && profile.roles.contains(&RoutingModelRole::Compression)
        {
            reasons.push("compression_role".to_owned());
            if !profile.auto_routing {
                reasons.push("reserved_compression_model".to_owned());
            }
        }
    } else {
        reasons.push("default_strength=0.50".to_owned());
    }

    if model.loaded == Some(true) {
        let bonus = config
            .routing
            .loaded_model_preference
            .bonus(classification.effort);
        score += bonus;
        reasons.push(format!(
            "already_loaded:{}(+{bonus:.3})",
            config.routing.loaded_model_preference.as_str()
        ));
    }

    if let Some(position) = route
        .candidates
        .iter()
        .position(|candidate| candidate == &model.id)
    {
        score += (route.candidates.len().saturating_sub(position) as f64) / 100.0;
        reasons.push(format!("route_candidate={}", position + 1));
    }

    CandidateScore {
        model: model.id.clone(),
        score,
        reasons,
    }
}

fn model_strength(profile: &RoutingModelConfig, classification: &Classification) -> (f64, String) {
    let keys = [
        classification.strength_key(),
        classification.task.as_str().to_owned(),
        classification.operation.as_str().to_owned(),
        "default".to_owned(),
    ];
    for key in keys {
        if let Some(value) = profile.strengths.get(&key) {
            return (*value, key);
        }
    }
    (0.5, "default".to_owned())
}

#[cfg(test)]
pub fn classify(
    payload: &Value,
    forced_task: RoutingTask,
    forced_effort: RoutingEffort,
) -> Classification {
    classify_with_token_estimate(payload, forced_task, forced_effort, None, None, false)
}

fn classify_with_token_estimate(
    payload: &Value,
    forced_task: RoutingTask,
    forced_effort: RoutingEffort,
    token_estimate_bytes_per_token: Option<f64>,
    compression_max_output_tokens: Option<u64>,
    compression_hint: bool,
) -> Classification {
    let evidence = intent_evidence(payload);
    let latest = evidence.latest.to_ascii_lowercase();
    let user_context = evidence.user_context;
    let mut signals = Vec::new();
    if let Some(bytes_per_token) = token_estimate_bytes_per_token {
        signals.push(format!("token_estimate_bytes_per_token={bytes_per_token}"));
    }
    if evidence.scaffold_bytes >= 10_000 {
        signals.push("harness_scaffold_excluded".to_owned());
    }

    let claude_code_compaction = is_claude_code_compaction(payload, &latest);
    let compression_request = compression_hint || claude_code_compaction;
    if claude_code_compaction {
        signals.push("claude_code_compaction".to_owned());
    } else if compression_hint {
        signals.push("recipe_compression".to_owned());
    }
    let output_cap = compression_request
        .then_some(compression_max_output_tokens)
        .flatten();
    if let Some(limit) = output_cap {
        signals.push(format!("compression_max_output_tokens={limit}"));
    }

    let code_keywords = [
        "```",
        "code",
        "function",
        "class ",
        "method",
        "compiler",
        "stack trace",
        "exception",
        "repository",
        "codebase",
        "cargo ",
        "npm ",
        "python",
        "rust",
        "typescript",
        "javascript",
        ".rs",
        ".py",
        ".ts",
        ".js",
        ".go",
        ".java",
        "pull request",
        "git ",
    ];
    let data_keywords = [
        "sql",
        "csv",
        "dataframe",
        "dataset",
        "query",
        "table",
        "spreadsheet",
        "regression",
        "correlation",
        "chart",
        "dashboard",
        "metric",
        "analytics",
        "analyze the data",
    ];
    let code_score = keyword_score(&latest, &code_keywords);
    let data_score = keyword_score(&latest, &data_keywords);

    let mut task = if forced_task != RoutingTask::Auto {
        signals.push(format!("route_task={}", forced_task.as_str()));
        forced_task
    } else if code_score > data_score && code_score > 0 {
        signals.push(format!("code_signals={code_score}"));
        RoutingTask::Code
    } else if data_score > 0 {
        signals.push(format!("data_signals={data_score}"));
        RoutingTask::Data
    } else {
        signals.push("general_language".to_owned());
        RoutingTask::Chat
    };

    let mut operation = if compression_request {
        signals.push("operation=summarize".to_owned());
        RoutingOperation::Summarize
    } else {
        classify_operation(task, &latest, &mut signals)
    };
    let mut effort = if forced_effort != RoutingEffort::Auto {
        signals.push(format!("route_effort={}", forced_effort.as_str()));
        forced_effort
    } else {
        classify_effort(
            operation,
            &latest,
            &user_context,
            evidence.user_turns,
            &mut signals,
        )
    };

    let dominant = code_score.max(data_score) as f64;
    let secondary = code_score.min(data_score) as f64;
    let mut confidence = if forced_task != RoutingTask::Auto {
        1.0
    } else if dominant == 0.0 {
        0.62
    } else {
        (0.68 + ((dominant - secondary) / (dominant + 1.0)) * 0.25).min(0.98)
    };

    if !compression_request && tool_loop_active(payload) {
        let activity = recent_code_work_activity(payload);
        if activity.mutation_calls > 0 && activity.code_files > 0 {
            signals.push("active_code_mutation".to_owned());
            signals.push(format!("recent_code_mutations={}", activity.mutation_calls));
            if activity.code_files > 0 {
                signals.push(format!("recent_code_files={}", activity.code_files));
            }
            signals.push(format!("recent_mutation_bytes={}", activity.mutation_bytes));

            if forced_task == RoutingTask::Auto {
                task = RoutingTask::Code;
            }
            if task == RoutingTask::Code {
                let structural_operation = if activity.write_calls > 0
                    || activity.code_files >= 2
                    || activity.mutation_bytes >= 8_000
                {
                    RoutingOperation::Implement
                } else {
                    RoutingOperation::Modify
                };
                if operation_intensity(structural_operation) > operation_intensity(operation) {
                    operation = structural_operation;
                }
            }
            if forced_effort == RoutingEffort::Auto
                && matches!(task, RoutingTask::Code | RoutingTask::Data)
            {
                let structural_effort = if activity.code_files >= 2
                    || activity.mutation_calls >= 3
                    || activity.mutation_bytes >= 12_000
                {
                    RoutingEffort::Heavy
                } else {
                    RoutingEffort::Standard
                };
                if routing_effort_rank(structural_effort) > routing_effort_rank(effort) {
                    effort = structural_effort;
                }
            }
            confidence = confidence.max(if effort == RoutingEffort::Heavy {
                0.92
            } else {
                0.86
            });
            signals.push(format!("tool_activity_operation={}", operation.as_str()));
            signals.push(format!("tool_activity_effort={}", effort.as_str()));
        }
    }

    let token_estimate = estimate_tokens(payload, token_estimate_bytes_per_token, output_cap);
    Classification {
        task,
        operation,
        effort,
        confidence,
        vision_input: request_has_images(payload),
        compression_request,
        intent_bytes: evidence.latest.len(),
        scaffold_bytes: evidence.scaffold_bytes,
        estimated_input_tokens: token_estimate.input,
        requested_output_tokens: token_estimate.requested_output,
        reserved_output_tokens: token_estimate.reserved_output,
        estimated_required_tokens: token_estimate.required(),
        signals,
    }
}

fn is_claude_code_compaction(payload: &Value, latest_lowercase: &str) -> bool {
    const TEXT_ONLY: &str = "critical: respond with text only. do not call any tools.";
    const BLOCKS: &str = "your entire response must be plain text: an <analysis> block followed by a <summary> block.";
    const SUMMARY_TASK: &str =
        "your task is to create a detailed summary of the conversation so far";
    let prompt_signature = latest_lowercase.contains(TEXT_ONLY)
        && latest_lowercase.contains(BLOCKS)
        && latest_lowercase.contains(SUMMARY_TASK);
    if prompt_signature {
        return true;
    }

    let system = payload
        .get("system")
        .map(content_text)
        .unwrap_or_default()
        .to_ascii_lowercase();
    system.contains("you are a helpful ai assistant tasked with summarizing conversations.")
        && latest_lowercase.contains(TEXT_ONLY)
        && latest_lowercase.contains(BLOCKS)
}

/// Expose the narrow Claude Code compaction signature to the recipe dispatcher
/// without making the rest of classification part of request resolution.
pub(crate) fn is_claude_code_compression(payload: &Value) -> bool {
    let evidence = intent_evidence(payload);
    is_claude_code_compaction(payload, &evidence.latest.to_ascii_lowercase())
}

/// Recognize Hermes' auxiliary context-compression calls. Detection is used
/// only by a `kind = "hermes"` recipe. Requiring several stable prompt markers
/// avoids redirecting ordinary user requests to summarize or checkpoint work.
pub(crate) fn is_hermes_compression(payload: &Value) -> bool {
    let latest = intent_evidence(payload).latest.to_ascii_lowercase();
    let batch = latest.contains("you are a summarization agent creating a context checkpoint")
        && latest.contains("produce only the structured summary")
        && ((latest.contains("turns to summarize:")
            && latest.contains("use this exact structure:"))
            || (latest.contains("new turns to incorporate:")
                && latest.contains("update the summary using this exact structure")));
    let micro = latest.contains(
        "you are a summarization agent creating a compact record of an ongoing conversation",
    ) && latest.contains("## current running summary")
        && latest.contains("## next exchange to merge")
        && latest.contains("return only the updated summary text");
    batch || micro
}

/// Recognize a Hermes `delegate_task` child at the provider boundary. Hermes
/// keeps the raw child session identity inside its own process; ordinary
/// OpenAI-compatible requests receive a content-derived prompt cache key, not
/// that session ID. The focused child system contract is therefore the stable
/// signal available to a transparent router.
pub(crate) fn is_hermes_delegation(payload: &Value) -> bool {
    let system = payload
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        })
        .filter_map(|message| message.get("content"))
        .map(content_text)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    system.contains("you are a focused subagent working on a specific delegated task")
        && system.contains("your task:")
        && system.contains("complete this task using the tools available to you")
        && system.contains("your response is returned to the parent agent as a summary")
}

/// Recognize Hermes' `vision_analyze` auxiliary provider call. Hermes does not
/// place its internal `task = "vision"` label on the OpenAI wire request, so
/// this detector uses the deliberately small and stable auxiliary shape: one
/// user message containing both text and an image, no system/developer
/// scaffold, no tools, and a non-streaming response. Hermes nominally asks for
/// a 2K output cap, but its Chat Completions compatibility layer deliberately
/// omits output caps for most providers, including custom endpoints. Both the
/// explicit cap and its observed absence are therefore valid. The caller
/// additionally scopes this to an authenticated Hermes recipe.
pub(crate) fn is_hermes_vision_analyze(payload: &Value) -> bool {
    let Some(messages) = payload.get("messages").and_then(Value::as_array) else {
        return false;
    };
    if messages.len() != 1
        || messages[0].get("role").and_then(Value::as_str) != Some("user")
        || payload.get("stream").and_then(Value::as_bool) == Some(true)
        || payload
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
    {
        return false;
    }

    let output_cap = payload
        .get("max_tokens")
        .or_else(|| payload.get("max_completion_tokens"))
        .and_then(Value::as_u64);
    if output_cap.is_some_and(|cap| cap != 2_000) {
        return false;
    }

    let Some(parts) = messages[0].get("content").and_then(Value::as_array) else {
        return false;
    };
    let has_text = parts.iter().any(|part| {
        part.get("type").and_then(Value::as_str) == Some("text")
            && part
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
    });
    let has_image = parts.iter().any(|part| {
        part.get("type").and_then(Value::as_str) == Some("image_url")
            && part
                .pointer("/image_url/url")
                .and_then(Value::as_str)
                .is_some_and(|url| url.starts_with("data:image/"))
    });
    has_text && has_image
}

/// Recognize Claude Code's auto-mode safety-classifier wire shape. This is
/// deliberately recipe-scoped by the caller and requires both the transcript
/// envelope and one of Claude's exact classifier output instructions. Merely
/// discussing classifiers, permissions, or safety must never redirect a turn.
pub(crate) fn is_claude_code_classifier(payload: &Value) -> bool {
    let latest = payload
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last())
        .and_then(|message| message.get("content"))
        .map(content_text)
        .unwrap_or_default();
    let latest = latest.trim_start().to_ascii_lowercase();
    if !latest.starts_with("<transcript>") {
        return false;
    }
    latest.contains(
        "your entire response must begin with <block>. do not output any analysis, reasoning, or commentary before <block>",
    ) || (latest.contains("respond with <severity>n</severity> only")
        && latest.contains("use <thinking> first, then respond with <severity>"))
}

fn classify_operation(
    task: RoutingTask,
    latest: &str,
    signals: &mut Vec<String>,
) -> RoutingOperation {
    let checks: &[(RoutingOperation, &[&str])] = &[
        (
            RoutingOperation::Architect,
            &[
                "architect",
                "architecture",
                "design a system",
                "design the system",
                "new system component",
            ],
        ),
        (
            RoutingOperation::Migrate,
            &["migrate", "migration", "upgrade across", "port the"],
        ),
        (
            RoutingOperation::Implement,
            &[
                "implement",
                "build ",
                "develop ",
                "create a component",
                "add a feature",
                "wire in",
            ],
        ),
        (
            RoutingOperation::Modify,
            &["modify", "change ", "edit ", "patch ", "refactor", "fix "],
        ),
        (
            RoutingOperation::Debug,
            &[
                "debug",
                "diagnose",
                "stack trace",
                "exception",
                "error log",
                "why is",
                "failing",
            ],
        ),
        (
            RoutingOperation::Review,
            &[
                "review",
                "audit",
                "security issue",
                "find bugs",
                "code review",
            ],
        ),
        (
            RoutingOperation::Inspect,
            &[
                "read ",
                "scan ",
                "inspect",
                "explain",
                "what does",
                "find ",
                "locate",
                "summarize this code",
            ],
        ),
    ];
    if matches!(task, RoutingTask::Code | RoutingTask::Data) {
        for (operation, phrases) in checks {
            if phrases.iter().any(|phrase| latest.contains(phrase)) {
                signals.push(format!("operation={}", operation.as_str()));
                return *operation;
            }
        }
    }
    if task == RoutingTask::Data {
        signals.push("operation=analyze".to_owned());
        RoutingOperation::Analyze
    } else if latest.contains("summarize") {
        signals.push("operation=summarize".to_owned());
        RoutingOperation::Summarize
    } else {
        signals.push("operation=converse".to_owned());
        RoutingOperation::Converse
    }
}

fn classify_effort(
    operation: RoutingOperation,
    latest: &str,
    user_context: &str,
    user_turns: usize,
    signals: &mut Vec<String>,
) -> RoutingEffort {
    let mut heavy = 0;
    if user_context.len() >= 30_000 {
        heavy += 2;
        signals.push("large_user_context".to_owned());
    } else if user_context.len() >= 10_000 {
        heavy += 1;
        signals.push("medium_user_context".to_owned());
    }
    if latest.len() >= 4_000 {
        heavy += 1;
        signals.push("large_latest_prompt".to_owned());
    }
    if matches!(
        operation,
        RoutingOperation::Architect | RoutingOperation::Migrate
    ) {
        heavy += 3;
    } else if operation == RoutingOperation::Implement {
        heavy += 2;
    }

    let scope_phrases = [
        "multiple files",
        "across the codebase",
        "entire codebase",
        "new component",
        "new system",
        "end to end",
        "integration test",
        "authentication",
        "authorization",
        "concurrency",
        "distributed",
        "database migration",
        "full implementation",
    ];
    let scope_matches = scope_phrases
        .iter()
        .filter(|phrase| latest.contains(**phrase))
        .count();
    if scope_matches > 0 {
        heavy += scope_matches.min(3);
        signals.push(format!("broad_scope_signals={scope_matches}"));
    }

    if user_turns >= 8 {
        heavy += 1;
        signals.push("long_user_conversation".to_owned());
    }

    if heavy >= 3 {
        signals.push(format!("effort_score={heavy}"));
        RoutingEffort::Heavy
    } else if heavy > 0 {
        signals.push(format!("effort_score={heavy}"));
        RoutingEffort::Standard
    } else if matches!(
        operation,
        RoutingOperation::Inspect | RoutingOperation::Summarize | RoutingOperation::Converse
    ) && latest.len() < 2500
    {
        signals.push("bounded_read_or_chat".to_owned());
        RoutingEffort::Light
    } else {
        RoutingEffort::Standard
    }
}

fn keyword_score(text: &str, keywords: &[&str]) -> usize {
    keywords
        .iter()
        .filter(|keyword| text.contains(**keyword))
        .count()
}

#[derive(Default)]
struct CodeWorkActivity {
    mutation_calls: usize,
    write_calls: usize,
    code_files: usize,
    mutation_bytes: usize,
}

fn recent_code_work_activity(payload: &Value) -> CodeWorkActivity {
    const RECENT_MESSAGE_LIMIT: usize = 32;

    let messages = payload
        .get("messages")
        .or_else(|| payload.get("input"))
        .and_then(Value::as_array);
    let Some(messages) = messages else {
        return CodeWorkActivity::default();
    };

    let latest_user_intent = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            (message.get("role").and_then(Value::as_str) == Some("user"))
                .then(|| message.get("content").map(user_content_text))
                .flatten()
                .filter(|text| !text.trim().is_empty())
                .map(|_| index)
        });
    let recent_start = latest_user_intent.map_or(0, |index| index.saturating_add(1));

    let mut activity = CodeWorkActivity::default();
    let mut code_files = HashSet::new();
    for message in messages[recent_start..]
        .iter()
        .rev()
        .take(RECENT_MESSAGE_LIMIT)
    {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }

        if let Some(blocks) = message.get("content").and_then(Value::as_array) {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                let input = block.get("input").unwrap_or(&Value::Null);
                record_code_mutation(name, input, &mut activity, &mut code_files);
            }
        }

        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                let function = call.get("function").unwrap_or(call);
                let name = function.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = function.get("arguments").unwrap_or(&Value::Null);
                record_code_mutation(name, arguments, &mut activity, &mut code_files);
            }
        }
    }
    activity.code_files = code_files.len();
    activity
}

fn record_code_mutation(
    name: &str,
    input: &Value,
    activity: &mut CodeWorkActivity,
    code_files: &mut HashSet<String>,
) {
    let normalized = name
        .rsplit([':', '.', '/'])
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase();
    let is_write = matches!(
        normalized.as_str(),
        "write" | "write_file" | "create_file" | "notebookedit" | "notebook_edit"
    );
    let is_edit = matches!(
        normalized.as_str(),
        "edit" | "edit_file" | "multiedit" | "multi_edit" | "apply_patch"
    );
    if !is_write && !is_edit {
        return;
    }

    activity.mutation_calls += 1;
    activity.write_calls += usize::from(is_write);
    let parsed_input = input
        .as_str()
        .and_then(|encoded| serde_json::from_str::<Value>(encoded).ok());
    let structured_input = parsed_input.as_ref().unwrap_or(input);
    activity.mutation_bytes += mutation_payload_bytes(structured_input);

    for key in ["file_path", "path", "notebook_path"] {
        if let Some(path) = structured_input.get(key).and_then(Value::as_str)
            && code_file_path(path)
        {
            code_files.insert(path.to_owned());
        }
    }
    if normalized == "apply_patch" {
        for key in ["patch", "input"] {
            let Some(patch) = structured_input.get(key).and_then(Value::as_str) else {
                continue;
            };
            for line in patch.lines() {
                let path = ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                    .iter()
                    .find_map(|prefix| line.strip_prefix(prefix));
                if let Some(path) = path
                    && code_file_path(path)
                {
                    code_files.insert(path.to_owned());
                }
            }
        }
    }
}

fn mutation_payload_bytes(input: &Value) -> usize {
    ["content", "new_string", "patch", "input", "cell_source"]
        .iter()
        .filter_map(|key| input.get(*key))
        .map(text_content_bytes)
        .sum()
}

fn text_content_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Array(parts) => parts.iter().map(text_content_bytes).sum(),
        Value::Object(object) => object
            .get("text")
            .or_else(|| object.get("input_text"))
            .map_or(0, text_content_bytes),
        _ => 0,
    }
}

fn code_file_path(path: &str) -> bool {
    let lowercase = path.to_ascii_lowercase();
    [
        ".c", ".cc", ".cpp", ".cs", ".css", ".dart", ".ex", ".exs", ".go", ".h", ".hpp", ".html",
        ".ipynb", ".java", ".js", ".jsx", ".kt", ".kts", ".lua", ".m", ".mm", ".php", ".py", ".rb",
        ".rs", ".scala", ".sh", ".sql", ".swift", ".ts", ".tsx", ".vue", ".zig",
    ]
    .iter()
    .any(|extension| lowercase.ends_with(extension))
}

fn operation_intensity(operation: RoutingOperation) -> u8 {
    match operation {
        RoutingOperation::Converse | RoutingOperation::Summarize => 0,
        RoutingOperation::Inspect | RoutingOperation::Review | RoutingOperation::Analyze => 1,
        RoutingOperation::Debug | RoutingOperation::Modify => 2,
        RoutingOperation::Implement => 3,
        RoutingOperation::Architect | RoutingOperation::Migrate => 4,
    }
}

struct IntentEvidence {
    latest: String,
    user_context: String,
    user_turns: usize,
    scaffold_bytes: usize,
}

fn intent_evidence(payload: &Value) -> IntentEvidence {
    let parts = user_intent_parts(payload);
    let latest = parts.last().cloned().unwrap_or_default();
    let user_bytes = parts.iter().map(String::len).sum::<usize>();
    let transport_bytes = serde_json::to_vec(payload).map_or(0, |bytes| bytes.len());
    IntentEvidence {
        latest,
        user_context: parts.join("\n"),
        user_turns: parts.len(),
        scaffold_bytes: transport_bytes.saturating_sub(user_bytes),
    }
}

fn user_intent_parts(payload: &Value) -> Vec<String> {
    if let Some(messages) = payload.get("messages").and_then(Value::as_array) {
        return messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
            .filter_map(|message| message.get("content"))
            .map(user_content_text)
            .filter(|text| !text.trim().is_empty())
            .collect();
    }

    match payload.get("input") {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(items)) => {
            let structured = items
                .iter()
                .filter(|item| item.get("role").and_then(Value::as_str) == Some("user"))
                .filter_map(|item| item.get("content"))
                .map(user_content_text)
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>();
            if structured.is_empty() {
                let text = user_content_text(payload.get("input").unwrap_or(&Value::Null));
                (!text.trim().is_empty())
                    .then_some(text)
                    .into_iter()
                    .collect()
            } else {
                structured
            }
        }
        _ => payload
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .into_iter()
            .collect(),
    }
}

fn user_content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| {
                !matches!(
                    part.get("type").and_then(Value::as_str),
                    Some(
                        "tool_result"
                            | "tool_use"
                            | "function_call"
                            | "function_call_output"
                            | "image"
                            | "image_url"
                            | "input_image"
                    )
                )
            })
            .filter_map(|part| {
                if let Some(text) = part.as_str() {
                    Some(text.to_owned())
                } else {
                    part.get("text")
                        .or_else(|| part.get("input_text"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn request_has_images(payload: &Value) -> bool {
    payload
        .get("messages")
        .or_else(|| payload.get("input"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .any(|part| {
            matches!(
                part.get("type").and_then(Value::as_str),
                Some("image_url" | "input_image" | "image")
            )
        })
}

#[cfg(test)]
pub(crate) fn estimated_required_tokens(payload: &Value) -> u64 {
    estimated_required_tokens_with_ratio(payload, None)
}

pub(crate) fn estimated_required_tokens_with_ratio(
    payload: &Value,
    bytes_per_token: Option<f64>,
) -> u64 {
    estimate_tokens(payload, bytes_per_token, None).required()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TokenEstimate {
    input: u64,
    requested_output: u64,
    reserved_output: u64,
}

impl TokenEstimate {
    fn required(self) -> u64 {
        self.input.saturating_add(self.reserved_output)
    }
}

fn estimate_tokens(
    payload: &Value,
    bytes_per_token: Option<f64>,
    output_cap: Option<u64>,
) -> TokenEstimate {
    let input_tokens = serde_json::to_vec(payload)
        .map(|bytes| match bytes_per_token {
            Some(ratio) => (bytes.len() as f64 / ratio).ceil() as u64,
            None => bytes.len() as u64 / 3,
        })
        .unwrap_or(0);
    let requested_output = payload
        .get("max_completion_tokens")
        .or_else(|| payload.get("max_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(1024);
    TokenEstimate {
        input: input_tokens,
        requested_output,
        reserved_output: output_cap
            .map(|limit| requested_output.min(limit))
            .unwrap_or(requested_output),
    }
}

pub(crate) fn cap_requested_output(payload: &mut Value, limit: u64) -> Option<(u64, u64)> {
    for key in ["max_completion_tokens", "max_tokens"] {
        let Some(requested) = payload.get(key).and_then(Value::as_u64) else {
            continue;
        };
        let effective = requested.min(limit);
        if effective < requested {
            payload[key] = Value::from(effective);
        }
        return Some((requested, effective));
    }
    None
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                if let Some(text) = part.as_str() {
                    Some(text.to_owned())
                } else {
                    part.get("text")
                        .or_else(|| part.get("input_text"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn tool_loop_active(payload: &Value) -> bool {
    let Some(last_message) = payload
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last())
    else {
        return false;
    };
    if last_message.get("role").and_then(Value::as_str) == Some("tool") {
        return true;
    }
    last_message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

pub(crate) fn tool_requirement(payload: &Value) -> Option<String> {
    if tool_loop_active(payload) {
        return Some("active_tool_loop".to_owned());
    }
    let choice = payload.get("tool_choice")?;
    if let Some(choice) = choice.as_str() {
        return match choice.to_ascii_lowercase().as_str() {
            "auto" | "none" => None,
            "required" | "any" => Some(format!("tool_choice:{choice}")),
            _ if !choice.is_empty() => Some(format!("tool_choice:{choice}")),
            _ => None,
        };
    }
    let choice = choice.as_object()?;
    let choice_type = choice
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    match choice_type.as_str() {
        "auto" | "none" => None,
        "any" | "tool" | "function" => Some(format!("tool_choice:{choice_type}")),
        "allowed_tools"
            if choice
                .get("mode")
                .and_then(Value::as_str)
                .is_some_and(|mode| mode.eq_ignore_ascii_case("required")) =>
        {
            Some("tool_choice:allowed_tools:required".to_owned())
        }
        _ if choice.contains_key("function") || choice.contains_key("name") => {
            Some("tool_choice:specific".to_owned())
        }
        _ => None,
    }
}

fn conversation_key(route: &str, payload: &Value) -> String {
    let identity = {
        let top_level_system = payload
            .get("system")
            .map(content_text)
            .filter(|text| !text.is_empty());
        let messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        top_level_system
            .into_iter()
            .chain(
                messages
                    .iter()
                    .filter(|message| {
                        matches!(
                            message.get("role").and_then(Value::as_str),
                            Some("system" | "developer" | "user")
                        )
                    })
                    .filter_map(|message| message.get("content"))
                    .map(content_text)
                    .filter(|text| !text.trim().is_empty())
                    .take(2),
            )
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut hasher = Sha256::new();
    hasher.update(
        payload
            .get("user")
            .and_then(Value::as_str)
            .unwrap_or("")
            .as_bytes(),
    );
    hasher.update(b"\0");
    hasher.update(route.as_bytes());
    hasher.update(b"\0");
    hasher.update(identity.as_bytes());
    format!("{:x}", hasher.finalize())
}

use crate::config::wildcard_matches;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_distinguish_clients_sessions_and_conversations() {
        let engine = RoutingEngine::default();
        assert_ne!(
            engine.scoped("a", None).scope,
            engine.scoped("b", None).scope
        );
        assert_ne!(
            engine.scoped("a", Some("one")).scope,
            engine.scoped("a", Some("two")).scope
        );
        let a = serde_json::json!({"user":"same", "messages":[{"role":"user", "content":"first task"}]});
        let b = serde_json::json!({"user":"same", "messages":[{"role":"user", "content":"different task"}]});
        assert_ne!(conversation_key("auto", &a), conversation_key("auto", &b));
    }
    use crate::config::LoadedModelPreference;
    use serde_json::json;

    fn discovered_model(id: &str, loaded: bool) -> DiscoveredModel {
        DiscoveredModel {
            id: id.to_owned(),
            provider: "lab".to_owned(),
            provider_title: "Lab".to_owned(),
            upstream_id: id.trim_start_matches("lab/").to_owned(),
            management_id: None,
            loaded: Some(loaded),
            realm: "local".to_owned(),
            created: 0,
            provider_priority: 1,
            capabilities: serde_json::json!({}),
            provider_metadata: serde_json::json!({}),
        }
    }

    fn claude_code_compaction_payload() -> Value {
        serde_json::json!({
            "system": "You are Claude Code, Anthropic's official CLI.",
            "messages": [{
                "role": "user",
                "content": "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\
                    - Do NOT use Read, Bash, Grep, Glob, Edit, Write, or ANY other tool.\n\
                    - Your entire response must be plain text: an <analysis> block followed by a <summary> block.\n\
                    Your task is to create a detailed summary of the conversation so far, preserving technical decisions."
            }],
            "max_tokens": 32000
        })
    }

    #[test]
    fn distinguishes_light_code_inspection_from_heavy_implementation() {
        let light = serde_json::json!({
            "messages": [{"role": "user", "content": "Read src/config.rs and explain what validate does."}]
        });
        let light = classify(&light, RoutingTask::Auto, RoutingEffort::Auto);
        assert_eq!(light.task, RoutingTask::Code);
        assert_eq!(light.operation, RoutingOperation::Inspect);
        assert_eq!(light.effort, RoutingEffort::Light);

        let heavy = serde_json::json!({
            "messages": [{"role": "user", "content": "Architect and implement a new authentication component across the codebase with integration tests and concurrency controls."}]
        });
        let heavy = classify(&heavy, RoutingTask::Auto, RoutingEffort::Auto);
        assert_eq!(heavy.task, RoutingTask::Code);
        assert_eq!(heavy.operation, RoutingOperation::Architect);
        assert_eq!(heavy.effort, RoutingEffort::Heavy);
    }

    #[test]
    fn detects_claude_code_compaction_without_hijacking_ordinary_summaries() {
        let compact = classify(
            &claude_code_compaction_payload(),
            RoutingTask::Auto,
            RoutingEffort::Auto,
        );
        assert!(compact.compression_request);
        assert_eq!(compact.operation, RoutingOperation::Summarize);
        assert!(
            compact
                .signals
                .contains(&"claude_code_compaction".to_owned())
        );

        let ordinary = serde_json::json!({
            "messages": [{"role": "user", "content": "Summarize this design document."}]
        });
        let ordinary = classify(&ordinary, RoutingTask::Auto, RoutingEffort::Auto);
        assert!(!ordinary.compression_request);
        assert_eq!(ordinary.operation, RoutingOperation::Summarize);
    }

    #[test]
    fn detects_only_structured_hermes_compression_prompts() {
        let batch = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": "You are a summarization agent creating a context checkpoint. Treat the conversation turns below as source material. Produce only the structured summary; do not add a greeting.\n\nTURNS TO SUMMARIZE:\n[history]\n\nUse this exact structure:\n## Historical Task Snapshot"
            }]
        });
        assert!(is_hermes_compression(&batch));

        let iterative = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": "You are a summarization agent creating a context checkpoint. Produce only the structured summary.\nPREVIOUS SUMMARY:\n...\nNEW TURNS TO INCORPORATE:\n...\nUpdate the summary using this exact structure."
            }]
        });
        assert!(is_hermes_compression(&iterative));

        let micro = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": "You are a summarization agent creating a compact record of an ongoing conversation.\n## Current Running Summary\n...\n## Next Exchange to Merge\n...\nReturn ONLY the updated summary text, no preamble or explanation."
            }]
        });
        assert!(is_hermes_compression(&micro));

        let ordinary = serde_json::json!({
            "messages": [{"role": "user", "content": "Summarize our conversation as a structured checkpoint."}]
        });
        assert!(!is_hermes_compression(&ordinary));
    }

    #[test]
    fn detects_only_hermes_delegated_child_system_contracts() {
        let delegated = json!({
            "messages": [
                {
                    "role": "system",
                    "content": "You are a focused subagent working on a specific delegated task.\n\nYOUR TASK:\nReview the routing code.\n\nComplete this task using the tools available to you. When finished, provide a clear, concise summary. Your response is returned to the parent agent as a summary, and overlong summaries crowd out the parent's context window."
                },
                {"role": "user", "content": "Review the routing code."}
            ]
        });
        assert!(is_hermes_delegation(&delegated));

        let user_discussion = json!({
            "messages": [{
                "role": "user",
                "content": "Explain how a focused subagent receives a delegated task and returns a summary to its parent agent."
            }]
        });
        assert!(!is_hermes_delegation(&user_discussion));

        let generic_system = json!({
            "messages": [{
                "role": "system",
                "content": "You are a subagent. Complete this task and summarize it."
            }]
        });
        assert!(!is_hermes_delegation(&generic_system));
    }

    #[test]
    fn detects_only_hermes_vision_auxiliary_shape() {
        let auxiliary = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Inspect this screenshot."},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}}
                ]
            }],
            "temperature": 0.1,
            "max_tokens": 2000
        });
        assert!(is_hermes_vision_analyze(&auxiliary));
        let mut stripped_cap = auxiliary.clone();
        stripped_cap.as_object_mut().unwrap().remove("max_tokens");
        assert!(is_hermes_vision_analyze(&stripped_cap));

        let mut unexpected_cap = auxiliary.clone();
        unexpected_cap["max_tokens"] = json!(8_000);
        assert!(!is_hermes_vision_analyze(&unexpected_cap));

        let main_turn = json!({
            "messages": [
                {"role": "system", "content": "You are Hermes."},
                {"role": "user", "content": [
                    {"type": "text", "text": "Inspect this screenshot."},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}}
                ]}
            ],
            "tools": [{"type": "function", "function": {"name": "read_file"}}],
            "max_tokens": 2000
        });
        assert!(!is_hermes_vision_analyze(&main_turn));

        let arbitrary_image_request = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Inspect this screenshot."},
                    {"type": "image_url", "image_url": {"url": "https://example.test/image.png"}}
                ]
            }],
            "max_tokens": 2000
        });
        assert!(!is_hermes_vision_analyze(&arbitrary_image_request));
    }

    #[test]
    fn recipe_compression_hint_specializes_adaptive_classification() {
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": "opaque harness payload"}],
            "max_tokens": 20_000
        });
        let classification = classify_with_token_estimate(
            &payload,
            RoutingTask::Auto,
            RoutingEffort::Auto,
            Some(4.0),
            Some(12_000),
            true,
        );
        assert!(classification.compression_request);
        assert_eq!(classification.operation, RoutingOperation::Summarize);
        assert_eq!(classification.reserved_output_tokens, 12_000);
        assert!(
            classification
                .signals
                .contains(&"recipe_compression".to_owned())
        );
    }

    #[test]
    fn compression_output_limit_changes_only_detected_compaction() {
        let compact_payload = claude_code_compaction_payload();
        let uncapped = classify_with_token_estimate(
            &compact_payload,
            RoutingTask::Auto,
            RoutingEffort::Auto,
            Some(4.0),
            None,
            false,
        );
        let capped = classify_with_token_estimate(
            &compact_payload,
            RoutingTask::Auto,
            RoutingEffort::Auto,
            Some(4.0),
            Some(16_384),
            false,
        );
        assert_eq!(capped.requested_output_tokens, 32_000);
        assert_eq!(capped.reserved_output_tokens, 16_384);
        assert_eq!(
            uncapped.estimated_required_tokens - capped.estimated_required_tokens,
            32_000 - 16_384
        );

        let ordinary_payload = serde_json::json!({
            "messages": [{"role": "user", "content": "Please answer this question."}],
            "max_tokens": 32_000
        });
        let ordinary = classify_with_token_estimate(
            &ordinary_payload,
            RoutingTask::Auto,
            RoutingEffort::Auto,
            Some(4.0),
            Some(16_384),
            false,
        );
        assert!(!ordinary.compression_request);
        assert_eq!(ordinary.reserved_output_tokens, 32_000);
    }

    #[test]
    fn compression_output_limit_rewrites_both_supported_request_fields() {
        let mut anthropic = serde_json::json!({ "max_tokens": 32_000 });
        assert_eq!(
            cap_requested_output(&mut anthropic, 16_384),
            Some((32_000, 16_384))
        );
        assert_eq!(anthropic["max_tokens"], 16_384);

        let mut openai = serde_json::json!({ "max_completion_tokens": 32_000 });
        assert_eq!(
            cap_requested_output(&mut openai, 16_384),
            Some((32_000, 16_384))
        );
        assert_eq!(openai["max_completion_tokens"], 16_384);
    }

    #[test]
    fn route_token_estimate_override_changes_only_the_input_estimate() {
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": "x".repeat(10_000)}],
            "max_tokens": 32_000
        });
        let encoded_bytes = serde_json::to_vec(&payload).unwrap().len() as u64;
        let conservative = estimated_required_tokens_with_ratio(&payload, None);
        let tuned = estimated_required_tokens_with_ratio(&payload, Some(3.5));

        assert_eq!(conservative, encoded_bytes / 3 + 32_000);
        assert_eq!(tuned, (encoded_bytes as f64 / 3.5).ceil() as u64 + 32_000);
        assert!(tuned < conservative);
    }

    #[test]
    fn explicit_route_task_overrides_ambiguous_language() {
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": "Could you take a quick look at this?"}]
        });
        let classification = classify(&payload, RoutingTask::Data, RoutingEffort::Auto);
        assert_eq!(classification.task, RoutingTask::Data);
        assert_eq!(classification.operation, RoutingOperation::Analyze);
    }

    #[test]
    fn balanced_load_preference_does_not_overcome_a_meaningful_strength_gap() {
        let mut config: Config = toml::from_str(
            r#"
            [routing]
            loaded_model_preference = "balanced"

            [[routing.models]]
            model = "lab/warm"
            [routing.models.strengths]
            chat = 0.80

            [[routing.models]]
            model = "lab/cold"
            [routing.models.strengths]
            chat = 0.85

            [[providers]]
            id = "lab"
            base_url = "http://127.0.0.1:8000/v1"

            [[routes]]
            id = "private/auto"
            strategy = "adaptive"
            allowed_realms = ["local"]
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        let classification = Classification {
            task: RoutingTask::Chat,
            operation: RoutingOperation::Converse,
            effort: RoutingEffort::Light,
            confidence: 1.0,
            vision_input: false,
            compression_request: false,
            intent_bytes: 0,
            scaffold_bytes: 0,
            estimated_input_tokens: 0,
            requested_output_tokens: 0,
            reserved_output_tokens: 0,
            estimated_required_tokens: 0,
            signals: Vec::new(),
        };
        let route = config.routes[0].clone();
        let warm = score_model(
            &config,
            &route,
            &classification,
            &discovered_model("lab/warm", true),
        );
        let cold = score_model(
            &config,
            &route,
            &classification,
            &discovered_model("lab/cold", false),
        );
        assert!(cold.score > warm.score);

        config.routing.loaded_model_preference = LoadedModelPreference::WarmFirst;
        let warm = score_model(
            &config,
            &route,
            &classification,
            &discovered_model("lab/warm", true),
        );
        let cold = score_model(
            &config,
            &route,
            &classification,
            &discovered_model("lab/cold", false),
        );
        assert!(warm.score > cold.score);
    }

    #[test]
    fn additive_vision_role_preferences_image_requests() {
        let config: Config = toml::from_str(
            r#"
            [[routing.models]]
            model = "lab/vision"
            roles = ["inspect", "vision"]
            [routing.models.strengths]
            "code.inspect" = 0.8

            [[routing.models]]
            model = "lab/text"
            roles = ["inspect"]
            [routing.models.strengths]
            "code.inspect" = 0.8

            [[providers]]
            id = "lab"
            base_url = "http://127.0.0.1:8000/v1"

            [[routes]]
            id = "private/auto"
            strategy = "adaptive"
            allowed_realms = ["local"]
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        let payload = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Inspect this UI code and screenshot."},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,x"}}
                ]
            }]
        });
        let classification = classify(&payload, RoutingTask::Auto, RoutingEffort::Auto);
        assert!(classification.vision_input);
        let route = &config.routes[0];
        let vision = score_model(
            &config,
            route,
            &classification,
            &discovered_model("lab/vision", false),
        );
        let text = score_model(
            &config,
            route,
            &classification,
            &discovered_model("lab/text", false),
        );
        assert!(vision.score > text.score);
        assert!(vision.reasons.contains(&"vision_role".to_owned()));
    }

    #[test]
    fn reserved_model_remains_in_catalog_but_is_not_adaptively_eligible() {
        let config: Config = toml::from_str(
            r#"
            [[routing.models]]
            model = "lab/compressor"
            auto_routing = false
            [routing.models.strengths]
            chat = 1.0

            [[routing.models]]
            model = "lab/general"
            [routing.models.strengths]
            chat = 0.7

            [[providers]]
            id = "lab"
            base_url = "http://127.0.0.1:8000/v1"

            [[routes]]
            id = "private/auto"
            strategy = "adaptive"
            allowed_realms = ["local"]
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        let catalog = Catalog {
            models: vec![
                discovered_model("lab/compressor", true),
                discovered_model("lab/general", false),
            ],
            providers: Vec::new(),
            refreshed_at: 0,
        };
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": "Hi!"}]
        });
        let classification = classify(&payload, RoutingTask::Auto, RoutingEffort::Auto);
        let (eligible, rejected, _) = eligible_models(
            &config,
            &catalog,
            &config.routes[0],
            &payload,
            &classification,
            false,
            false,
        );
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(
            eligible
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["lab/general"]
        );
        assert!(rejected.iter().any(|model| {
            model.model == "lab/compressor"
                && model.reasons == ["auto_routing_disabled(profile=lab/compressor)"]
        }));
    }

    #[tokio::test]
    async fn anthropic_adaptive_routes_reject_incompatible_cloud_providers_before_scoring() {
        let config: Config = toml::from_str(
            r#"
            [[routing.models]]
            model = "codex/gpt"
            [routing.models.strengths]
            chat = 1.0

            [[routing.models]]
            model = "ollama/gemma"
            [routing.models.strengths]
            chat = 0.5

            [[providers]]
            id = "codex"
            kind = "openai_codex"
            realm = "cloud"

            [[providers]]
            id = "ollama"
            kind = "ollama_cloud"
            realm = "cloud"
            api_key_env = "OLLAMA_TEST_KEY"

            [[routes]]
            id = "cloud/auto"
            strategy = "adaptive"
            allowed_realms = ["cloud"]
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        let mut codex = discovered_model("codex/gpt", false);
        codex.provider = "codex".to_owned();
        codex.realm = "cloud".to_owned();
        let mut ollama = discovered_model("ollama/gemma", false);
        ollama.provider = "ollama".to_owned();
        ollama.realm = "cloud".to_owned();
        let catalog = Catalog {
            models: vec![codex, ollama],
            providers: Vec::new(),
            refreshed_at: 0,
        };
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": "Hello"}],
            "max_tokens": 1024
        });

        let outcome = RoutingEngine::default()
            .evaluate_for_anthropic_messages(&config, &catalog, &config.routes[0], &payload, false)
            .await
            .unwrap();

        assert_eq!(outcome.selected.id, "ollama/gemma");
        assert!(outcome.decision.rejected.iter().any(|candidate| {
            candidate.model == "codex/gpt"
                && candidate.reasons == ["anthropic_messages_unsupported(provider=codex)"]
        }));
    }

    #[tokio::test]
    async fn detected_compaction_uses_tagged_reserved_model_exclusively() {
        let config: Config = toml::from_str(
            r#"
            [[routing.models]]
            model = "lab/compressor"
            auto_routing = false
            roles = ["fast", "compression"]
            [routing.models.strengths]
            summarize = 0.95

            [[routing.models]]
            model = "lab/general"
            roles = ["balanced"]
            [routing.models.strengths]
            summarize = 1.0

            [[providers]]
            id = "lab"
            base_url = "http://127.0.0.1:8000/v1"

            [[routes]]
            id = "private/auto"
            strategy = "adaptive"
            allowed_realms = ["local"]
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        let catalog = Catalog {
            models: vec![
                discovered_model("lab/compressor", false),
                discovered_model("lab/general", true),
            ],
            providers: Vec::new(),
            refreshed_at: 0,
        };
        let payload = claude_code_compaction_payload();
        let outcome = RoutingEngine::default()
            .evaluate(&config, &catalog, &config.routes[0], &payload, false)
            .await
            .unwrap();

        assert_eq!(outcome.selected.id, "lab/compressor");
        assert!(
            outcome
                .decision
                .selection_reasons
                .contains(&"compression_role".to_owned())
        );
        assert!(
            outcome
                .decision
                .selection_reasons
                .contains(&"reserved_compression_model".to_owned())
        );
        assert!(outcome.decision.rejected.iter().any(|candidate| {
            candidate.model == "lab/general" && candidate.reasons == ["compression_role_required"]
        }));
    }

    #[test]
    fn compaction_without_tagged_models_preserves_existing_routing_pool() {
        let config: Config = toml::from_str(
            r#"
            [[routing.models]]
            model = "lab/general"
            roles = ["balanced"]

            [[providers]]
            id = "lab"
            base_url = "http://127.0.0.1:8000/v1"

            [[routes]]
            id = "private/auto"
            strategy = "adaptive"
            allowed_realms = ["local"]
            "#,
        )
        .unwrap();
        let catalog = Catalog {
            models: vec![discovered_model("lab/general", true)],
            providers: Vec::new(),
            refreshed_at: 0,
        };
        let payload = claude_code_compaction_payload();
        let classification = classify(&payload, RoutingTask::Auto, RoutingEffort::Auto);
        let (eligible, rejected, _) = eligible_models(
            &config,
            &catalog,
            &config.routes[0],
            &payload,
            &classification,
            false,
            false,
        );
        assert_eq!(eligible[0].id, "lab/general");
        assert!(rejected.is_empty());
    }

    #[test]
    fn route_context_contract_excludes_small_and_unknown_models() {
        let config: Config = toml::from_str(
            r#"
            [[providers]]
            id = "lab"
            base_url = "http://127.0.0.1:8000/v1"

            [[routes]]
            id = "private/auto"
            strategy = "adaptive"
            allowed_realms = ["local"]
            context_length = 131072
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        let mut small = discovered_model("lab/small", false);
        small.provider_metadata = serde_json::json!({"max_context_length": 65536});
        let mut large = discovered_model("lab/large", false);
        large.provider_metadata = serde_json::json!({"max_context_length": 262144});
        let unknown = discovered_model("lab/unknown", false);
        let catalog = Catalog {
            models: vec![small, large, unknown],
            providers: Vec::new(),
            refreshed_at: 0,
        };
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": "Hi!"}],
            "max_tokens": 1024
        });
        let classification = classify(&payload, RoutingTask::Auto, RoutingEffort::Auto);

        let (eligible, rejected, _) = eligible_models(
            &config,
            &catalog,
            &config.routes[0],
            &payload,
            &classification,
            false,
            false,
        );

        assert_eq!(
            eligible
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["lab/large"]
        );
        assert!(rejected.iter().any(|candidate| {
            candidate.model == "lab/small"
                && candidate
                    .reasons
                    .iter()
                    .any(|reason| reason.starts_with("context_below_route_contract("))
        }));
        assert!(rejected.iter().any(|candidate| {
            candidate.model == "lab/unknown"
                && candidate
                    .reasons
                    .iter()
                    .any(|reason| reason.starts_with("context_unknown("))
        }));
    }

    #[tokio::test]
    async fn request_larger_than_route_contract_fails_before_selection() {
        let config: Config = toml::from_str(
            r#"
            [[providers]]
            id = "lab"
            base_url = "http://127.0.0.1:8000/v1"

            [[routes]]
            id = "private/auto"
            strategy = "adaptive"
            allowed_realms = ["local"]
            context_length = 1024
            "#,
        )
        .unwrap();
        let mut model = discovered_model("lab/large", false);
        model.provider_metadata = serde_json::json!({"max_context_length": 262144});
        let catalog = Catalog {
            models: vec![model],
            providers: Vec::new(),
            refreshed_at: 0,
        };
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": "Hi!"}],
            "max_tokens": 2048
        });

        let error = RoutingEngine::default()
            .evaluate(&config, &catalog, &config.routes[0], &payload, false)
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::RouteContextExceeded { .. }));
    }

    #[tokio::test]
    async fn context_only_candidate_exhaustion_reports_the_available_ceiling() {
        let config: Config = toml::from_str(
            r#"
            [[providers]]
            id = "lab"
            base_url = "http://127.0.0.1:8000/v1"

            [[routes]]
            id = "private/auto"
            strategy = "adaptive"
            allowed_realms = ["local"]
            "#,
        )
        .unwrap();
        let mut smaller = discovered_model("lab/smaller", false);
        smaller.provider_metadata = serde_json::json!({"max_context_length": 1024});
        let mut larger = discovered_model("lab/larger", false);
        larger.provider_metadata = serde_json::json!({"max_context_length": 2048});
        let catalog = Catalog {
            models: vec![smaller, larger],
            providers: Vec::new(),
            refreshed_at: 0,
        };
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": "Continue the session."}],
            "max_tokens": 4096
        });

        let error = RoutingEngine::default()
            .evaluate(&config, &catalog, &config.routes[0], &payload, false)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ResolveError::RouteContextExceeded {
                required_tokens,
                context_length: 2048,
                ..
            } if required_tokens > 2048
        ));
    }

    #[test]
    fn response_input_text_is_classified() {
        let payload = serde_json::json!({
            "input": "Debug this Python exception and propose a fix."
        });
        let classification = classify(&payload, RoutingTask::Auto, RoutingEffort::Auto);
        assert_eq!(classification.task, RoutingTask::Code);
        assert_eq!(classification.operation, RoutingOperation::Debug);
    }

    #[test]
    fn large_harness_scaffold_does_not_define_user_intent_or_effort() {
        let system = "You are a coding agent. Inspect the Rust repository, functions, classes, \
                      Python stack traces, SQL datasets, and TypeScript code. "
            .repeat(300);
        let tools = (0..33)
            .map(|index| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": format!("developer_tool_{index}"),
                        "description": "Read code, modify files, inspect databases, and debug exceptions.".repeat(25),
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "query": {
                                    "type": "string",
                                    "description": "Code or SQL query to process".repeat(10)
                                }
                            }
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        let request = serde_json::json!({
            "model": "route:private:auto",
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": "howdy"}
            ],
            "tools": tools,
            "stream": true,
            "max_tokens": 65_536,
            "reasoning_effort": "medium"
        });

        let classification = classify(&request, RoutingTask::Auto, RoutingEffort::Auto);
        assert_eq!(classification.task, RoutingTask::Chat);
        assert_eq!(classification.operation, RoutingOperation::Converse);
        assert_eq!(classification.effort, RoutingEffort::Light);
        assert_eq!(classification.intent_bytes, 5);
        assert!(classification.scaffold_bytes > 50_000);
        assert!(classification.estimated_required_tokens > 65_536);
        assert!(
            classification
                .signals
                .contains(&"harness_scaffold_excluded".to_owned())
        );

        let mut constrained = discovered_model("lab/constrained", false);
        constrained.provider_metadata = serde_json::json!({
            "max_context_length": classification.estimated_required_tokens - 1
        });
        assert!(
            capability_rejections(
                &constrained,
                &request,
                classification.estimated_required_tokens,
                None,
            )
            .iter()
            .any(|reason| reason.starts_with("context_too_small("))
        );
    }

    #[test]
    fn heavy_user_request_remains_heavy_with_the_same_harness_scaffold() {
        let request = serde_json::json!({
            "messages": [
                {
                    "role": "system",
                    "content": "General conversation only. Avoid code and data work.".repeat(500)
                },
                {
                    "role": "user",
                    "content": "Architect and implement a new authentication component across the codebase with integration tests and concurrency controls."
                }
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "chat",
                    "description": "Only casual conversation".repeat(1000)
                }
            }]
        });

        let classification = classify(&request, RoutingTask::Auto, RoutingEffort::Auto);
        assert_eq!(classification.task, RoutingTask::Code);
        assert_eq!(classification.operation, RoutingOperation::Architect);
        assert_eq!(classification.effort, RoutingEffort::Heavy);
    }

    #[test]
    fn active_code_writes_raise_weak_tool_loop_intent_to_heavy_implementation() {
        let request = serde_json::json!({
            "system": "You are a coding assistant.",
            "messages": [
                {"role": "user", "content": "Keep going."},
                {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu_write",
                    "name": "Write",
                    "input": {
                        "file_path": "/workspace/service.py",
                        "content": "def generated():\n    return 1\n".repeat(500)
                    }
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_write",
                    "content": "File written."
                }]}
            ]
        });

        let classification = classify(&request, RoutingTask::Auto, RoutingEffort::Auto);
        assert_eq!(classification.task, RoutingTask::Code);
        assert_eq!(classification.operation, RoutingOperation::Implement);
        assert_eq!(classification.effort, RoutingEffort::Heavy);
        assert!(classification.confidence >= 0.92);
        assert!(
            classification
                .signals
                .contains(&"active_code_mutation".to_owned())
        );
        assert!(
            classification
                .signals
                .contains(&"recent_code_files=1".to_owned())
        );
    }

    #[test]
    fn read_only_tools_and_stale_writes_do_not_raise_current_intent() {
        let request = serde_json::json!({
            "messages": [
                {"role": "user", "content": "Implement the old feature."},
                {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu_write",
                    "name": "Write",
                    "input": {
                        "file_path": "/workspace/old.py",
                        "content": "x = 1\n".repeat(3_000)
                    }
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_write",
                    "content": "File written."
                }]},
                {"role": "user", "content": "What does this configuration mean?"},
                {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu_read",
                    "name": "Read",
                    "input": {"file_path": "/workspace/settings.toml"}
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_read",
                    "content": "Configuration contents."
                }]}
            ]
        });

        let classification = classify(&request, RoutingTask::Auto, RoutingEffort::Auto);
        assert_eq!(classification.task, RoutingTask::Chat);
        assert_eq!(classification.operation, RoutingOperation::Converse);
        assert!(
            !classification
                .signals
                .contains(&"active_code_mutation".to_owned())
        );
    }

    #[test]
    fn openai_apply_patch_activity_is_detected_without_inspecting_code_semantics() {
        let patch = format!(
            "*** Begin Patch\n*** Add File: src/generated.py\n+{}\n*** End Patch\n",
            "x = 1\\n".repeat(2_000)
        );
        let request = serde_json::json!({
            "messages": [
                {"role": "user", "content": "Continue."},
                {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_patch",
                        "type": "function",
                        "function": {
                            "name": "functions.apply_patch",
                            "arguments": serde_json::json!({"patch": patch}).to_string()
                        }
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_patch",
                    "content": "Done."
                }
            ]
        });

        let classification = classify(&request, RoutingTask::Auto, RoutingEffort::Auto);
        assert_eq!(classification.task, RoutingTask::Code);
        assert_eq!(classification.operation, RoutingOperation::Implement);
        assert_eq!(classification.effort, RoutingEffort::Heavy);
        assert!(
            classification
                .signals
                .contains(&"active_code_mutation".to_owned())
        );
    }

    #[tokio::test]
    async fn active_code_work_can_upgrade_a_light_tool_loop_pin_once() {
        let config: Config = toml::from_str(
            r#"
            [routing]
            session_ttl_seconds = 3600

            [[routing.models]]
            model = "lab/fast-chat"
            [routing.models.strengths]
            chat = 0.95
            "code.implement" = 0.20
            [routing.models.traits]
            preferred_effort = ["light"]

            [[routing.models]]
            model = "lab/heavy-code"
            [routing.models.strengths]
            chat = 0.20
            "code.implement" = 1.0
            [routing.models.traits]
            preferred_effort = ["heavy"]

            [[providers]]
            id = "lab"
            base_url = "http://127.0.0.1:8000/v1"

            [[routes]]
            id = "private/auto"
            strategy = "adaptive"
            allowed_realms = ["local"]
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        let catalog = Catalog {
            models: vec![
                discovered_model("lab/fast-chat", false),
                discovered_model("lab/heavy-code", false),
            ],
            providers: Vec::new(),
            refreshed_at: 0,
        };
        let engine = RoutingEngine::default();
        let initial = serde_json::json!({
            "system": "Stable session identity.",
            "messages": [{"role": "user", "content": "Keep going."}]
        });
        let initial = engine
            .evaluate(&config, &catalog, &config.routes[0], &initial, false)
            .await
            .unwrap();
        assert_eq!(initial.selected.id, "lab/fast-chat");

        let active_write = serde_json::json!({
            "system": "Stable session identity.",
            "messages": [
                {"role": "user", "content": "Keep going."},
                {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu_write",
                    "name": "Write",
                    "input": {
                        "file_path": "/workspace/service.py",
                        "content": "def generated():\n    return 1\n".repeat(500)
                    }
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_write",
                    "content": "File written."
                }]}
            ]
        });
        let upgraded = engine
            .evaluate(&config, &catalog, &config.routes[0], &active_write, false)
            .await
            .unwrap();
        assert_eq!(upgraded.selected.id, "lab/heavy-code");
        assert!(!upgraded.decision.tool_loop_pinned);
        assert!(
            upgraded
                .decision
                .selection_reasons
                .contains(&"tool_loop_escalation".to_owned())
        );

        let pinned = engine
            .evaluate(&config, &catalog, &config.routes[0], &active_write, false)
            .await
            .unwrap();
        assert_eq!(pinned.selected.id, "lab/heavy-code");
        assert!(pinned.decision.tool_loop_pinned);
        assert_eq!(pinned.decision.selection_reasons, ["tool_loop_pin"]);
    }

    #[test]
    fn recognizes_anthropic_system_and_tool_result_shapes() {
        let request = serde_json::json!({
            "system": [{"type": "text", "text": "You are working in a Rust codebase."}],
            "messages": [
                {"role": "user", "content": "Inspect src/app.rs."},
                {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "read_file",
                    "input": {"path": "src/app.rs"}
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": "file contents"
                }]}
            ]
        });
        let classification = classify(&request, RoutingTask::Auto, RoutingEffort::Auto);
        assert_eq!(classification.task, RoutingTask::Code);
        assert_eq!(classification.operation, RoutingOperation::Inspect);
        assert_eq!(classification.intent_bytes, "Inspect src/app.rs.".len());
        assert!(tool_loop_active(&request));
    }

    #[test]
    fn provider_reported_capabilities_reject_incompatible_requests() {
        let model = DiscoveredModel {
            id: "local/text-only".into(),
            provider: "local".into(),
            provider_title: "Local".into(),
            upstream_id: "text-only".into(),
            management_id: None,
            loaded: Some(true),
            realm: "local".into(),
            created: 0,
            provider_priority: 1,
            capabilities: serde_json::json!({
                "vision": {"support": "unsupported"},
                "tools": {"support": "unsupported"},
            }),
            provider_metadata: serde_json::json!({"max_context_length": 8192}),
        };
        let image = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,x"}}]
            }]
        });
        assert!(
            capability_rejections(&model, &image, estimated_required_tokens(&image), None)
                .iter()
                .any(|reason| reason == "vision_unsupported")
        );

        let tool_result = serde_json::json!({
            "messages": [
                {"role": "user", "content": "inspect"},
                {"role": "tool", "tool_call_id": "call_1", "content": "result"}
            ]
        });
        assert!(
            capability_rejections(
                &model,
                &tool_result,
                estimated_required_tokens(&tool_result),
                None,
            )
            .iter()
            .any(|reason| reason == "tools_unsupported(required_by=active_tool_loop)")
        );
    }

    #[test]
    fn automatic_tool_choices_do_not_become_hard_requirements() {
        for choice in [
            serde_json::json!("auto"),
            serde_json::json!("none"),
            serde_json::json!({"type": "auto"}),
            serde_json::json!({"type": "none"}),
            serde_json::json!({"type": "allowed_tools", "mode": "auto"}),
        ] {
            let request = serde_json::json!({
                "messages": [{"role": "user", "content": "Hi!"}],
                "tools": [{"type": "function", "function": {"name": "search"}}],
                "tool_choice": choice,
            });
            assert_eq!(tool_requirement(&request), None, "choice={choice}");
        }
    }

    #[test]
    fn forced_tool_choices_explain_the_requirement_source() {
        let cases = [
            (
                serde_json::json!("required"),
                "tool_choice:required".to_owned(),
            ),
            (serde_json::json!("any"), "tool_choice:any".to_owned()),
            (
                serde_json::json!({"type": "any"}),
                "tool_choice:any".to_owned(),
            ),
            (
                serde_json::json!({"type": "tool", "name": "search"}),
                "tool_choice:tool".to_owned(),
            ),
            (
                serde_json::json!({"type": "function", "function": {"name": "search"}}),
                "tool_choice:function".to_owned(),
            ),
            (
                serde_json::json!({"type": "allowed_tools", "mode": "required"}),
                "tool_choice:allowed_tools:required".to_owned(),
            ),
        ];
        for (choice, expected) in cases {
            let request = serde_json::json!({
                "messages": [{"role": "user", "content": "Use a tool."}],
                "tool_choice": choice,
            });
            assert_eq!(
                tool_requirement(&request),
                Some(expected),
                "choice={choice}"
            );
        }
    }
}
