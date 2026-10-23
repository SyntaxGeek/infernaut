use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::debug;

use crate::{
    auth::{AuthManager, RuntimeCredential},
    config::{
        AudioService, Config, ModelOrder, ProviderConfig, ProviderKind, RecipeConfig, RecipeKind,
        RecipeReasoningEffort, RecipeSlotConfig,
    },
};

pub(crate) const CODEX_CLIENT_VERSION: &str = "1.0.0";
pub(crate) const CLAUDE_CODE_1M_CONTEXT_MIN_TOKENS: u64 = 1_000_000;

pub(crate) fn supports_claude_code_1m(context_length: Option<u64>) -> bool {
    context_length.is_some_and(|tokens| tokens >= CLAUDE_CODE_1M_CONTEXT_MIN_TOKENS)
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Catalog {
    pub models: Vec<DiscoveredModel>,
    pub providers: Vec<ProviderStatus>,
    pub refreshed_at: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RouteContextPool {
    pub total: usize,
    pub eligible: usize,
    pub below_contract: usize,
    pub unknown: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub provider: String,
    pub provider_title: String,
    pub upstream_id: String,
    pub management_id: Option<String>,
    pub loaded: Option<bool>,
    pub realm: String,
    pub created: u64,
    pub provider_priority: u32,
    pub capabilities: Value,
    pub provider_metadata: Value,
}

impl DiscoveredModel {
    fn provider_model_id(&self) -> &str {
        self.id
            .split_once('/')
            .filter(|(provider, _)| *provider == self.provider)
            .map(|(_, model_id)| model_id)
            .unwrap_or(&self.upstream_id)
    }

    pub(crate) fn context_length(&self) -> Option<u64> {
        context_length_from_value(&self.provider_metadata)
    }

    fn max_output_tokens(&self) -> Option<u64> {
        value_for_keys(
            &self.provider_metadata,
            &[
                "max_output_tokens",
                "max_completion_tokens",
                "maximum_output_tokens",
            ],
        )
    }

    pub(crate) fn supports_audio_service(&self, service: AudioService) -> bool {
        supported_capability(&self.capabilities, service.as_str())
    }

    pub(crate) fn is_audio_service(&self) -> bool {
        [AudioService::Transcription, AudioService::Speech]
            .into_iter()
            .any(|service| self.supports_audio_service(service))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderStatus {
    pub id: String,
    pub title: String,
    pub realm: String,
    pub available: bool,
    pub model_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelList {
    pub object: &'static str,
    pub data: Vec<ModelObject>,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelObject {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
    #[serde(rename = "type")]
    pub model_type: &'static str,
    pub display_name: String,
    pub created_at: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    pub capabilities: Value,
    #[serde(rename = "x-infernaut")]
    pub metadata: Value,
}

#[derive(Debug, Deserialize)]
struct UpstreamModelList {
    #[serde(default)]
    data: Vec<UpstreamModel>,
    #[serde(default)]
    models: Vec<UpstreamModel>,
}

#[derive(Debug, Deserialize)]
struct UpstreamModel {
    id: String,
    #[serde(default)]
    created: u64,
    #[serde(default)]
    owned_by: String,
    #[serde(flatten)]
    metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct CodexModelList {
    #[serde(default)]
    models: Vec<CodexModel>,
}

#[derive(Debug, Deserialize)]
struct CodexModel {
    slug: String,
    #[serde(default)]
    visibility: String,
    #[serde(default)]
    priority: u64,
    #[serde(flatten)]
    metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct XaiLanguageModelList {
    #[serde(default)]
    models: Vec<XaiLanguageModel>,
}

#[derive(Debug, Deserialize)]
struct XaiLanguageModel {
    id: String,
    #[serde(default)]
    created: u64,
    #[serde(default)]
    owned_by: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    fingerprint: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
    #[serde(flatten)]
    metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct LmStudioModelList {
    #[serde(default)]
    models: Vec<LmStudioModel>,
}

#[derive(Debug, Deserialize)]
struct LmStudioModel {
    #[serde(rename = "type")]
    model_type: String,
    key: String,
    #[serde(default)]
    display_name: String,
    architecture: Option<String>,
    #[serde(default)]
    quantization: Value,
    #[serde(default)]
    size_bytes: u64,
    params_string: Option<String>,
    #[serde(default)]
    loaded_instances: Vec<LmStudioLoadedInstance>,
    #[serde(default)]
    max_context_length: u64,
    #[serde(default)]
    format: String,
    #[serde(default)]
    capabilities: LmStudioCapabilities,
}

#[derive(Debug, Deserialize)]
struct LmStudioLoadedInstance {
    id: String,
    #[serde(default)]
    config: LmStudioLoadedInstanceConfig,
}

#[derive(Debug, Default, Deserialize)]
struct LmStudioLoadedInstanceConfig {
    context_length: Option<u64>,
    parallel: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct LmStudioCapabilities {
    #[serde(default)]
    vision: bool,
    #[serde(default)]
    trained_for_tool_use: bool,
    #[serde(default)]
    reasoning: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct LlamaCppModelList {
    #[serde(default)]
    data: Vec<LlamaCppModel>,
}

#[derive(Debug, Deserialize)]
struct LlamaCppModel {
    id: String,
    #[serde(default)]
    created: u64,
    #[serde(default)]
    status: LlamaCppStatus,
    #[serde(default)]
    architecture: Value,
    #[serde(default)]
    meta: Value,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct LlamaCppStatus {
    #[serde(default)]
    value: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    failed: bool,
    exit_code: Option<i64>,
}

const CONTEXT_LENGTH_KEYS: &[&str] = &[
    "max_context_length",
    "max_model_len",
    "max_input_tokens",
    "context_length",
    "context_window",
    "maximum_context_length",
    "max_position_embeddings",
    "max_sequence_length",
    "max_seq_len",
    "n_ctx",
    "n_ctx_train",
];

fn value_for_keys(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| value_for_key(value, key))
}

fn value_for_key(value: &Value, key: &str) -> Option<u64> {
    match value {
        Value::Object(object) => object
            .iter()
            .find_map(|(candidate, value)| {
                candidate.eq_ignore_ascii_case(key).then(|| {
                    value
                        .as_u64()
                        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                })
            })
            .flatten()
            .or_else(|| object.values().find_map(|value| value_for_key(value, key))),
        Value::Array(values) => values.iter().find_map(|value| value_for_key(value, key)),
        _ => None,
    }
}

fn context_length_from_value(value: &Value) -> Option<u64> {
    value_for_keys(value, CONTEXT_LENGTH_KEYS).filter(|value| *value > 0)
}

fn normalize_context_metadata(model: &mut DiscoveredModel) {
    if model
        .provider_metadata
        .get("max_context_length")
        .and_then(Value::as_u64)
        .is_some_and(|value| value > 0)
    {
        return;
    }
    if let Some(context_length) = context_length_from_value(&model.provider_metadata) {
        model.provider_metadata["max_context_length"] = json!(context_length);
    }
}

fn apply_model_context_override(model: &mut DiscoveredModel, config: &Config) {
    if model
        .provider_metadata
        .get("context_length_override")
        .is_some()
    {
        if let Some(detected) = model
            .provider_metadata
            .get("detected_context_length")
            .and_then(Value::as_u64)
        {
            model.provider_metadata["max_context_length"] = json!(detected);
        } else if let Some(metadata) = model.provider_metadata.as_object_mut() {
            metadata.remove("max_context_length");
        }
        if let Some(metadata) = model.provider_metadata.as_object_mut() {
            metadata.remove("context_length_override");
            metadata.remove("context_length_source");
            metadata.remove("detected_context_length");
        }
    }

    let Some(context_length) = config
        .routing
        .models
        .iter()
        .find(|profile| !profile.disabled && wildcard_matches(&profile.model, &model.id))
        .and_then(|profile| profile.context_length)
        .filter(|value| *value > 0)
    else {
        return;
    };

    if let Some(detected) = model.context_length() {
        model.provider_metadata["detected_context_length"] = json!(detected);
    }
    model.provider_metadata["max_context_length"] = json!(context_length);
    model.provider_metadata["context_length_override"] = json!(context_length);
    model.provider_metadata["context_length_source"] = json!("operator_override");
}

fn model_display_name(model: &DiscoveredModel) -> String {
    model
        .provider_metadata
        .get("display_name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{} — {}", model.provider_title, model.provider_model_id()))
}

fn supported_capability(capabilities: &Value, name: &str) -> bool {
    capabilities
        .get(name)
        .and_then(|capability| capability.get("support"))
        .and_then(Value::as_str)
        == Some("native")
}

fn reasoning_efforts(capabilities: &Value) -> HashSet<String> {
    let reasoning = capabilities.get("reasoning").unwrap_or(&Value::Null);
    let options = reasoning
        .get("allowed_options")
        .or_else(|| reasoning.pointer("/options/allowed_options"))
        .or_else(|| reasoning.get("options"))
        .and_then(Value::as_array);
    options
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|option| match option.to_ascii_lowercase().as_str() {
            "off" | "none" => Some("none"),
            "minimal" => Some("minimal"),
            "low" => Some("low"),
            "medium" => Some("medium"),
            "high" => Some("high"),
            "xhigh" => Some("xhigh"),
            "max" => Some("max"),
            "ultra" => Some("ultra"),
            _ => None,
        })
        .map(str::to_owned)
        .collect()
}

fn anthropic_capabilities(capabilities: &Value, reasoning_policy: RecipeReasoningEffort) -> Value {
    let vision = supported_capability(capabilities, "vision");
    let structured_outputs = supported_capability(capabilities, "structured_output");
    let thinking = supported_capability(capabilities, "reasoning");
    let mut efforts = reasoning_efforts(capabilities);
    if let Some(effort) = reasoning_policy.request_value() {
        efforts.insert(effort.to_owned());
    }
    let supports_effort = !efforts.is_empty();
    json!({
        "batch": { "supported": false },
        "citations": { "supported": false },
        "code_execution": { "supported": false },
        "context_management": {
            "supported": false,
            "clear_thinking_20251015": { "supported": false },
            "clear_tool_uses_20250919": { "supported": false },
            "compact_20260112": { "supported": false }
        },
        "effort": {
            "supported": supports_effort,
            "none": { "supported": efforts.contains("none") },
            "minimal": { "supported": efforts.contains("minimal") },
            "low": { "supported": efforts.contains("low") },
            "medium": { "supported": efforts.contains("medium") },
            "high": { "supported": efforts.contains("high") },
            "max": { "supported": efforts.contains("max") },
            "xhigh": { "supported": efforts.contains("xhigh") },
            "ultra": { "supported": efforts.contains("ultra") }
        },
        "image_input": { "supported": vision },
        "pdf_input": { "supported": false },
        "structured_outputs": { "supported": structured_outputs },
        "thinking": {
            "supported": thinking,
            "types": {
                "adaptive": { "supported": false },
                "enabled": { "supported": thinking }
            }
        }
    })
}

fn anthropic_route_capabilities(reasoning_policy: RecipeReasoningEffort) -> Value {
    // Routes can select models with different capabilities. Advertising a feature
    // here would cause clients to rely on it before Infernaut sees the request.
    anthropic_capabilities(&json!({}), reasoning_policy)
}

use crate::config::wildcard_matches;

impl Catalog {
    pub async fn discover(config: Arc<Config>, client: reqwest::Client, auth: AuthManager) -> Self {
        let futures = config
            .providers
            .iter()
            .filter(|provider| config.provider(&provider.id).is_some())
            .cloned()
            .map(|provider| discover_provider(provider, client.clone(), auth.clone()));

        let mut models = Vec::new();
        let mut providers = Vec::new();
        for (status, mut discovered) in join_all(futures).await {
            let anthropic_messages = config
                .provider(&status.id)
                .is_some_and(ProviderConfig::supports_anthropic_messages);
            for model in &mut discovered {
                normalize_context_metadata(model);
                apply_model_context_override(model, &config);
                model.capabilities["anthropic_messages"] = json!({
                    "support": if anthropic_messages { "native" } else { "unsupported" },
                    "confidence": "provider_declared",
                });
            }
            providers.push(status);
            models.append(&mut discovered);
        }

        models.sort_by(|a, b| {
            a.provider_priority
                .cmp(&b.provider_priority)
                .then_with(|| b.loaded.unwrap_or(false).cmp(&a.loaded.unwrap_or(false)))
                .then_with(|| a.id.cmp(&b.id))
        });
        providers.sort_by(|a, b| a.id.cmp(&b.id));

        Self {
            models,
            providers,
            refreshed_at: unix_time(),
        }
    }

    pub(crate) fn apply_context_overrides(&mut self, config: &Config) {
        for model in &mut self.models {
            apply_model_context_override(model, config);
        }
    }

    pub fn openai_model_list(&self, config: &Config) -> ModelList {
        let mut published = self
            .models
            .iter()
            .filter(|model| !model.is_audio_service())
            .map(|model| {
                let namespace = config
                    .provider(&model.provider)
                    .map(|provider| provider.display_namespace().to_owned())
                    .unwrap_or_else(|| model.provider.clone());
                let provider_model_id = model.provider_model_id();
                let context_length = model.context_length();
                (
                    namespace.to_lowercase(),
                    provider_model_id.to_lowercase(),
                    ModelObject {
                        id: config.published_model_id(&model.provider, provider_model_id),
                        object: "model",
                        created: model.created,
                        owned_by: model.provider.clone(),
                        model_type: "model",
                        display_name: model_display_name(model),
                        created_at: "1970-01-01T00:00:00Z",
                        max_input_tokens: context_length,
                        max_tokens: model.max_output_tokens(),
                        capabilities: anthropic_capabilities(
                            &model.capabilities,
                            RecipeReasoningEffort::Inherit,
                        ),
                        metadata: json!({
                            "kind": "physical",
                            "canonical_id": model.id,
                            "provider": model.provider,
                            "provider_alias": namespace,
                            "provider_title": model.provider_title,
                            "provider_model_id": provider_model_id,
                            "load_id": model.management_id,
                            "inference_id": model.upstream_id,
                            "upstream_id": model.upstream_id,
                            "realm": model.realm,
                            "capabilities": model.capabilities,
                            "provider_metadata": model.provider_metadata,
                        }),
                    },
                )
            })
            .collect::<Vec<_>>();

        published.extend(config.routes.iter().filter(|route| {
            !self.route_is_audio(config, route, AudioService::Transcription)
                && !self.route_is_audio(config, route, AudioService::Speech)
        }).map(|route| {
            let context_length = self.route_context_length(config, route);
            (
                "route".to_owned(),
                route.id.to_lowercase(),
                ModelObject {
                    id: config.server.model_id_format.publish(&route.qualified_id()),
                    object: "model",
                    created: self.refreshed_at,
                    owned_by: "infernaut".to_owned(),
                    model_type: "model",
                    display_name: if route.title.is_empty() {
                        route.id.clone()
                    } else {
                        route.title.clone()
                    },
                    created_at: "1970-01-01T00:00:00Z",
                    max_input_tokens: context_length,
                    max_tokens: None,
                    capabilities: anthropic_route_capabilities(RecipeReasoningEffort::Inherit),
                    metadata: json!({
                        "kind": "route",
                        "canonical_id": route.qualified_id(),
                        "title": if route.title.is_empty() { &route.id } else { &route.title },
                        "description": route.description,
                        "allowed_realms": route.allowed_realms,
                        "strategy": route.strategy,
                        "task": route.task,
                        "effort": route.effort,
                        "context_length": context_length,
                        "context_contract_configured": route.context_length.is_some(),
                        "fail_closed": route.on_no_eligible_model == crate::config::NoEligibleModel::Deny,
                    }),
                },
            )
        }));

        published.extend(
            config
                .aliases
                .iter()
                .filter(|alias| !alias.disabled && !alias.target.starts_with("recipe/"))
                .map(|alias| {
                    (
                        "alias".to_owned(),
                        alias.id.to_lowercase(),
                        self.alias_model_object(config, alias),
                    )
                }),
        );

        published.sort_by(|left, right| {
            let ordering = match config.server.model_order {
                ModelOrder::ProviderThenModel => {
                    left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1))
                }
                ModelOrder::ModelThenProvider => {
                    left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0))
                }
            };
            ordering.then_with(|| left.2.id.cmp(&right.2.id))
        });
        let data = published
            .into_iter()
            .map(|(_, _, model)| model)
            .collect::<Vec<_>>();
        let first_id = data.first().map(|model| model.id.clone());
        let last_id = data.last().map(|model| model.id.clone());

        ModelList {
            object: "list",
            data,
            first_id,
            last_id,
            has_more: false,
        }
    }

    pub fn model_object(&self, config: &Config, requested: &str) -> Option<ModelObject> {
        self.openai_model_list(config)
            .data
            .into_iter()
            .find(|model| model.id == requested)
    }

    fn alias_model_object(
        &self,
        config: &Config,
        alias: &crate::config::ModelAliasConfig,
    ) -> ModelObject {
        let context_length = self.alias_context_length(config, alias);
        let (effective_target, reasoning_effort, max_tokens) = config
            .recipe_slot_target(&alias.target)
            .map(|(_recipe, slot)| {
                (
                    slot.target.as_str(),
                    slot.reasoning_effort,
                    slot.max_output_tokens,
                )
            })
            .unwrap_or((alias.target.as_str(), RecipeReasoningEffort::Inherit, None));
        let capabilities = if let Some(route_id) = effective_target.strip_prefix("route/") {
            config
                .route(route_id)
                .map(|_| anthropic_route_capabilities(reasoning_effort))
                .unwrap_or_else(|| json!({}))
        } else {
            self.models
                .iter()
                .find(|model| model.id == effective_target && !model.is_audio_service())
                .map(|model| anthropic_capabilities(&model.capabilities, reasoning_effort))
                .unwrap_or_else(|| json!({}))
        };
        ModelObject {
            id: config.server.model_id_format.publish(&alias.qualified_id()),
            object: "model",
            created: self.refreshed_at,
            owned_by: "infernaut".to_owned(),
            model_type: "model",
            display_name: if alias.title.is_empty() {
                alias.id.clone()
            } else {
                alias.title.clone()
            },
            created_at: "1970-01-01T00:00:00Z",
            max_input_tokens: context_length,
            max_tokens,
            capabilities,
            metadata: json!({
                "kind": "alias",
                "canonical_id": alias.qualified_id(),
                "title": if alias.title.is_empty() { &alias.id } else { &alias.title },
                "target": alias.target,
                "context_length": context_length,
                "context_contract_configured": alias.context_length.is_some(),
                "target_available": self.alias_target_available(config, alias),
                "deterministic": true,
                "fail_closed": true,
            }),
        }
    }

    fn recipe_alias_models(&self, config: &Config, recipe: &RecipeConfig) -> Vec<ModelObject> {
        config
            .aliases
            .iter()
            .filter(|alias| {
                !alias.disabled
                    && config
                        .recipe_slot_target(&alias.target)
                        .is_some_and(|(target_recipe, _slot)| target_recipe.id == recipe.id)
            })
            .map(|alias| self.alias_model_object(config, alias))
            .collect()
    }

    /// Publish only operator-selectable virtual slots exposed by a key-bound
    /// recipe. Internal classifier and compression slots are selected by the
    /// dispatcher and never appear in a harness model picker.
    pub fn recipe_model_list(&self, config: &Config, recipe: &RecipeConfig) -> ModelList {
        let mut list = self.recipe_model_list_from_slots(config, recipe, recipe.selectable_slots());
        list.data.extend(self.recipe_alias_models(config, recipe));
        list.data.sort_by(|left, right| left.id.cmp(&right.id));
        list.first_id = list.data.first().map(|model| model.id.clone());
        list.last_id = list.data.last().map(|model| model.id.clone());
        list
    }

    /// Publish only operator-selectable slots from an additive recipe. Internal
    /// classifier and compression slots remain reachable through dispatch but
    /// do not clutter a harness model picker.
    pub fn selectable_recipe_model_list(
        &self,
        config: &Config,
        recipe: &RecipeConfig,
    ) -> ModelList {
        self.recipe_model_list(config, recipe)
    }

    fn recipe_model_list_from_slots<'a>(
        &self,
        config: &Config,
        recipe: &RecipeConfig,
        slots: impl Iterator<Item = &'a RecipeSlotConfig>,
    ) -> ModelList {
        let mut data = slots
            .map(|slot| {
                let canonical = recipe.qualified_slot_id(slot);
                let (context_length, max_tokens, capabilities) = if let Some(route_id) =
                    slot.target.strip_prefix("route/")
                {
                    config
                        .route(route_id)
                        .map(|route| {
                            (
                                self.route_context_length(config, route),
                                None,
                                anthropic_route_capabilities(slot.reasoning_effort),
                            )
                        })
                        .unwrap_or((None, None, json!({})))
                } else {
                    self.models
                        .iter()
                        .find(|model| model.id == slot.target)
                        .map(|model| {
                            (
                                model.context_length(),
                                model.max_output_tokens(),
                                anthropic_capabilities(&model.capabilities, slot.reasoning_effort),
                            )
                        })
                        .unwrap_or((None, None, json!({})))
                };
                let mut published_id = config.server.model_id_format.publish(&canonical);
                let claude_extended_context = recipe.kind == RecipeKind::ClaudeCode
                    && supports_claude_code_1m(context_length);
                if claude_extended_context {
                    published_id.push_str("[1m]");
                }
                ModelObject {
                    id: published_id,
                    object: "model",
                    created: self.refreshed_at,
                    owned_by: "infernaut".to_owned(),
                    model_type: "model",
                    display_name: slot.display_name().to_owned(),
                    created_at: "1970-01-01T00:00:00Z",
                    max_input_tokens: context_length,
                    max_tokens,
                    capabilities,
                    metadata: json!({
                        "kind": "recipe_slot",
                        "canonical_id": canonical,
                        "recipe": recipe.id,
                        "recipe_kind": recipe.kind,
                        "slot": slot.id,
                        "trigger": slot.trigger,
                        "target": slot.target,
                        "allowed_realms": recipe.allowed_realms,
                        "context_length": context_length,
                        "claude_extended_context": claude_extended_context,
                        "reasoning_effort_policy": slot.reasoning_effort,
                    }),
                }
            })
            .collect::<Vec<_>>();
        data.sort_by(|left, right| left.id.cmp(&right.id));
        ModelList {
            object: "list",
            first_id: data.first().map(|model| model.id.clone()),
            last_id: data.last().map(|model| model.id.clone()),
            data,
            has_more: false,
        }
    }

    /// Add session-selectable recipe models to the ordinary combined catalog.
    /// This is intentionally different from an exclusive key-bound recipe,
    /// which receives only its own slots.
    pub fn catalog_with_selectable_recipes(
        &self,
        config: &Config,
        recipes: &[&RecipeConfig],
    ) -> ModelList {
        let mut list = self.openai_model_list(config);
        for recipe in recipes {
            list.data
                .extend(self.selectable_recipe_model_list(config, recipe).data);
        }
        list.data.sort_by(|left, right| left.id.cmp(&right.id));
        list.first_id = list.data.first().map(|model| model.id.clone());
        list.last_id = list.data.last().map(|model| model.id.clone());
        list
    }

    pub fn recipe_model_object(
        &self,
        config: &Config,
        recipe: &RecipeConfig,
        requested: &str,
    ) -> Option<ModelObject> {
        self.recipe_model_list(config, recipe)
            .data
            .into_iter()
            .find(|model| model.id == requested)
    }

    pub(crate) fn target_context_length(&self, config: &Config, target: &str) -> Option<u64> {
        if let Some(alias_id) = target.strip_prefix("alias/") {
            config
                .model_alias(alias_id)
                .and_then(|alias| self.alias_context_length(config, alias))
        } else if let Some((_recipe, slot)) = config.recipe_slot_target(target) {
            self.target_context_length(config, &slot.target)
        } else if let Some(route_id) = target.strip_prefix("route/") {
            config
                .route(route_id)
                .and_then(|route| self.route_context_length(config, route))
        } else {
            self.models
                .iter()
                .find(|model| model.id == target)
                .and_then(DiscoveredModel::context_length)
        }
    }

    pub(crate) fn target_available(&self, config: &Config, target: &str) -> bool {
        if let Some(alias_id) = target.strip_prefix("alias/") {
            config
                .model_alias(alias_id)
                .is_some_and(|alias| self.resolve_alias(config, target, alias).is_ok())
        } else if let Some((_recipe, slot)) = config.recipe_slot_target(target) {
            self.target_available(config, &slot.target)
        } else if let Some(route_id) = target.strip_prefix("route/") {
            config.route(route_id).is_some_and(|route| {
                if route.strategy == crate::config::RouteStrategy::FirstAvailable {
                    self.resolve_route(config, target, route).is_ok()
                } else {
                    self.route_models(config, route).next().is_some()
                }
            })
        } else {
            self.models.iter().any(|model| model.id == target)
        }
    }

    pub(crate) fn alias_context_length(
        &self,
        config: &Config,
        alias: &crate::config::ModelAliasConfig,
    ) -> Option<u64> {
        alias
            .context_length
            .or_else(|| self.target_context_length(config, &alias.target))
    }

    pub(crate) fn alias_target_available(
        &self,
        config: &Config,
        alias: &crate::config::ModelAliasConfig,
    ) -> bool {
        self.target_available(config, &alias.target)
            && alias.context_length.is_none_or(|contract| {
                self.target_context_length(config, &alias.target)
                    .is_some_and(|limit| limit >= contract)
            })
    }

    fn route_context_length(
        &self,
        config: &Config,
        route: &crate::config::RouteConfig,
    ) -> Option<u64> {
        if let Some(context_length) = route.context_length {
            return Some(context_length);
        }

        if route.strategy == crate::config::RouteStrategy::FirstAvailable {
            return self
                .resolve_route(config, &route.qualified_id(), route)
                .ok()
                .and_then(|resolved| {
                    self.models
                        .iter()
                        .find(|model| model.id == resolved.qualified_id)
                        .and_then(DiscoveredModel::context_length)
                });
        }

        let models = self.route_models(config, route).collect::<Vec<_>>();
        if models.is_empty() || models.iter().any(|model| model.context_length().is_none()) {
            return None;
        }
        models
            .into_iter()
            .filter_map(DiscoveredModel::context_length)
            .min()
    }

    fn route_models<'a>(
        &'a self,
        config: &'a Config,
        route: &'a crate::config::RouteConfig,
    ) -> impl Iterator<Item = &'a DiscoveredModel> + 'a {
        let route_service = route
            .candidates
            .iter()
            .find_map(|candidate| config.static_audio_service(candidate));
        self.models.iter().filter(move |model| {
            route_service.map_or(!model.is_audio_service(), |service| {
                model.supports_audio_service(service)
            }) && route_model_base_allowed(config, route, model)
                && model_meets_context_contract(model, route.context_length)
        })
    }

    pub(crate) fn route_context_pool(
        &self,
        config: &Config,
        route: &crate::config::RouteConfig,
    ) -> RouteContextPool {
        let mut pool = RouteContextPool::default();
        for model in self
            .models
            .iter()
            .filter(|model| route_model_base_allowed(config, route, model))
        {
            pool.total += 1;
            match (route.context_length, model.context_length()) {
                (None, _) => pool.eligible += 1,
                (Some(contract), Some(limit)) if limit >= contract => pool.eligible += 1,
                (Some(_), Some(_)) => pool.below_contract += 1,
                (Some(_), None) => pool.unknown += 1,
            }
        }
        pool
    }

    pub fn resolve(&self, config: &Config, requested: &str) -> Result<ResolvedModel, ResolveError> {
        let stable_match = self.models.iter().find(|model| {
            model.id == requested
                || config.published_model_id(&model.provider, model.provider_model_id())
                    == requested
        });
        // Before stable provider-model IDs were introduced, Infernaut published the active
        // inference ID. Continue accepting that ID so cached harness selections survive upgrades
        // and currently loaded LM Studio aliases remain usable. Stable identities take precedence
        // if an inference alias happens to collide with another model's provider identity.
        let model_match = stable_match.or_else(|| {
            self.models.iter().find(|model| {
                config.published_model_id(&model.provider, &model.upstream_id) == requested
            })
        });
        if let Some(model) = model_match {
            if model.is_audio_service() {
                return Err(ResolveError::CapabilityUnavailable {
                    model: model.id.clone(),
                    capability: "chat inference",
                });
            }
            config
                .provider(&model.provider)
                .ok_or_else(|| ResolveError::UnknownProvider(model.provider.clone()))?;
            return Ok(ResolvedModel::from_discovered(requested, model));
        }

        if let Some(alias) = config.requested_alias(requested) {
            return self.resolve_alias(config, requested, alias);
        }

        if let Some(route) = config.routes.iter().find(|route| {
            route.qualified_id() == requested
                || config.server.model_id_format.publish(&route.qualified_id()) == requested
        }) {
            return self.resolve_route(config, requested, route);
        }

        let (namespace, remainder) = requested
            .split_once('/')
            .ok_or_else(|| ResolveError::NotQualified(requested.to_owned()))?;

        if namespace == "alias" {
            return Err(ResolveError::UnknownAlias(remainder.to_owned()));
        }
        if namespace != "route" {
            return Err(ResolveError::ModelUnavailable(requested.to_owned()));
        }

        let route = config
            .route(remainder)
            .ok_or_else(|| ResolveError::UnknownRoute(remainder.to_owned()))?;
        self.resolve_route(config, requested, route)
    }

    pub fn resolve_transcription(
        &self,
        config: &Config,
        requested: &str,
    ) -> Result<ResolvedModel, ResolveError> {
        self.resolve_audio(config, requested, AudioService::Transcription)
    }

    pub fn resolve_audio(
        &self,
        config: &Config,
        requested: &str,
        service: AudioService,
    ) -> Result<ResolvedModel, ResolveError> {
        if let Some(route) = config.requested_route(requested) {
            let eligible = |model: &&DiscoveredModel| {
                model.supports_audio_service(service)
                    && route_model_base_allowed(config, route, model)
                    && model_meets_context_contract(model, route.context_length)
            };
            let selected = if route.candidates.is_empty() {
                self.models.iter().find(eligible)
            } else {
                route.candidates.iter().find_map(|candidate| {
                    self.models
                        .iter()
                        .find(|model| model.id == *candidate && eligible(model))
                })
            };
            return selected
                .map(|model| ResolvedModel::from_discovered(requested, model))
                .ok_or_else(|| ResolveError::NoEligibleModel {
                    route: route.id.clone(),
                    allowed_realms: route.allowed_realms.clone(),
                });
        }

        let model = self.models.iter().find(|model| {
            model.supports_audio_service(service)
                && (model.id == requested
                    || config.published_model_id(&model.provider, model.provider_model_id())
                        == requested
                    || config.published_model_id(&model.provider, &model.upstream_id) == requested)
        });
        if let Some(model) = model {
            config
                .provider(&model.provider)
                .ok_or_else(|| ResolveError::UnknownProvider(model.provider.clone()))?;
            Ok(ResolvedModel::from_discovered(requested, model))
        } else {
            Err(ResolveError::CapabilityUnavailable {
                model: requested.to_owned(),
                capability: service.as_str(),
            })
        }
    }

    pub fn resolve_default_audio(
        &self,
        config: &Config,
        service: AudioService,
    ) -> Result<ResolvedModel, ResolveError> {
        let routes = config
            .routes
            .iter()
            .filter(|route| self.route_is_audio(config, route, service))
            .collect::<Vec<_>>();
        if routes.len() == 1 {
            return self.resolve_audio(config, &routes[0].qualified_id(), service);
        }
        let models = self
            .models
            .iter()
            .filter(|model| {
                model.supports_audio_service(service) && config.provider(&model.provider).is_some()
            })
            .collect::<Vec<_>>();
        if models.len() == 1 {
            return Ok(ResolvedModel::from_discovered(&models[0].id, models[0]));
        }
        Err(ResolveError::AudioRouteRequired {
            service: service.as_str(),
            eligible_routes: routes
                .into_iter()
                .map(|route| route.qualified_id())
                .collect(),
        })
    }

    pub(crate) fn route_is_audio(
        &self,
        config: &Config,
        route: &crate::config::RouteConfig,
        service: AudioService,
    ) -> bool {
        !route.candidates.is_empty()
            && route
                .candidates
                .iter()
                .all(|candidate| config.is_static_audio_model(candidate, service))
    }

    fn resolve_route(
        &self,
        config: &Config,
        requested: &str,
        route: &crate::config::RouteConfig,
    ) -> Result<ResolvedModel, ResolveError> {
        let eligible = |model: &&DiscoveredModel| {
            !model.is_audio_service()
                && route_model_base_allowed(config, route, model)
                && model_meets_context_contract(model, route.context_length)
        };

        let selected = if route.candidates.is_empty() {
            self.models.iter().find(eligible)
        } else {
            route.candidates.iter().find_map(|candidate| {
                self.models
                    .iter()
                    .find(|model| model.id == *candidate && eligible(model))
            })
        };

        selected
            .map(|model| ResolvedModel::from_discovered(requested, model))
            .ok_or_else(|| ResolveError::NoEligibleModel {
                route: route.id.clone(),
                allowed_realms: route.allowed_realms.clone(),
            })
    }

    fn resolve_alias(
        &self,
        config: &Config,
        requested: &str,
        alias: &crate::config::ModelAliasConfig,
    ) -> Result<ResolvedModel, ResolveError> {
        if let Some(route_id) = alias.target.strip_prefix("route/") {
            let route = config
                .route(route_id)
                .ok_or_else(|| ResolveError::AliasUnavailable {
                    alias: alias.id.clone(),
                    target: alias.target.clone(),
                })?;
            let mut effective = route.clone();
            if let Some(contract) = alias.context_length {
                effective.context_length = Some(
                    effective
                        .context_length
                        .map_or(contract, |route_contract| route_contract.max(contract)),
                );
            }
            return self
                .resolve_route(config, requested, &effective)
                .map_err(|_| ResolveError::AliasUnavailable {
                    alias: alias.id.clone(),
                    target: alias.target.clone(),
                });
        }

        let model = self
            .models
            .iter()
            .find(|model| model.id == alias.target && !model.is_audio_service())
            .filter(|model| config.provider(&model.provider).is_some())
            .filter(|model| model_meets_context_contract(model, alias.context_length))
            .ok_or_else(|| ResolveError::AliasUnavailable {
                alias: alias.id.clone(),
                target: alias.target.clone(),
            })?;
        Ok(ResolvedModel::from_discovered(requested, model))
    }
}

fn route_model_base_allowed(
    config: &Config,
    route: &crate::config::RouteConfig,
    model: &DiscoveredModel,
) -> bool {
    let realm_allowed = route.allowed_realms.is_empty()
        || route
            .allowed_realms
            .iter()
            .any(|realm| realm == &model.realm);
    let candidate_allowed = route.candidates.is_empty() || route.candidates.contains(&model.id);
    let auto_routing_allowed = route.strategy != crate::config::RouteStrategy::Adaptive
        || !config.routing.models.iter().any(|profile| {
            !profile.disabled
                && wildcard_matches(&profile.model, &model.id)
                && !profile.auto_routing
        });
    realm_allowed
        && candidate_allowed
        && auto_routing_allowed
        && config.provider(&model.provider).is_some()
}

fn model_meets_context_contract(model: &DiscoveredModel, context_length: Option<u64>) -> bool {
    context_length.is_none_or(|contract| {
        model
            .context_length()
            .is_some_and(|context_length| context_length >= contract)
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolvedModel {
    pub requested_id: String,
    pub qualified_id: String,
    pub provider: String,
    pub upstream_id: String,
    pub management_id: Option<String>,
    pub loaded: Option<bool>,
    pub realm: String,
}

impl ResolvedModel {
    pub(crate) fn from_discovered(requested: &str, model: &DiscoveredModel) -> Self {
        Self {
            requested_id: requested.to_owned(),
            qualified_id: model.id.clone(),
            provider: model.provider.clone(),
            upstream_id: model.upstream_id.clone(),
            management_id: model.management_id.clone(),
            loaded: model.loaded,
            realm: model.realm.clone(),
        }
    }
}

#[derive(Debug)]
pub enum ResolveError {
    NotQualified(String),
    UnknownProvider(String),
    UnknownRoute(String),
    UnknownAlias(String),
    AliasUnavailable {
        alias: String,
        target: String,
    },
    ModelUnavailable(String),
    CapabilityUnavailable {
        model: String,
        capability: &'static str,
    },
    AudioRouteRequired {
        service: &'static str,
        eligible_routes: Vec<String>,
    },
    RouteContextExceeded {
        route: String,
        required_tokens: u64,
        context_length: u64,
    },
    AliasContextExceeded {
        alias: String,
        required_tokens: u64,
        context_length: u64,
    },
    NoEligibleModel {
        route: String,
        allowed_realms: Vec<String>,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotQualified(model) => {
                write!(formatter, "model '{model}' is not provider-qualified")
            }
            Self::UnknownProvider(provider) => write!(formatter, "unknown provider '{provider}'"),
            Self::UnknownRoute(route) => write!(formatter, "unknown route '{route}'"),
            Self::UnknownAlias(alias) => write!(formatter, "unknown alias '{alias}'"),
            Self::AliasUnavailable { alias, target } => write!(
                formatter,
                "alias '{alias}' target '{target}' is not currently eligible or available"
            ),
            Self::ModelUnavailable(model) => {
                write!(formatter, "model '{model}' is not currently available")
            }
            Self::CapabilityUnavailable { model, capability } => {
                write!(formatter, "model '{model}' does not support {capability}")
            }
            Self::AudioRouteRequired {
                service,
                eligible_routes,
            } => write!(
                formatter,
                "audio {service} routing is ambiguous; select one of [{}]",
                eligible_routes.join(", ")
            ),
            Self::RouteContextExceeded {
                route,
                required_tokens,
                context_length,
            } => write!(
                formatter,
                "request requires approximately {required_tokens} tokens, exceeding route '{route}' available context limit of {context_length} tokens"
            ),
            Self::AliasContextExceeded {
                alias,
                required_tokens,
                context_length,
            } => write!(
                formatter,
                "request requires approximately {required_tokens} tokens, exceeding alias '{alias}' context contract of {context_length} tokens"
            ),
            Self::NoEligibleModel {
                route,
                allowed_realms,
            } => write!(
                formatter,
                "no available model satisfies route '{route}' in realms [{}]",
                allowed_realms.join(", ")
            ),
        }
    }
}

async fn discover_provider(
    provider: ProviderConfig,
    client: reqwest::Client,
    auth: AuthManager,
) -> (ProviderStatus, Vec<DiscoveredModel>) {
    let api_key = provider.api_key();
    if let Some(environment_name) = &provider.api_key_env
        && api_key.is_none()
    {
        return (
            ProviderStatus {
                id: provider.id.clone(),
                title: provider.display_title().to_owned(),
                realm: provider.realm.clone(),
                available: false,
                model_count: 0,
                error: Some(format!(
                    "credential environment variable '{environment_name}' is missing or empty"
                )),
            },
            Vec::new(),
        );
    }

    if provider.is_audio_provider() {
        let provider_title = provider.display_title().to_owned();
        let models = provider
            .models
            .iter()
            .map(|model| {
                let mut capabilities = json!({
                    "chat": { "support": "unsupported", "confidence": "provider_declared" },
                    "streaming": { "support": "unknown", "confidence": "unverified" },
                    "tools": { "support": "unsupported", "confidence": "provider_declared" },
                    "structured_output": { "support": "unsupported", "confidence": "provider_declared" },
                    "vision": { "support": "unsupported", "confidence": "provider_declared" },
                    "transcription": { "support": "unsupported", "confidence": "provider_declared" },
                    "speech": { "support": "unsupported", "confidence": "provider_declared" }
                });
                let services = [AudioService::Transcription, AudioService::Speech]
                    .into_iter()
                    .filter(|service| model.supports_audio_service(provider.kind, *service))
                    .collect::<Vec<_>>();
                for service in &services {
                    capabilities[service.as_str()] =
                        json!({ "support": "native", "confidence": "provider_declared" });
                }
                DiscoveredModel {
                    id: format!("{}/{}", provider.id, model.id),
                    provider: provider.id.clone(),
                    provider_title: provider_title.clone(),
                    upstream_id: model.upstream_id().to_owned(),
                    management_id: None,
                    loaded: None,
                    realm: provider.realm.clone(),
                    created: 0,
                    provider_priority: provider.priority,
                    capabilities,
                    provider_metadata: json!({
                        "adapter": "openai_audio",
                        "services": services.iter().map(|service| service.as_str()).collect::<Vec<_>>(),
                        "display_name": model.display_name(),
                        "static": true,
                    }),
                }
            })
            .collect::<Vec<_>>();
        return (
            ProviderStatus {
                id: provider.id,
                title: provider_title,
                realm: provider.realm,
                available: true,
                model_count: models.len(),
                error: None,
            },
            models,
        );
    }

    if provider.kind == ProviderKind::LmStudio {
        return discover_lm_studio_provider(provider, client).await;
    }
    if provider.kind == ProviderKind::LlamaCpp {
        return discover_llama_cpp_provider(provider, client).await;
    }
    if provider.kind == ProviderKind::Vllm {
        return discover_vllm_provider(provider, client).await;
    }
    if provider.kind == ProviderKind::OllamaCloud {
        return discover_ollama_cloud_provider(provider, client).await;
    }
    let oauth = if provider.kind.uses_oauth() {
        match auth.credential(&provider, &client).await {
            Ok(value) => Some(value),
            Err(error) => return unavailable(&provider, error.to_string()),
        }
    } else {
        None
    };
    if provider.kind == ProviderKind::OpenaiCodex {
        return discover_codex_provider(provider, client, oauth.expect("OAuth credential")).await;
    }
    if matches!(provider.kind, ProviderKind::Xai | ProviderKind::XaiOauth) {
        return discover_xai_provider(provider, client, oauth).await;
    }

    let url = format!(
        "{}/models",
        provider.effective_base_url().trim_end_matches('/')
    );
    let mut request = client.get(url).timeout(Duration::from_secs(10));
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }

    let result = async {
        let response = request.send().await?.error_for_status()?;
        let payload: UpstreamModelList = response.json().await?;
        Ok::<_, reqwest::Error>(if payload.data.is_empty() {
            payload.models
        } else {
            payload.data
        })
    }
    .await;

    match result {
        Ok(upstream_models) => {
            let provider_title = provider.display_title().to_owned();
            let models = upstream_models
                .into_iter()
                .filter(|model| !model.id.is_empty() && provider.model_allowed(&model.id))
                .map(|model| generic_model(&provider, &provider_title, model, None))
                .collect::<Vec<_>>();
            let status = ProviderStatus {
                id: provider.id,
                title: provider_title,
                realm: provider.realm,
                available: true,
                model_count: models.len(),
                error: None,
            };
            (status, models)
        }
        Err(error) => {
            let provider_title = provider.display_title().to_owned();
            (
                ProviderStatus {
                    id: provider.id,
                    title: provider_title,
                    realm: provider.realm,
                    available: false,
                    model_count: 0,
                    error: Some(error.to_string()),
                },
                Vec::new(),
            )
        }
    }
}

async fn discover_ollama_cloud_provider(
    provider: ProviderConfig,
    client: reqwest::Client,
) -> (ProviderStatus, Vec<DiscoveredModel>) {
    let provider_title = provider.display_title().to_owned();
    let models_url = format!(
        "{}/models",
        provider.effective_base_url().trim_end_matches('/')
    );
    let mut request = client.get(models_url).timeout(Duration::from_secs(10));
    if let Some(api_key) = provider.api_key() {
        request = request.bearer_auth(api_key);
    }

    let result = async {
        let response = request.send().await?.error_for_status()?;
        response.json::<UpstreamModelList>().await
    }
    .await;

    match result {
        Ok(payload) => {
            let upstream_models = if payload.data.is_empty() {
                payload.models
            } else {
                payload.data
            };
            let mut models = upstream_models
                .into_iter()
                .filter(|model| !model.id.is_empty() && provider.model_allowed(&model.id))
                .map(|model| generic_model(&provider, &provider_title, model, None))
                .collect::<Vec<_>>();

            let details = join_all(models.iter().map(|model| {
                discover_ollama_model_details(&provider, &client, model.upstream_id.clone())
            }))
            .await;
            for (model, details) in models.iter_mut().zip(details) {
                if let Some(details) = details {
                    enrich_ollama_model(model, details);
                }
            }

            (
                ProviderStatus {
                    id: provider.id,
                    title: provider_title,
                    realm: provider.realm,
                    available: true,
                    model_count: models.len(),
                    error: None,
                },
                models,
            )
        }
        Err(error) => unavailable(&provider, error.to_string()),
    }
}

async fn discover_ollama_model_details(
    provider: &ProviderConfig,
    client: &reqwest::Client,
    model: String,
) -> Option<Value> {
    let inference_base = provider.effective_base_url().trim_end_matches('/');
    let native_base = inference_base.strip_suffix("/v1").unwrap_or(inference_base);
    let url = format!("{native_base}/api/show");
    let mut request = client
        .post(&url)
        .timeout(Duration::from_secs(10))
        .json(&json!({ "model": model, "verbose": false }));
    if let Some(api_key) = provider.api_key() {
        request = request.bearer_auth(api_key);
    }

    match request.send().await {
        Ok(response) => match response.error_for_status() {
            Ok(response) => match response.json::<Value>().await {
                Ok(payload) => Some(payload),
                Err(error) => {
                    debug!(
                        provider = provider.id,
                        endpoint = url,
                        error = %error,
                        "could not decode Ollama model details"
                    );
                    None
                }
            },
            Err(error) => {
                debug!(
                    provider = provider.id,
                    endpoint = url,
                    error = %error,
                    "Ollama model details are unavailable; manual context override remains available"
                );
                None
            }
        },
        Err(error) => {
            debug!(
                provider = provider.id,
                endpoint = url,
                error = %error,
                "Ollama model-details request failed; manual context override remains available"
            );
            None
        }
    }
}

fn ollama_context_length(payload: &Value) -> Option<u64> {
    let provider_reported = payload
        .get("model_info")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|model_info| model_info.iter())
        .filter(|(key, _)| {
            key.eq_ignore_ascii_case("context_length")
                || key.to_ascii_lowercase().ends_with(".context_length")
        })
        .filter_map(|(_, value)| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
        .filter(|value| *value > 0)
        .max();
    provider_reported.or_else(|| context_length_from_value(payload))
}

fn enrich_ollama_model(model: &mut DiscoveredModel, details: Value) {
    if let Some(context_length) = ollama_context_length(&details) {
        model.provider_metadata["max_context_length"] = json!(context_length);
        model.provider_metadata["maximum_context_length"] = json!(context_length);
    }
    if let Some(capabilities) = details.get("capabilities").and_then(Value::as_array) {
        let supports = |name: &str| {
            capabilities
                .iter()
                .any(|value| value.as_str() == Some(name))
        };
        if supports("tools") {
            model.capabilities["tools"] =
                json!({ "support": "native", "confidence": "provider_reported" });
        }
        if supports("vision") {
            model.capabilities["vision"] =
                json!({ "support": "native", "confidence": "provider_reported" });
        }
        if supports("thinking") {
            model.capabilities["reasoning"] =
                json!({ "support": "native", "confidence": "provider_reported" });
        }
    }
    model.provider_metadata["ollama_details"] =
        details.get("details").cloned().unwrap_or(Value::Null);
}

async fn discover_vllm_provider(
    provider: ProviderConfig,
    client: reqwest::Client,
) -> (ProviderStatus, Vec<DiscoveredModel>) {
    let provider_title = provider.display_title().to_owned();
    let models_url = format!(
        "{}/models",
        provider.effective_base_url().trim_end_matches('/')
    );
    let mut models_request = client.get(models_url).timeout(Duration::from_secs(10));
    if let Some(api_key) = provider.api_key() {
        models_request = models_request.bearer_auth(api_key);
    }

    let server_context = discover_vllm_server_context(&provider, &client);
    let models_request = async {
        let response = models_request.send().await?.error_for_status()?;
        response.json::<UpstreamModelList>().await
    };
    let (models_result, server_context) = tokio::join!(models_request, server_context);

    match models_result {
        Ok(payload) => {
            let upstream_models = if payload.data.is_empty() {
                payload.models
            } else {
                payload.data
            };
            let models = upstream_models
                .into_iter()
                .filter(|model| !model.id.is_empty() && provider.model_allowed(&model.id))
                .map(|model| generic_model(&provider, &provider_title, model, server_context))
                .collect::<Vec<_>>();
            (
                ProviderStatus {
                    id: provider.id,
                    title: provider_title,
                    realm: provider.realm,
                    available: true,
                    model_count: models.len(),
                    error: None,
                },
                models,
            )
        }
        Err(error) => unavailable(&provider, error.to_string()),
    }
}

async fn discover_vllm_server_context(
    provider: &ProviderConfig,
    client: &reqwest::Client,
) -> Option<u64> {
    let inference_base = provider.effective_base_url().trim_end_matches('/');
    let server_base = inference_base.strip_suffix("/v1").unwrap_or(inference_base);
    let url = format!("{server_base}/server_info?config_format=json");
    let mut request = client.get(&url).timeout(Duration::from_secs(5));
    if let Some(api_key) = provider.api_key() {
        request = request.bearer_auth(api_key);
    }

    match request.send().await {
        Ok(response) => match response.error_for_status() {
            Ok(response) => match response.json::<Value>().await {
                Ok(payload) => {
                    let context_length = payload
                        .pointer("/vllm_config/model_config/max_model_len")
                        .and_then(Value::as_u64)
                        .or_else(|| value_for_key(&payload, "max_model_len"))
                        .filter(|value| *value > 0);
                    if context_length.is_none() {
                        debug!(
                            provider = provider.id,
                            endpoint = url,
                            "vLLM server info did not contain max_model_len"
                        );
                    }
                    context_length
                }
                Err(error) => {
                    debug!(
                        provider = provider.id,
                        endpoint = url,
                        error = %error,
                        "could not decode vLLM server info"
                    );
                    None
                }
            },
            Err(error) => {
                debug!(
                    provider = provider.id,
                    endpoint = url,
                    error = %error,
                    "vLLM server info is unavailable; model-list metadata will be used"
                );
                None
            }
        },
        Err(error) => {
            debug!(
                provider = provider.id,
                endpoint = url,
                error = %error,
                "vLLM server info request failed; model-list metadata will be used"
            );
            None
        }
    }
}

fn generic_model(
    provider: &ProviderConfig,
    provider_title: &str,
    model: UpstreamModel,
    provider_context_length: Option<u64>,
) -> DiscoveredModel {
    let details = json!(model.metadata);
    let context_length = context_length_from_value(&details).or(provider_context_length);
    let mut provider_metadata = json!({
        "adapter": provider.kind,
        "owned_by": model.owned_by,
        "details": details,
    });
    if let Some(context_length) = context_length {
        provider_metadata["max_context_length"] = json!(context_length);
        provider_metadata["maximum_context_length"] = json!(context_length);
    }

    DiscoveredModel {
        id: format!("{}/{}", provider.id, model.id),
        provider: provider.id.clone(),
        provider_title: provider_title.to_owned(),
        upstream_id: model.id,
        management_id: None,
        loaded: None,
        realm: provider.realm.clone(),
        created: model.created,
        provider_priority: provider.priority,
        capabilities: json!({
            "chat": { "support": "unknown", "confidence": "unverified" },
            "streaming": { "support": "unknown", "confidence": "unverified" },
            "tools": { "support": "unknown", "confidence": "unverified" },
            "structured_output": { "support": "unknown", "confidence": "unverified" },
            "vision": { "support": "unknown", "confidence": "unverified" }
        }),
        provider_metadata,
    }
}

async fn discover_xai_provider(
    provider: ProviderConfig,
    client: reqwest::Client,
    oauth: Option<RuntimeCredential>,
) -> (ProviderStatus, Vec<DiscoveredModel>) {
    let provider_title = provider.display_title().to_owned();
    let url = format!(
        "{}/language-models",
        provider.effective_base_url().trim_end_matches('/')
    );
    let mut request = client.get(url).timeout(Duration::from_secs(10));
    if let Some(api_key) = provider.api_key() {
        request = request.bearer_auth(api_key);
    } else if let Some(oauth) = oauth {
        request = request.bearer_auth(oauth.access_token);
    }
    let result = async {
        let response = request.send().await?.error_for_status()?;
        response.json::<XaiLanguageModelList>().await
    }
    .await;

    match result {
        Ok(payload) => {
            let models = xai_models(&provider, &provider_title, payload);
            (
                ProviderStatus {
                    id: provider.id,
                    title: provider_title,
                    realm: provider.realm,
                    available: true,
                    model_count: models.len(),
                    error: None,
                },
                models,
            )
        }
        Err(error) => (
            ProviderStatus {
                id: provider.id,
                title: provider_title,
                realm: provider.realm,
                available: false,
                model_count: 0,
                error: Some(error.to_string()),
            },
            Vec::new(),
        ),
    }
}

async fn discover_codex_provider(
    provider: ProviderConfig,
    client: reqwest::Client,
    credential: RuntimeCredential,
) -> (ProviderStatus, Vec<DiscoveredModel>) {
    let provider_title = provider.display_title().to_owned();
    let url = format!(
        "{}/models?client_version={}",
        provider.effective_base_url().trim_end_matches('/'),
        CODEX_CLIENT_VERSION
    );
    let mut request = client.get(url).bearer_auth(credential.access_token);
    if let Some(account_id) = credential.account_id {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    let result = async {
        let response = request.send().await?.error_for_status()?;
        response.json::<CodexModelList>().await
    }
    .await;
    match result {
        Ok(mut payload) => {
            payload.models.sort_by_key(|model| model.priority);
            let models = payload
                .models
                .into_iter()
                .filter(|model| {
                    !model.slug.is_empty()
                        && !matches!(model.visibility.as_str(), "hide" | "hidden")
                        && provider.model_allowed(&model.slug)
                })
                .map(|model| DiscoveredModel {
                    id: format!("{}/{}", provider.id, model.slug),
                    provider: provider.id.clone(),
                    provider_title: provider_title.clone(),
                    upstream_id: model.slug,
                    management_id: None,
                    loaded: None,
                    realm: provider.realm.clone(),
                    created: 0,
                    provider_priority: provider.priority,
                    capabilities: json!({
                        "chat": { "support": "native", "confidence": "provider_reported" },
                        "streaming": { "support": "native", "confidence": "provider_reported" },
                        "tools": { "support": "native", "confidence": "provider_reported" },
                        "structured_output": { "support": "unknown", "confidence": "unverified" },
                        "vision": { "support": "unknown", "confidence": "unverified" }
                    }),
                    provider_metadata: json!({
                        "adapter": "openai_codex",
                        "visibility": model.visibility,
                        "catalog": model.metadata,
                    }),
                })
                .collect::<Vec<_>>();
            let status = ProviderStatus {
                id: provider.id,
                title: provider_title,
                realm: provider.realm,
                available: true,
                model_count: models.len(),
                error: None,
            };
            (status, models)
        }
        Err(error) => unavailable(&provider, error.to_string()),
    }
}

fn unavailable(provider: &ProviderConfig, error: String) -> (ProviderStatus, Vec<DiscoveredModel>) {
    (
        ProviderStatus {
            id: provider.id.clone(),
            title: provider.display_title().to_owned(),
            realm: provider.realm.clone(),
            available: false,
            model_count: 0,
            error: Some(error),
        },
        Vec::new(),
    )
}

fn xai_models(
    provider: &ProviderConfig,
    provider_title: &str,
    payload: XaiLanguageModelList,
) -> Vec<DiscoveredModel> {
    let mut seen = HashSet::new();
    let mut discovered = Vec::new();
    for model in payload.models {
        let canonical_id = model.id.clone();
        let mut selectable_ids = vec![model.id.clone()];
        selectable_ids.extend(model.aliases.iter().cloned());
        for selectable_id in selectable_ids {
            if selectable_id.is_empty()
                || !seen.insert(selectable_id.clone())
                || !provider.model_allowed(&selectable_id)
            {
                continue;
            }
            let vision = model
                .input_modalities
                .iter()
                .any(|modality| modality == "image");
            discovered.push(DiscoveredModel {
                id: format!("{}/{}", provider.id, selectable_id),
                provider: provider.id.clone(),
                provider_title: provider_title.to_owned(),
                upstream_id: selectable_id.clone(),
                management_id: None,
                loaded: None,
                realm: provider.realm.clone(),
                created: model.created,
                provider_priority: provider.priority,
                capabilities: json!({
                    "chat": { "support": "native", "confidence": "provider_reported" },
                    "streaming": { "support": "unknown", "confidence": "unverified" },
                    "tools": { "support": "unknown", "confidence": "unverified" },
                    "structured_output": { "support": "unknown", "confidence": "unverified" },
                    "vision": {
                        "support": if vision { "native" } else { "unsupported" },
                        "confidence": "provider_reported"
                    }
                }),
                provider_metadata: json!({
                    "adapter": "xai",
                    "canonical_id": canonical_id,
                    "is_alias": selectable_id != canonical_id,
                    "aliases": model.aliases,
                    "owned_by": model.owned_by,
                    "fingerprint": model.fingerprint,
                    "version": model.version,
                    "input_modalities": model.input_modalities,
                    "output_modalities": model.output_modalities,
                    "details": model.metadata,
                }),
            });
        }
    }
    discovered
}

async fn discover_lm_studio_provider(
    provider: ProviderConfig,
    client: reqwest::Client,
) -> (ProviderStatus, Vec<DiscoveredModel>) {
    let provider_title = provider.display_title().to_owned();
    let inference_base = provider.effective_base_url().trim_end_matches('/');
    let server_base = inference_base.strip_suffix("/v1").unwrap_or(inference_base);
    let url = format!("{server_base}/api/v1/models");
    let mut request = client.get(url).timeout(Duration::from_secs(10));
    if let Some(api_key) = provider.api_key() {
        request = request.bearer_auth(api_key);
    }

    let result = async {
        let response = request.send().await?.error_for_status()?;
        response.json::<LmStudioModelList>().await
    }
    .await;

    match result {
        Ok(payload) => {
            let models = lm_studio_models(&provider, &provider_title, payload);
            (
                ProviderStatus {
                    id: provider.id,
                    title: provider_title,
                    realm: provider.realm,
                    available: true,
                    model_count: models.len(),
                    error: None,
                },
                models,
            )
        }
        Err(error) => (
            ProviderStatus {
                id: provider.id,
                title: provider_title,
                realm: provider.realm,
                available: false,
                model_count: 0,
                error: Some(error.to_string()),
            },
            Vec::new(),
        ),
    }
}

fn lm_studio_models(
    provider: &ProviderConfig,
    provider_title: &str,
    payload: LmStudioModelList,
) -> Vec<DiscoveredModel> {
    payload
        .models
        .into_iter()
        .filter(|model| {
            model.model_type == "llm" && !model.key.is_empty() && provider.model_allowed(&model.key)
        })
        .map(|model| lm_studio_model(provider, provider_title, model))
        .collect()
}

fn lm_studio_model(
    provider: &ProviderConfig,
    provider_title: &str,
    model: LmStudioModel,
) -> DiscoveredModel {
    let tools_support = if model.capabilities.trained_for_tool_use {
        "native"
    } else {
        "unsupported"
    };
    let vision_support = if model.capabilities.vision {
        "native"
    } else {
        "unsupported"
    };
    let reasoning_support = if model.capabilities.reasoning.is_some() {
        "native"
    } else {
        "unknown"
    };
    let loaded_instance_ids = model
        .loaded_instances
        .iter()
        .map(|instance| instance.id.clone())
        .collect::<Vec<_>>();
    let loaded_instance_id = loaded_instance_ids.first().cloned();
    let loaded_context_length = model
        .loaded_instances
        .first()
        .and_then(|instance| instance.config.context_length);
    let loaded_parallel = model
        .loaded_instances
        .first()
        .and_then(|instance| instance.config.parallel);
    let effective_context_length = loaded_context_length.unwrap_or(model.max_context_length);
    let model_key = model.key;

    DiscoveredModel {
        id: format!("{}/{}", provider.id, model_key),
        provider: provider.id.clone(),
        provider_title: provider_title.to_owned(),
        upstream_id: loaded_instance_id.unwrap_or_else(|| model_key.clone()),
        management_id: Some(model_key),
        loaded: Some(!loaded_instance_ids.is_empty()),
        realm: provider.realm.clone(),
        created: 0,
        provider_priority: provider.priority,
        capabilities: json!({
            "tools": { "support": tools_support, "confidence": "provider_reported" },
            "structured_output": { "support": "unknown", "confidence": "unverified" },
            "vision": { "support": vision_support, "confidence": "provider_reported" },
            "reasoning": {
                "support": reasoning_support,
                "confidence": if reasoning_support == "native" { "provider_reported" } else { "unverified" },
                "options": model.capabilities.reasoning,
            }
        }),
        provider_metadata: json!({
            "display_name": model.display_name,
            "architecture": model.architecture,
            "quantization": model.quantization,
            "size_bytes": model.size_bytes,
            "params": model.params_string,
            "max_context_length": effective_context_length,
            "maximum_context_length": model.max_context_length,
            "configured_context_length": loaded_context_length,
            "configured_parallel": loaded_parallel,
            "format": model.format,
            "loaded": !loaded_instance_ids.is_empty(),
            "loaded_instance_ids": loaded_instance_ids,
        }),
    }
}

async fn discover_llama_cpp_provider(
    provider: ProviderConfig,
    client: reqwest::Client,
) -> (ProviderStatus, Vec<DiscoveredModel>) {
    let provider_title = provider.display_title().to_owned();
    let inference_base = provider.effective_base_url().trim_end_matches('/');
    let server_base = inference_base.strip_suffix("/v1").unwrap_or(inference_base);
    let url = format!("{server_base}/models");
    let mut request = client.get(url).timeout(Duration::from_secs(10));
    if let Some(api_key) = provider.api_key() {
        request = request.bearer_auth(api_key);
    }

    let result = async {
        let response = request.send().await?.error_for_status()?;
        response.json::<LlamaCppModelList>().await
    }
    .await;

    match result {
        Ok(payload) => {
            let models = llama_cpp_models(&provider, &provider_title, payload);
            (
                ProviderStatus {
                    id: provider.id,
                    title: provider_title,
                    realm: provider.realm,
                    available: true,
                    model_count: models.len(),
                    error: None,
                },
                models,
            )
        }
        Err(error) => (
            ProviderStatus {
                id: provider.id,
                title: provider_title,
                realm: provider.realm,
                available: false,
                model_count: 0,
                error: Some(error.to_string()),
            },
            Vec::new(),
        ),
    }
}

fn llama_cpp_models(
    provider: &ProviderConfig,
    provider_title: &str,
    payload: LlamaCppModelList,
) -> Vec<DiscoveredModel> {
    payload
        .data
        .into_iter()
        .filter(|model| !model.id.is_empty() && provider.model_allowed(&model.id))
        .map(|model| llama_cpp_model(provider, provider_title, model))
        .collect()
}

fn llama_cpp_model(
    provider: &ProviderConfig,
    provider_title: &str,
    model: LlamaCppModel,
) -> DiscoveredModel {
    let input_modalities = model
        .architecture
        .get("input_modalities")
        .and_then(Value::as_array);
    let vision = input_modalities.is_some_and(|modalities| {
        modalities
            .iter()
            .any(|modality| modality.as_str() == Some("image"))
    });
    let vision_support = if vision { "native" } else { "unsupported" };
    let status_value = if model.status.value.is_empty() {
        "unknown".to_owned()
    } else {
        model.status.value.clone()
    };
    let loaded = status_value == "loaded";
    let model_id = model.id;

    DiscoveredModel {
        id: format!("{}/{}", provider.id, model_id),
        provider: provider.id.clone(),
        provider_title: provider_title.to_owned(),
        upstream_id: model_id.clone(),
        management_id: Some(model_id),
        loaded: Some(loaded),
        realm: provider.realm.clone(),
        created: model.created,
        provider_priority: provider.priority,
        capabilities: json!({
            "tools": { "support": "unknown", "confidence": "unverified" },
            "structured_output": { "support": "native", "confidence": "provider_reported" },
            "vision": { "support": vision_support, "confidence": "provider_reported" },
            "reasoning": { "support": "unknown", "confidence": "unverified" }
        }),
        provider_metadata: json!({
            "status": status_value,
            "status_args": model.status.args,
            "load_failed": model.status.failed,
            "exit_code": model.status.exit_code,
            "architecture": model.architecture,
            "meta": model.meta,
        }),
    }
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ModelAliasConfig, NoEligibleModel, ProviderKind, RouteConfig, RouteStrategy, RoutingEffort,
        RoutingTask, ServerConfig, StaticModelConfig,
    };

    #[test]
    fn target_advertised_reasoning_efforts_flow_into_published_capabilities() {
        let capabilities = json!({
            "reasoning": {
                "support": "native",
                "options": { "allowed_options": ["off", "low", "medium", "high"] }
            }
        });
        let published = anthropic_capabilities(&capabilities, RecipeReasoningEffort::Inherit);
        assert_eq!(published["thinking"]["supported"], true);
        assert_eq!(published["effort"]["supported"], true);
        assert_eq!(published["effort"]["none"]["supported"], true);
        assert_eq!(published["effort"]["low"]["supported"], true);
        assert_eq!(published["effort"]["medium"]["supported"], true);
        assert_eq!(published["effort"]["high"]["supported"], true);
        assert_eq!(published["effort"]["max"]["supported"], false);
    }

    fn fixture() -> (Config, Catalog) {
        let config = Config {
            server: ServerConfig::default(),
            privacy_lock: crate::config::PrivacyLockConfig::default(),
            probes: crate::config::ProbeConfig::default(),
            routing: crate::config::RoutingConfig::default(),
            clients: vec![],
            recipes: vec![],
            providers: vec![
                ProviderConfig {
                    id: "cloud".into(),
                    kind: ProviderKind::OpenaiCompatible,
                    title: "Cloud".into(),
                    alias: None,
                    base_url: "http://cloud/v1".into(),
                    realm: "cloud".into(),
                    api_key_env: None,
                    oauth_client_id: None,
                    anthropic_compat: None,
                    models: vec![],
                    include_models: vec![],
                    exclude_models: vec![],
                    priority: 1,
                    auto_load: false,
                    jit_load: false,
                    load_context_length: None,
                    disabled: false,
                },
                ProviderConfig {
                    id: "local".into(),
                    kind: ProviderKind::OpenaiCompatible,
                    title: "Local".into(),
                    alias: None,
                    base_url: "http://local/v1".into(),
                    realm: "local".into(),
                    api_key_env: None,
                    oauth_client_id: None,
                    anthropic_compat: None,
                    models: vec![],
                    include_models: vec![],
                    exclude_models: vec![],
                    priority: 10,
                    auto_load: false,
                    jit_load: false,
                    load_context_length: None,
                    disabled: false,
                },
            ],
            routes: vec![RouteConfig {
                id: "private".into(),
                title: "Private".into(),
                description: String::new(),
                allowed_realms: vec!["local".into()],
                candidates: vec![],
                strategy: crate::config::RouteStrategy::FirstAvailable,
                task: crate::config::RoutingTask::Auto,
                effort: crate::config::RoutingEffort::Auto,
                context_length: None,
                token_estimate_bytes_per_token: None,
                compression_max_output_tokens: None,
                on_no_eligible_model: NoEligibleModel::Deny,
            }],
            aliases: vec![],
            plugins: vec![],
        };
        let model = |provider: &str, realm: &str, priority| DiscoveredModel {
            id: format!("{provider}/model"),
            provider: provider.into(),
            provider_title: provider.into(),
            upstream_id: "model".into(),
            management_id: None,
            loaded: None,
            realm: realm.into(),
            created: 0,
            provider_priority: priority,
            capabilities: json!({}),
            provider_metadata: json!({}),
        };
        let catalog = Catalog {
            models: vec![model("cloud", "cloud", 1), model("local", "local", 10)],
            providers: vec![],
            refreshed_at: 1,
        };
        (config, catalog)
    }

    #[test]
    fn private_route_cannot_select_cloud_model() {
        let (config, catalog) = fixture();
        let resolved = catalog.resolve(&config, "route/private").unwrap();
        assert_eq!(resolved.qualified_id, "local/model");
        assert_eq!(resolved.realm, "local");
    }

    #[test]
    fn aliases_publish_and_resolve_to_exact_targets_without_fallback() {
        let (mut config, catalog) = fixture();
        config.aliases.push(ModelAliasConfig {
            id: "cron".into(),
            title: "Cron automation".into(),
            target: "cloud/model".into(),
            context_length: None,
            disabled: false,
        });

        let published_id = config.server.model_id_format.publish("alias/cron");
        let published = catalog
            .openai_model_list(&config)
            .data
            .into_iter()
            .find(|model| model.id == published_id)
            .expect("alias is published");
        assert_eq!(published.metadata["kind"], "alias");
        assert_eq!(published.metadata["target"], "cloud/model");
        let resolved = catalog.resolve(&config, &published_id).unwrap();
        assert_eq!(resolved.requested_id, published_id);
        assert_eq!(resolved.qualified_id, "cloud/model");

        config.privacy_lock.mode = crate::config::PrivacyMode::LocalOnly;
        assert!(matches!(
            catalog.resolve(&config, &published_id),
            Err(ResolveError::AliasUnavailable { .. })
        ));
    }

    #[test]
    fn alias_context_contract_rejects_an_undersized_exact_target() {
        let (mut config, mut catalog) = fixture();
        catalog.models[1].provider_metadata = json!({ "max_context_length": 65_536 });
        config.aliases.push(ModelAliasConfig {
            id: "long-cron".into(),
            title: String::new(),
            target: "local/model".into(),
            context_length: Some(131_072),
            disabled: false,
        });
        assert!(matches!(
            catalog.resolve(&config, "alias/long-cron"),
            Err(ResolveError::AliasUnavailable { .. })
        ));
    }

    #[test]
    fn alias_can_pin_an_existing_route_without_becoming_adaptive() {
        let (mut config, catalog) = fixture();
        config.aliases.push(ModelAliasConfig {
            id: "private-cron".into(),
            title: String::new(),
            target: "route/private".into(),
            context_length: None,
            disabled: false,
        });
        assert_eq!(
            config
                .requested_route("alias/private-cron")
                .map(|route| route.id.as_str()),
            Some("private")
        );
        let resolved = catalog.resolve(&config, "alias/private-cron").unwrap();
        assert_eq!(resolved.qualified_id, "local/model");
    }

    #[test]
    fn recipe_aliases_are_published_only_with_the_authorized_recipe_catalog() {
        let (mut config, catalog) = fixture();
        config.recipes.push(RecipeConfig {
            id: "hermes-cron".into(),
            title: "Hermes cron".into(),
            kind: crate::config::RecipeKind::Hermes,
            protocol: crate::config::RecipeProtocol::Openai,
            allowed_realms: vec!["local".into()],
            slots: vec![RecipeSlotConfig {
                id: "primary".into(),
                title: "Primary".into(),
                target: "local/model".into(),
                trigger: crate::config::RecipeSlotTrigger::Model,
                max_output_tokens: None,
                reasoning_effort: RecipeReasoningEffort::Inherit,
            }],
            disabled: false,
        });
        config.aliases.push(ModelAliasConfig {
            id: "cron".into(),
            title: "Cron".into(),
            target: "recipe/hermes-cron/primary".into(),
            context_length: None,
            disabled: false,
        });
        config.validate().unwrap();
        let published = config.server.model_id_format.publish("alias/cron");

        assert!(
            catalog
                .openai_model_list(&config)
                .data
                .iter()
                .all(|model| model.id != published)
        );
        let recipe = config.recipe("hermes-cron").unwrap();
        let authorized = catalog.catalog_with_selectable_recipes(&config, &[recipe]);
        let alias = authorized
            .data
            .iter()
            .find(|model| model.id == published)
            .expect("authorized recipe alias is published");
        assert_eq!(alias.metadata["target"], "recipe/hermes-cron/primary");
        assert!(catalog.target_available(&config, "recipe/hermes-cron/primary"));
    }

    #[test]
    fn nested_adaptive_route_publishes_and_resolves_with_colon_ids() {
        let (mut config, catalog) = fixture();
        config.server.model_id_format = crate::config::ModelIdFormat::ColonQualified;
        config.routes[0].id = "private/auto".into();
        config.routes[0].strategy = RouteStrategy::Adaptive;
        config.routes[0].task = RoutingTask::Auto;
        config.routes[0].effort = RoutingEffort::Auto;

        let ids = catalog
            .openai_model_list(&config)
            .data
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>();
        assert!(ids.contains(&"route:private:auto".to_owned()));
        assert_eq!(
            catalog
                .resolve(&config, "route:private:auto")
                .unwrap()
                .qualified_id,
            "local/model"
        );
    }

    #[test]
    fn vllm_server_info_context_is_extracted_from_runtime_config() {
        let payload = json!({
            "vllm_config": {
                "model_config": {
                    "model": "org/qwen",
                    "max_model_len": 262144
                }
            }
        });

        assert_eq!(
            payload
                .pointer("/vllm_config/model_config/max_model_len")
                .and_then(Value::as_u64)
                .or_else(|| value_for_key(&payload, "max_model_len")),
            Some(262144)
        );
    }

    #[test]
    fn ollama_native_details_report_the_largest_model_context() {
        let payload = json!({
            "model_info": {
                "qwen3.context_length": 262144,
                "qwen3.vision.context_length": 4096,
                "qwen3.embedding_length": 4096
            }
        });
        assert_eq!(ollama_context_length(&payload), Some(262144));
    }

    #[test]
    fn model_context_override_preserves_provider_detection() {
        let (mut config, mut catalog) = fixture();
        config
            .routing
            .models
            .push(crate::config::RoutingModelConfig {
                model: "local/model".into(),
                context_length: Some(128 * 1024),
                auto_routing: true,
                roles: Vec::new(),
                strengths: BTreeMap::new(),
                traits: crate::config::RoutingModelTraits::default(),
                disabled: false,
            });
        catalog.models[1].provider_metadata = json!({ "max_context_length": 262144 });

        apply_model_context_override(&mut catalog.models[1], &config);

        assert_eq!(catalog.models[1].context_length(), Some(128 * 1024));
        assert_eq!(
            catalog.models[1].provider_metadata["detected_context_length"],
            262144
        );
        assert_eq!(
            catalog.models[1].provider_metadata["context_length_source"],
            "operator_override"
        );

        config.routing.models[0].context_length = None;
        apply_model_context_override(&mut catalog.models[1], &config);
        assert_eq!(catalog.models[1].context_length(), Some(262144));
        assert!(
            catalog.models[1]
                .provider_metadata
                .get("context_length_override")
                .is_none()
        );
    }

    #[test]
    fn vllm_model_uses_server_context_and_publishes_anthropic_fields() {
        let (mut config, mut catalog) = fixture();
        config.providers[1].kind = ProviderKind::Vllm;
        config.providers[1].alias = Some("agam".into());
        let upstream: UpstreamModel = serde_json::from_value(json!({
            "id": "qwen3.6-27b-nvfp4",
            "object": "model",
            "owned_by": "vllm"
        }))
        .unwrap();
        let mut model = generic_model(&config.providers[1], "Agamotto", upstream, Some(262144));
        normalize_context_metadata(&mut model);
        catalog.models[1] = model;

        let published = catalog
            .openai_model_list(&config)
            .data
            .into_iter()
            .find(|model| model.id == "agam/qwen3.6-27b-nvfp4")
            .unwrap();

        assert_eq!(published.max_input_tokens, Some(262144));
        assert_eq!(published.max_tokens, None);
        assert_eq!(
            published.metadata["provider_metadata"]["max_context_length"],
            262144
        );
        assert_eq!(published.model_type, "model");
        assert_eq!(published.display_name, "Agamotto — qwen3.6-27b-nvfp4");
    }

    #[test]
    fn claude_recipe_publishes_1m_suffix_only_for_verified_context() {
        let (mut config, mut catalog) = fixture();
        config.server.model_id_format = crate::config::ModelIdFormat::ColonQualified;
        config.recipes.push(RecipeConfig {
            id: "claude-lab".into(),
            title: "Claude lab".into(),
            kind: RecipeKind::ClaudeCode,
            protocol: crate::config::RecipeProtocol::Anthropic,
            allowed_realms: vec!["local".into()],
            slots: vec![crate::config::RecipeSlotConfig {
                id: "primary".into(),
                title: "Primary".into(),
                target: "local/model".into(),
                trigger: crate::config::RecipeSlotTrigger::Model,
                max_output_tokens: None,
                reasoning_effort: RecipeReasoningEffort::Inherit,
            }],
            disabled: false,
        });
        catalog.models[1].provider_metadata =
            json!({ "max_context_length": CLAUDE_CODE_1M_CONTEXT_MIN_TOKENS });

        let extended = catalog.recipe_model_list(&config, &config.recipes[0]);
        assert_eq!(extended.data[0].id, "recipe:claude-lab:primary[1m]");
        assert_eq!(
            extended.data[0].max_input_tokens,
            Some(CLAUDE_CODE_1M_CONTEXT_MIN_TOKENS)
        );
        assert_eq!(extended.data[0].metadata["claude_extended_context"], true);

        catalog.models[1].provider_metadata =
            json!({ "max_context_length": CLAUDE_CODE_1M_CONTEXT_MIN_TOKENS - 1 });
        let standard = catalog.recipe_model_list(&config, &config.recipes[0]);
        assert_eq!(standard.data[0].id, "recipe:claude-lab:primary");
        assert_eq!(standard.data[0].metadata["claude_extended_context"], false);
    }

    #[test]
    fn selectable_recipe_catalog_adds_primary_but_hides_internal_slots() {
        let (mut config, catalog) = fixture();
        config.server.model_id_format = crate::config::ModelIdFormat::ColonQualified;
        config.recipes.push(RecipeConfig {
            id: "hermes-private".into(),
            title: "Hermes private".into(),
            kind: RecipeKind::Hermes,
            protocol: crate::config::RecipeProtocol::Openai,
            allowed_realms: vec!["local".into()],
            slots: vec![
                RecipeSlotConfig {
                    id: "primary".into(),
                    title: "Primary".into(),
                    target: "local/model".into(),
                    trigger: crate::config::RecipeSlotTrigger::Model,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::High,
                },
                RecipeSlotConfig {
                    id: "delegation".into(),
                    title: "Delegation".into(),
                    target: "local/model".into(),
                    trigger: crate::config::RecipeSlotTrigger::Delegation,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::None,
                },
                RecipeSlotConfig {
                    id: "compression".into(),
                    title: "Compression".into(),
                    target: "local/model".into(),
                    trigger: crate::config::RecipeSlotTrigger::Compression,
                    max_output_tokens: Some(12_000),
                    reasoning_effort: RecipeReasoningEffort::None,
                },
            ],
            disabled: false,
        });

        let list = catalog.catalog_with_selectable_recipes(&config, &[&config.recipes[0]]);
        assert!(
            list.data
                .iter()
                .any(|model| model.id == "recipe:hermes-private:primary")
        );
        let primary = list
            .data
            .iter()
            .find(|model| model.id == "recipe:hermes-private:primary")
            .unwrap();
        assert_eq!(primary.capabilities["effort"]["supported"], true);
        assert_eq!(primary.capabilities["effort"]["high"]["supported"], true);
        assert_eq!(primary.metadata["reasoning_effort_policy"], "high");
        assert!(
            !list
                .data
                .iter()
                .any(|model| model.id.contains("compression"))
        );
        assert!(
            !list
                .data
                .iter()
                .any(|model| model.id.contains("delegation"))
        );
        assert!(list.data.iter().any(|model| model.id == "local:model"));
    }

    #[test]
    fn routes_publish_stable_context_contracts_and_filter_their_pools() {
        let (mut config, mut catalog) = fixture();
        catalog.models[1].provider_metadata = json!({ "max_context_length": 131072 });
        catalog.models.push(DiscoveredModel {
            id: "local/large".into(),
            provider: "local".into(),
            provider_title: "Local".into(),
            upstream_id: "large".into(),
            management_id: None,
            loaded: None,
            realm: "local".into(),
            created: 0,
            provider_priority: 10,
            capabilities: json!({}),
            provider_metadata: json!({ "max_context_length": 262144 }),
        });
        config.routes.push(RouteConfig {
            id: "private/auto".into(),
            title: "Private Auto".into(),
            description: String::new(),
            allowed_realms: vec!["local".into()],
            candidates: vec![],
            strategy: RouteStrategy::Adaptive,
            task: RoutingTask::Auto,
            effort: RoutingEffort::Auto,
            context_length: Some(200_000),
            token_estimate_bytes_per_token: None,
            compression_max_output_tokens: None,
            on_no_eligible_model: NoEligibleModel::Deny,
        });

        let models = catalog.openai_model_list(&config);
        let fixed = models
            .data
            .iter()
            .find(|model| model.id == "route/private")
            .unwrap();
        let adaptive = models
            .data
            .iter()
            .find(|model| model.id == "route/private/auto")
            .unwrap();

        assert_eq!(fixed.max_input_tokens, Some(131072));
        assert_eq!(adaptive.max_input_tokens, Some(200_000));
        assert_eq!(adaptive.metadata["context_contract_configured"], true);
        let pool = catalog.route_context_pool(&config, &config.routes[1]);
        assert_eq!(pool.total, 2);
        assert_eq!(pool.eligible, 1);
        assert_eq!(pool.below_contract, 1);
        assert!(!models.has_more);
        assert!(models.first_id.is_some());
        assert!(models.last_id.is_some());
    }

    #[test]
    fn uncontracted_adaptive_route_never_advertises_more_than_its_smallest_model() {
        let (mut config, mut catalog) = fixture();
        catalog.models[1].provider_metadata = json!({ "max_context_length": 131072 });
        catalog.models.push(DiscoveredModel {
            id: "local/large".into(),
            provider: "local".into(),
            provider_title: "Local".into(),
            upstream_id: "large".into(),
            management_id: None,
            loaded: None,
            realm: "local".into(),
            created: 0,
            provider_priority: 10,
            capabilities: json!({}),
            provider_metadata: json!({ "max_context_length": 262144 }),
        });
        config.routes[0].strategy = RouteStrategy::Adaptive;

        let route = catalog
            .openai_model_list(&config)
            .data
            .into_iter()
            .find(|model| model.id == "route/private")
            .unwrap();
        assert_eq!(route.max_input_tokens, Some(131072));
        assert_eq!(route.metadata["context_contract_configured"], false);
    }

    #[test]
    fn physical_ids_keep_slashes_inside_upstream_model_id() {
        let (config, mut catalog) = fixture();
        catalog.models[1].id = "local/org/model".into();
        catalog.models[1].upstream_id = "org/model".into();
        let resolved = catalog.resolve(&config, "local/org/model").unwrap();
        assert_eq!(resolved.upstream_id, "org/model");
    }

    #[test]
    fn colon_ids_resolve_and_publication_sorts_by_provider_then_model() {
        let (mut config, mut catalog) = fixture();
        config.server.model_id_format = crate::config::ModelIdFormat::ColonQualified;
        config.providers[0].alias = Some("C".into());
        config.providers[1].alias = Some("L".into());
        catalog.models.push(DiscoveredModel {
            id: "cloud/second".into(),
            provider: "cloud".into(),
            provider_title: "cloud".into(),
            upstream_id: "second".into(),
            management_id: None,
            loaded: None,
            realm: "cloud".into(),
            created: 0,
            provider_priority: 1,
            capabilities: json!({}),
            provider_metadata: json!({}),
        });

        assert_eq!(
            catalog.resolve(&config, "L:model").unwrap().qualified_id,
            "local/model"
        );
        assert_eq!(
            catalog
                .resolve(&config, "local/model")
                .unwrap()
                .qualified_id,
            "local/model"
        );

        let ids = catalog
            .openai_model_list(&config)
            .data
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["C:model", "C:second", "L:model", "route:private"]);
        assert!(ids.iter().all(|id| !id.contains("%3A")));
    }

    #[test]
    fn publication_can_sort_by_model_then_provider() {
        let (mut config, mut catalog) = fixture();
        config.server.model_id_format = crate::config::ModelIdFormat::ColonQualified;
        config.server.model_order = crate::config::ModelOrder::ModelThenProvider;
        config.providers[0].alias = Some("C".into());
        config.providers[1].alias = Some("L".into());
        catalog.models.push(DiscoveredModel {
            id: "cloud/second".into(),
            provider: "cloud".into(),
            provider_title: "cloud".into(),
            upstream_id: "second".into(),
            management_id: None,
            loaded: None,
            realm: "cloud".into(),
            created: 0,
            provider_priority: 1,
            capabilities: json!({}),
            provider_metadata: json!({}),
        });

        let ids = catalog
            .openai_model_list(&config)
            .data
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["C:model", "L:model", "route:private", "C:second"]);
    }

    #[test]
    fn lm_studio_public_id_remains_stable_when_loaded_instance_has_an_alias() {
        let (mut config, mut catalog) = fixture();
        config.server.model_id_format = crate::config::ModelIdFormat::ColonQualified;
        config.providers[1].alias = Some("lms".into());
        catalog.models[1].id = "local/qwen/qwen3.5-9b".into();
        catalog.models[1].provider = "local".into();
        catalog.models[1].upstream_id = "qwen3.5-9b".into();
        catalog.models[1].management_id = Some("qwen/qwen3.5-9b".into());
        catalog.models[1].loaded = Some(true);

        let model = catalog
            .openai_model_list(&config)
            .data
            .into_iter()
            .find(|model| model.owned_by == "local")
            .unwrap();

        assert_eq!(model.id, "lms:qwen:qwen3.5-9b");
        assert_eq!(model.metadata["provider_model_id"], "qwen/qwen3.5-9b");
        assert_eq!(model.metadata["load_id"], "qwen/qwen3.5-9b");
        assert_eq!(model.metadata["inference_id"], "qwen3.5-9b");
        assert_eq!(
            catalog
                .resolve(&config, "lms:qwen:qwen3.5-9b")
                .unwrap()
                .upstream_id,
            "qwen3.5-9b"
        );
        assert_eq!(
            catalog
                .resolve(&config, "lms:qwen3.5-9b")
                .unwrap()
                .qualified_id,
            "local/qwen/qwen3.5-9b"
        );
    }

    #[test]
    fn lm_studio_native_catalog_includes_downloaded_llms_and_capabilities() {
        let provider = ProviderConfig {
            id: "lmstudio".into(),
            kind: ProviderKind::LmStudio,
            title: "LM Studio".into(),
            alias: None,
            base_url: "http://localhost:1234/v1".into(),
            realm: "local".into(),
            api_key_env: None,
            oauth_client_id: None,
            anthropic_compat: None,
            models: vec![],
            include_models: vec![],
            exclude_models: vec![],
            priority: 10,
            auto_load: true,
            jit_load: false,
            load_context_length: None,
            disabled: false,
        };
        let payload: LmStudioModelList = serde_json::from_value(json!({
            "models": [
                {
                    "type": "llm",
                    "key": "qwen/qwen3.5-9b",
                    "display_name": "Qwen3.5 9B",
                    "architecture": "qwen3_5",
                    "quantization": { "name": "8bit", "bits_per_weight": 8 },
                    "size_bytes": 10,
                    "params_string": "9B",
                    "loaded_instances": [],
                    "max_context_length": 262144,
                    "format": "mlx",
                    "capabilities": { "vision": true, "trained_for_tool_use": true }
                },
                {
                    "type": "embedding",
                    "key": "embed/model",
                    "display_name": "Embedding",
                    "loaded_instances": []
                },
                {
                    "type": "llm",
                    "key": "nova-1.0-9b",
                    "display_name": "Nova 1.0 9B",
                    "loaded_instances": [
                        {
                            "id": "nova-1.0-9b",
                            "config": { "context_length": 68000, "parallel": 1 }
                        }
                    ],
                    "max_context_length": 262144,
                    "format": "mlx"
                }
            ]
        }))
        .unwrap();

        let models = lm_studio_models(&provider, "LM Studio", payload);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "lmstudio/qwen/qwen3.5-9b");
        assert_eq!(models[0].capabilities["tools"]["support"], "native");
        assert_eq!(models[0].capabilities["vision"]["support"], "native");
        assert_eq!(models[0].provider_metadata["loaded"], false);
        assert_eq!(models[0].provider_metadata["max_context_length"], 262144);
        assert_eq!(
            models[0].provider_metadata["maximum_context_length"],
            262144
        );
        assert_eq!(models[1].id, "lmstudio/nova-1.0-9b");
        assert_eq!(models[1].provider_metadata["loaded"], true);
        assert_eq!(models[1].provider_metadata["max_context_length"], 68000);
        assert_eq!(
            models[1].provider_metadata["maximum_context_length"],
            262144
        );
        assert_eq!(
            models[1].provider_metadata["configured_context_length"],
            68000
        );
        assert_eq!(models[1].provider_metadata["configured_parallel"], 1);
    }

    #[test]
    fn llama_cpp_router_catalog_preserves_status_and_modalities() {
        let provider = ProviderConfig {
            id: "llamacpp".into(),
            kind: ProviderKind::LlamaCpp,
            title: "llama.cpp".into(),
            alias: None,
            base_url: "http://localhost:8080/v1".into(),
            realm: "local".into(),
            api_key_env: None,
            oauth_client_id: None,
            anthropic_compat: None,
            models: vec![],
            include_models: vec![],
            exclude_models: vec![],
            priority: 20,
            auto_load: false,
            jit_load: true,
            load_context_length: None,
            disabled: false,
        };
        let payload: LlamaCppModelList = serde_json::from_value(json!({
            "data": [
                {
                    "id": "org/vision-model",
                    "status": { "value": "sleeping", "args": ["llama-server", "-ctx", "8192"] },
                    "architecture": {
                        "input_modalities": ["text", "image"],
                        "output_modalities": ["text"]
                    },
                    "meta": { "n_ctx_train": 32768 }
                }
            ]
        }))
        .unwrap();

        let models = llama_cpp_models(&provider, "llama.cpp", payload);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "llamacpp/org/vision-model");
        assert_eq!(models[0].loaded, Some(false));
        assert_eq!(models[0].provider_metadata["status"], "sleeping");
        assert_eq!(models[0].capabilities["vision"]["support"], "native");
        assert_eq!(models[0].capabilities["tools"]["support"], "unknown");
    }

    #[test]
    fn xai_catalog_publishes_language_models_and_selectable_aliases() {
        let provider = ProviderConfig {
            id: "xai".into(),
            kind: ProviderKind::Xai,
            title: "xAI".into(),
            alias: None,
            base_url: String::new(),
            realm: "cloud".into(),
            api_key_env: Some("XAI_API_KEY".into()),
            oauth_client_id: None,
            anthropic_compat: None,
            models: vec![],
            include_models: vec!["grok-*".into()],
            exclude_models: vec![],
            priority: 100,
            auto_load: false,
            jit_load: false,
            load_context_length: None,
            disabled: false,
        };
        let payload: XaiLanguageModelList = serde_json::from_value(json!({
            "models": [{
                "id": "grok-4.3",
                "aliases": ["grok-latest"],
                "fingerprint": "fp_test",
                "created": 123,
                "owned_by": "xai",
                "version": "1.0",
                "input_modalities": ["text", "image"],
                "output_modalities": ["text"],
                "context_length": 131072
            }]
        }))
        .unwrap();

        let models = xai_models(&provider, "xAI", payload);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "xai/grok-4.3");
        assert_eq!(models[1].id, "xai/grok-latest");
        assert_eq!(models[1].upstream_id, "grok-latest");
        assert_eq!(models[0].capabilities["vision"]["support"], "native");
        assert_eq!(
            models[0].provider_metadata["details"]["context_length"],
            131072
        );
    }

    #[test]
    fn audio_services_resolve_only_through_their_explicit_lanes() {
        let (mut config, mut catalog) = fixture();
        config.providers.push(ProviderConfig {
            id: "speech".into(),
            kind: ProviderKind::OpenaiAudio,
            title: "Agamotto Audio".into(),
            alias: Some("audio".into()),
            base_url: "http://agamotto:5090/v1".into(),
            realm: "local".into(),
            api_key_env: None,
            oauth_client_id: None,
            anthropic_compat: None,
            models: vec![
                StaticModelConfig {
                    id: "nemotron".into(),
                    upstream_id: "parakeet".into(),
                    title: String::new(),
                    services: vec![AudioService::Transcription],
                },
                StaticModelConfig {
                    id: "tts-1".into(),
                    upstream_id: "tts-1".into(),
                    title: String::new(),
                    services: vec![AudioService::Speech],
                },
            ],
            include_models: vec![],
            exclude_models: vec![],
            priority: 1,
            auto_load: false,
            jit_load: false,
            load_context_length: None,
            disabled: false,
        });
        config.routes.push(RouteConfig {
            id: "private/transcription".into(),
            title: String::new(),
            description: String::new(),
            allowed_realms: vec!["local".into()],
            candidates: vec!["speech/nemotron".into()],
            strategy: RouteStrategy::FirstAvailable,
            task: RoutingTask::Auto,
            effort: RoutingEffort::Auto,
            context_length: None,
            token_estimate_bytes_per_token: None,
            compression_max_output_tokens: None,
            on_no_eligible_model: NoEligibleModel::Deny,
        });
        config.routes.push(RouteConfig {
            id: "private/speech".into(),
            title: String::new(),
            description: String::new(),
            allowed_realms: vec!["local".into()],
            candidates: vec!["speech/tts-1".into()],
            strategy: RouteStrategy::FirstAvailable,
            task: RoutingTask::Auto,
            effort: RoutingEffort::Auto,
            context_length: None,
            token_estimate_bytes_per_token: None,
            compression_max_output_tokens: None,
            on_no_eligible_model: NoEligibleModel::Deny,
        });
        catalog.models.push(DiscoveredModel {
            id: "speech/nemotron".into(),
            provider: "speech".into(),
            provider_title: "Parakeet".into(),
            upstream_id: "parakeet".into(),
            management_id: None,
            loaded: Some(true),
            realm: "local".into(),
            created: 0,
            provider_priority: 1,
            capabilities: json!({"transcription": {"support": "native"}}),
            provider_metadata: json!({"service": "transcription", "static": true}),
        });
        catalog.models.push(DiscoveredModel {
            id: "speech/tts-1".into(),
            provider: "speech".into(),
            provider_title: "Agamotto Audio".into(),
            upstream_id: "tts-1".into(),
            management_id: None,
            loaded: None,
            realm: "local".into(),
            created: 0,
            provider_priority: 1,
            capabilities: json!({"speech": {"support": "native"}}),
            provider_metadata: json!({"service": "speech", "static": true}),
        });

        assert!(
            !catalog
                .openai_model_list(&config)
                .data
                .iter()
                .any(|model| model.id.contains("nemotron") || model.id.contains("transcription"))
        );
        let published = config.published_model_id("speech", "nemotron");
        assert!(matches!(
            catalog.resolve(&config, &published),
            Err(ResolveError::CapabilityUnavailable { .. })
        ));
        let published_route = config
            .server
            .model_id_format
            .publish("route/private/transcription");
        let resolved = catalog
            .resolve_transcription(&config, &published_route)
            .unwrap();
        assert_eq!(resolved.qualified_id, "speech/nemotron");
        assert_eq!(resolved.upstream_id, "parakeet");
        let speech = catalog
            .resolve_audio(&config, "route/private/speech", AudioService::Speech)
            .unwrap();
        assert_eq!(speech.qualified_id, "speech/tts-1");
        assert_eq!(speech.upstream_id, "tts-1");
        let voices_target = catalog
            .resolve_default_audio(&config, AudioService::Speech)
            .unwrap();
        assert_eq!(voices_target.qualified_id, "speech/tts-1");
    }
}
