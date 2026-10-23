use std::{
    collections::{BTreeMap, HashSet},
    fs,
    net::SocketAddr,
    path::Path,
};

use anyhow::{Context, Result, bail};
use infernaut::plugin_protocol::PluginEventKind;
use serde::{Deserialize, Serialize};

fn default_listen() -> SocketAddr {
    "127.0.0.1:4040".parse().expect("valid default address")
}

fn default_refresh_seconds() -> u64 {
    15
}

fn default_realm() -> String {
    "local".to_owned()
}

fn default_priority() -> u32 {
    100
}

fn default_deny() -> NoEligibleModel {
    NoEligibleModel::Deny
}

fn default_probe_only_loaded() -> bool {
    true
}

fn default_probe_concurrency() -> usize {
    1
}

fn default_probe_idle_grace() -> u64 {
    2
}

fn default_probe_timeout() -> u64 {
    30
}

fn default_probe_max_tokens() -> u32 {
    64
}

fn default_probe_cancel_on_inference() -> bool {
    true
}

fn default_plugin_queue_capacity() -> usize {
    1024
}

fn default_routing_session_ttl_seconds() -> u64 {
    3600
}

fn default_auto_routing() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub privacy_lock: PrivacyLockConfig,
    #[serde(default)]
    pub probes: ProbeConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub clients: Vec<ClientConfig>,
    #[serde(default)]
    pub recipes: Vec<RecipeConfig>,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    #[serde(default)]
    pub aliases: Vec<ModelAliasConfig>,
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    #[serde(default)]
    pub mode: RoutingMode,
    #[serde(default)]
    pub loaded_model_preference: LoadedModelPreference,
    #[serde(default = "default_routing_session_ttl_seconds")]
    pub session_ttl_seconds: u64,
    #[serde(default)]
    pub models: Vec<RoutingModelConfig>,
    #[serde(default)]
    pub rules: Vec<RoutingRuleConfig>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            mode: RoutingMode::Off,
            loaded_model_preference: LoadedModelPreference::default(),
            session_ttl_seconds: default_routing_session_ttl_seconds(),
            models: Vec::new(),
            rules: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadedModelPreference {
    QualityFirst,
    #[default]
    Balanced,
    WarmFirst,
}

impl LoadedModelPreference {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QualityFirst => "quality_first",
            Self::Balanced => "balanced",
            Self::WarmFirst => "warm_first",
        }
    }

    pub fn bonus(self, effort: RoutingEffort) -> f64 {
        match self {
            Self::QualityFirst => 0.001,
            Self::Balanced if effort == RoutingEffort::Light => 3.0,
            Self::Balanced => 1.0,
            Self::WarmFirst if effort == RoutingEffort::Light => 8.0,
            Self::WarmFirst => 3.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    #[default]
    Off,
    Shadow,
    Active,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RoutingModelRole {
    Fast,
    Inspect,
    Balanced,
    Heavy,
    Vision,
    Compression,
    Transcription,
    Speech,
}

impl RoutingModelRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Inspect => "inspect",
            Self::Balanced => "balanced",
            Self::Heavy => "heavy",
            Self::Vision => "vision",
            Self::Compression => "compression",
            Self::Transcription => "transcription",
            Self::Speech => "speech",
        }
    }

    pub fn is_additive(self) -> bool {
        matches!(self, Self::Vision | Self::Compression)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingModelConfig {
    pub model: String,
    /// Operator-authoritative context window for this physical model. This
    /// corrects incomplete provider discovery; it does not reconfigure the
    /// upstream runtime.
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default = "default_auto_routing")]
    pub auto_routing: bool,
    #[serde(default)]
    pub roles: Vec<RoutingModelRole>,
    #[serde(default)]
    pub strengths: BTreeMap<String, f64>,
    #[serde(default)]
    pub traits: RoutingModelTraits,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RoutingModelTraits {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub latency_tier: u8,
    #[serde(default)]
    pub cost_tier: u8,
    #[serde(default)]
    pub preferred_effort: Vec<RoutingEffort>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingRuleConfig {
    pub route: Option<String>,
    pub task: Option<RoutingTask>,
    pub operation: Option<RoutingOperation>,
    pub effort: Option<RoutingEffort>,
    #[serde(default)]
    pub prefer: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteStrategy {
    #[default]
    FirstAvailable,
    Adaptive,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingTask {
    #[default]
    Auto,
    Code,
    Data,
    Chat,
}

impl RoutingTask {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Code => "code",
            Self::Data => "data",
            Self::Chat => "chat",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingOperation {
    #[default]
    Converse,
    Inspect,
    Review,
    Debug,
    Modify,
    Implement,
    Architect,
    Migrate,
    Analyze,
    Summarize,
}

impl RoutingOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Converse => "converse",
            Self::Inspect => "inspect",
            Self::Review => "review",
            Self::Debug => "debug",
            Self::Modify => "modify",
            Self::Implement => "implement",
            Self::Architect => "architect",
            Self::Migrate => "migrate",
            Self::Analyze => "analyze",
            Self::Summarize => "summarize",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingEffort {
    #[default]
    Auto,
    Light,
    Standard,
    Heavy,
}

impl RoutingEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Light => "light",
            Self::Standard => "standard",
            Self::Heavy => "heavy",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    pub id: String,
    #[serde(default)]
    pub mode: PluginMode,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub events: Vec<PluginEventKind>,
    #[serde(default = "default_plugin_queue_capacity")]
    pub queue_capacity: usize,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginMode {
    #[default]
    Observe,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProbeConfig {
    #[serde(default)]
    pub mode: ProbeMode,
    #[serde(default = "default_probe_only_loaded")]
    pub only_loaded_models: bool,
    #[serde(default = "default_probe_concurrency")]
    pub max_concurrency: usize,
    #[serde(default = "default_probe_idle_grace")]
    pub idle_grace_seconds: u64,
    #[serde(default = "default_probe_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_probe_max_tokens")]
    pub max_output_tokens: u32,
    #[serde(default = "default_probe_cancel_on_inference")]
    pub cancel_on_inference: bool,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            mode: ProbeMode::Manual,
            only_loaded_models: default_probe_only_loaded(),
            max_concurrency: default_probe_concurrency(),
            idle_grace_seconds: default_probe_idle_grace(),
            timeout_seconds: default_probe_timeout(),
            max_output_tokens: default_probe_max_tokens(),
            cancel_on_inference: default_probe_cancel_on_inference(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeMode {
    Off,
    #[default]
    Manual,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PrivacyLockConfig {
    #[serde(default)]
    pub mode: PrivacyMode,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyMode {
    #[default]
    Off,
    Selective,
    LocalOnly,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub model_id_format: ModelIdFormat,
    #[serde(default)]
    pub model_order: ModelOrder,
    #[serde(default = "default_refresh_seconds")]
    pub catalog_refresh_seconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub api_key_env: String,
    /// Optional request-dispatch recipe selected by this client's API key.
    pub recipe: Option<String>,
    /// Optional recipe models published alongside the ordinary catalog. The
    /// request model selects one of these recipes per harness session.
    #[serde(default)]
    pub recipes: Vec<String>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecipeKind {
    #[default]
    Generic,
    ClaudeCode,
    Hermes,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecipeProtocol {
    #[default]
    Any,
    Openai,
    Anthropic,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecipeSlotTrigger {
    #[default]
    Model,
    Classifier,
    Compression,
    Delegation,
    Vision,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecipeReasoningEffort {
    #[default]
    Inherit,
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Ultra,
}

impl RecipeReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }

    pub fn request_value(self) -> Option<&'static str> {
        (self != Self::Inherit).then(|| self.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeConfig {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind: RecipeKind,
    #[serde(default)]
    pub protocol: RecipeProtocol,
    #[serde(default)]
    pub allowed_realms: Vec<String>,
    #[serde(default)]
    pub slots: Vec<RecipeSlotConfig>,
    #[serde(default)]
    pub disabled: bool,
}

impl RecipeConfig {
    pub fn display_name(&self) -> &str {
        if self.title.trim().is_empty() {
            &self.id
        } else {
            &self.title
        }
    }

    pub fn qualified_slot_id(&self, slot: &RecipeSlotConfig) -> String {
        format!("recipe/{}/{}", self.id, slot.id)
    }

    pub fn slot_for_request<'a>(&'a self, requested: &str) -> Option<&'a RecipeSlotConfig> {
        // Claude Code uses a `[1m]` model-name suffix to opt into its extended
        // context budget. Keep that transport-level marker out of the stable
        // recipe slot identity; the catalog and request path separately verify
        // that the resolved target genuinely supports the extended window.
        let requested = if self.kind == RecipeKind::ClaudeCode {
            requested.strip_suffix("[1m]").unwrap_or(requested)
        } else {
            requested
        };
        self.slots.iter().find(|slot| {
            let canonical = self.qualified_slot_id(slot);
            requested == canonical || requested == canonical.replace('/', ":")
        })
    }

    pub fn compression_slot(&self) -> Option<&RecipeSlotConfig> {
        self.slots
            .iter()
            .find(|slot| slot.trigger == RecipeSlotTrigger::Compression)
    }

    pub fn classifier_slot(&self) -> Option<&RecipeSlotConfig> {
        self.slots
            .iter()
            .find(|slot| slot.trigger == RecipeSlotTrigger::Classifier)
    }

    pub fn delegation_slot(&self) -> Option<&RecipeSlotConfig> {
        self.slots
            .iter()
            .find(|slot| slot.trigger == RecipeSlotTrigger::Delegation)
    }

    pub fn vision_slot(&self) -> Option<&RecipeSlotConfig> {
        self.slots
            .iter()
            .find(|slot| slot.trigger == RecipeSlotTrigger::Vision)
    }

    pub fn selectable_slots(&self) -> impl Iterator<Item = &RecipeSlotConfig> {
        self.slots
            .iter()
            .filter(|slot| slot.trigger == RecipeSlotTrigger::Model)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeSlotConfig {
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub target: String,
    #[serde(default)]
    pub trigger: RecipeSlotTrigger,
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_effort: RecipeReasoningEffort,
}

impl RecipeSlotConfig {
    pub fn display_name(&self) -> &str {
        if self.title.trim().is_empty() {
            &self.id
        } else {
            &self.title
        }
    }
}

impl ClientConfig {
    pub fn display_name(&self) -> &str {
        if self.title.trim().is_empty() {
            &self.id
        } else {
            &self.title
        }
    }

    pub fn authorizes_recipe(&self, recipe_id: &str) -> bool {
        self.recipe.as_deref() == Some(recipe_id)
            || self
                .recipes
                .iter()
                .any(|configured| configured == recipe_id)
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            api_key_env: None,
            model_id_format: ModelIdFormat::default(),
            model_order: ModelOrder::default(),
            catalog_refresh_seconds: default_refresh_seconds(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelOrder {
    #[default]
    ProviderThenModel,
    ModelThenProvider,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelIdFormat {
    #[default]
    Slash,
    ColonQualified,
}

impl ModelIdFormat {
    pub fn publish(self, canonical: &str) -> String {
        match self {
            Self::Slash => canonical.to_owned(),
            Self::ColonQualified => canonical.replace('/', ":"),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    #[serde(default)]
    pub kind: ProviderKind,
    #[serde(default)]
    pub title: String,
    pub alias: Option<String>,
    #[serde(default)]
    pub base_url: String,
    #[serde(default = "default_realm")]
    pub realm: String,
    pub api_key_env: Option<String>,
    pub oauth_client_id: Option<String>,
    /// Whether this provider accepts Anthropic-compatible `/v1/messages`
    /// requests natively. LM Studio, llama.cpp, and vLLM enable this
    /// automatically; generic OpenAI-compatible runtimes must opt in.
    pub anthropic_compat: Option<bool>,
    #[serde(default)]
    pub include_models: Vec<String>,
    #[serde(default)]
    pub exclude_models: Vec<String>,
    /// Statically declared endpoint-only models. V1 uses this for services
    /// such as OpenAI-compatible transcription servers that intentionally do
    /// not publish a `/v1/models` catalog.
    #[serde(default)]
    pub models: Vec<StaticModelConfig>,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default)]
    pub auto_load: bool,
    #[serde(default)]
    pub jit_load: bool,
    pub load_context_length: Option<u64>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    OpenaiCompatible,
    LmStudio,
    LlamaCpp,
    Vllm,
    Openai,
    Xai,
    OllamaCloud,
    OpenaiCodex,
    XaiOauth,
    OpenaiTranscription,
    OpenaiAudio,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AudioService {
    Transcription,
    Speech,
}

impl AudioService {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transcription => "transcription",
            Self::Speech => "speech",
        }
    }

    pub fn routing_role(self) -> RoutingModelRole {
        match self {
            Self::Transcription => RoutingModelRole::Transcription,
            Self::Speech => RoutingModelRole::Speech,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StaticModelConfig {
    pub id: String,
    #[serde(default)]
    pub upstream_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub services: Vec<AudioService>,
}

impl StaticModelConfig {
    pub fn upstream_id(&self) -> &str {
        if self.upstream_id.trim().is_empty() {
            &self.id
        } else {
            &self.upstream_id
        }
    }

    pub fn display_name(&self) -> &str {
        if self.title.trim().is_empty() {
            &self.id
        } else {
            &self.title
        }
    }

    pub fn supports_audio_service(&self, kind: ProviderKind, service: AudioService) -> bool {
        if self.services.is_empty() {
            kind == ProviderKind::OpenaiTranscription && service == AudioService::Transcription
        } else {
            self.services.contains(&service)
        }
    }
}

impl ProviderConfig {
    pub fn is_audio_provider(&self) -> bool {
        matches!(
            self.kind,
            ProviderKind::OpenaiTranscription | ProviderKind::OpenaiAudio
        )
    }

    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            &self.id
        } else {
            &self.title
        }
    }

    pub fn display_namespace(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.id)
    }

    pub fn api_key(&self) -> Option<String> {
        self.api_key_env
            .as_ref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.is_empty())
    }

    pub fn supports_anthropic_messages(&self) -> bool {
        self.anthropic_compat.unwrap_or(matches!(
            self.kind,
            ProviderKind::LmStudio
                | ProviderKind::LlamaCpp
                | ProviderKind::Vllm
                | ProviderKind::OllamaCloud
        ))
    }

    pub fn supports_native_anthropic_token_counting(&self) -> bool {
        matches!(self.kind, ProviderKind::LlamaCpp | ProviderKind::Vllm)
    }

    pub fn effective_base_url(&self) -> &str {
        if !self.base_url.is_empty() {
            &self.base_url
        } else {
            match self.kind {
                ProviderKind::Openai => "https://api.openai.com/v1",
                ProviderKind::Xai => "https://api.x.ai/v1",
                ProviderKind::OllamaCloud => "https://ollama.com/v1",
                ProviderKind::OpenaiCodex => "https://chatgpt.com/backend-api/codex",
                ProviderKind::XaiOauth => "https://api.x.ai/v1",
                _ => "",
            }
        }
    }

    pub fn model_allowed(&self, model: &str) -> bool {
        (self.include_models.is_empty()
            || self
                .include_models
                .iter()
                .any(|pattern| wildcard_matches(pattern, model)))
            && !self
                .exclude_models
                .iter()
                .any(|pattern| wildcard_matches(pattern, model))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct RouteConfig {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub allowed_realms: Vec<String>,
    #[serde(default)]
    pub candidates: Vec<String>,
    #[serde(default)]
    pub strategy: RouteStrategy,
    #[serde(default)]
    pub task: RoutingTask,
    #[serde(default)]
    pub effort: RoutingEffort,
    /// Stable context window advertised by this virtual route. Models must
    /// report at least this capacity to enter the route's eligible pool.
    #[serde(default)]
    pub context_length: Option<u64>,
    /// Optional tokenizer-independent input estimator override. Larger values
    /// admit more input bytes per estimated token; the default is 3.0.
    #[serde(default)]
    pub token_estimate_bytes_per_token: Option<f64>,
    /// Optional maximum output allowance for detected compression requests.
    /// This is used by adaptive-route admission and forwarded to the provider
    /// in place of a larger harness-supplied maximum.
    #[serde(default)]
    pub compression_max_output_tokens: Option<u64>,
    #[serde(default = "default_deny")]
    pub on_no_eligible_model: NoEligibleModel,
}

impl RouteConfig {
    pub fn qualified_id(&self) -> String {
        format!("route/{}", self.id)
    }
}

/// A stable, operator-owned model pointer. Unlike a route, an alias never
/// classifies, ranks, or falls back: it resolves to exactly one configured
/// physical model or virtual route and fails closed when that target cannot
/// serve the request.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAliasConfig {
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub target: String,
    /// Optional stable context contract advertised for this alias. The target
    /// must report at least this capacity and requests may not exceed it.
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub disabled: bool,
}

impl ModelAliasConfig {
    pub fn qualified_id(&self) -> String {
        format!("alias/{}", self.id)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoEligibleModel {
    #[default]
    Deny,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration at {}", path.display()))?;
        let mut config: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse configuration at {}", path.display()))?;
        config.migrate_legacy_recipes();
        config.validate()?;
        Ok(config)
    }

    /// Upgrade recipe schemas without requiring operators to hand-edit a live
    /// configuration. Claude's original classifier slot was actually the Haiku
    /// alias. Hermes' first recipe template predated delegation dispatch. New
    /// slots inherit an existing safe target and remain staged for explicit TUI
    /// review rather than silently selecting a different provider.
    pub fn migrate_legacy_recipes(&mut self) -> bool {
        let mut changed = false;
        for recipe in &mut self.recipes {
            if recipe.kind == RecipeKind::ClaudeCode
                && !recipe.slots.iter().any(|slot| slot.id == "background")
                && let Some(classifier_index) = recipe.slots.iter().position(|slot| {
                    slot.id == "classifier" && slot.trigger == RecipeSlotTrigger::Model
                })
            {
                let background_target = recipe
                    .slots
                    .iter()
                    .find(|slot| slot.id == "subagent")
                    .or_else(|| recipe.slots.iter().find(|slot| slot.id == "primary"))
                    .map(|slot| slot.target.clone())
                    .unwrap_or_else(|| recipe.slots[classifier_index].target.clone());
                recipe.slots[classifier_index].title = "Classifier".to_owned();
                recipe.slots[classifier_index].trigger = RecipeSlotTrigger::Classifier;
                recipe.slots[classifier_index]
                    .max_output_tokens
                    .get_or_insert(1024);
                recipe.slots.insert(
                    classifier_index,
                    RecipeSlotConfig {
                        id: "background".to_owned(),
                        title: "Background / Haiku".to_owned(),
                        target: background_target,
                        trigger: RecipeSlotTrigger::Model,
                        max_output_tokens: None,
                        reasoning_effort: RecipeReasoningEffort::Inherit,
                    },
                );
                changed = true;
            }

            if recipe.kind == RecipeKind::Hermes && recipe.delegation_slot().is_none() {
                let Some(target) = recipe
                    .slots
                    .iter()
                    .find(|slot| slot.id == "primary")
                    .or_else(|| recipe.selectable_slots().next())
                    .map(|slot| slot.target.clone())
                else {
                    continue;
                };
                let insertion = recipe
                    .slots
                    .iter()
                    .position(|slot| slot.trigger == RecipeSlotTrigger::Compression)
                    .unwrap_or(recipe.slots.len());
                recipe.slots.insert(
                    insertion,
                    RecipeSlotConfig {
                        id: "delegation".to_owned(),
                        title: "Delegation / subagent".to_owned(),
                        target,
                        trigger: RecipeSlotTrigger::Delegation,
                        max_output_tokens: None,
                        reasoning_effort: RecipeReasoningEffort::None,
                    },
                );
                changed = true;
            }

            if recipe.kind == RecipeKind::Hermes && recipe.vision_slot().is_none() {
                let Some(target) = recipe
                    .slots
                    .iter()
                    .find(|slot| slot.id == "primary")
                    .or_else(|| recipe.selectable_slots().next())
                    .map(|slot| slot.target.clone())
                else {
                    continue;
                };
                let insertion = recipe
                    .slots
                    .iter()
                    .position(|slot| slot.trigger == RecipeSlotTrigger::Compression)
                    .unwrap_or(recipe.slots.len());
                recipe.slots.insert(
                    insertion,
                    RecipeSlotConfig {
                        id: "vision".to_owned(),
                        title: "Vision analysis".to_owned(),
                        target,
                        trigger: RecipeSlotTrigger::Vision,
                        max_output_tokens: None,
                        reasoning_effort: RecipeReasoningEffort::None,
                    },
                );
                changed = true;
            }
        }
        changed
    }

    pub fn validate(&self) -> Result<()> {
        if self.providers.is_empty() {
            bail!("at least one provider must be configured");
        }
        if self.probes.max_concurrency != 1 {
            bail!("V1 probes.max_concurrency must be 1");
        }
        if self.probes.timeout_seconds == 0 || self.probes.max_output_tokens == 0 {
            bail!("probe timeout and token limits must be greater than zero");
        }
        let mut plugin_ids = HashSet::new();
        for plugin in &self.plugins {
            validate_namespace_id("plugin", &plugin.id)?;
            if !plugin_ids.insert(plugin.id.clone()) {
                bail!("duplicate plugin id '{}'", plugin.id);
            }
            if plugin.command.trim().is_empty() {
                bail!("plugin '{}' has an empty command", plugin.id);
            }
            if plugin.queue_capacity == 0 {
                bail!(
                    "plugin '{}' queue_capacity must be greater than zero",
                    plugin.id
                );
            }
            if plugin.events.is_empty() && !plugin.disabled {
                bail!(
                    "plugin '{}' must subscribe to at least one event",
                    plugin.id
                );
            }
            let mut events = HashSet::new();
            for event in &plugin.events {
                if !events.insert(*event) {
                    bail!("plugin '{}' subscribes to a duplicate event", plugin.id);
                }
            }
            if plugin.mode != PluginMode::Observe {
                bail!("plugin '{}' requests an unsupported mode", plugin.id);
            }
        }

        let mut recipe_ids = HashSet::new();
        for recipe in &self.recipes {
            validate_namespace_id("recipe", &recipe.id)?;
            if !recipe_ids.insert(recipe.id.clone()) {
                bail!("duplicate recipe id '{}'", recipe.id);
            }
            if recipe.slots.is_empty() {
                bail!("recipe '{}' must configure at least one slot", recipe.id);
            }
            if recipe.kind == RecipeKind::ClaudeCode && recipe.protocol != RecipeProtocol::Anthropic
            {
                bail!(
                    "Claude Code recipe '{}' must set protocol = 'anthropic'",
                    recipe.id
                );
            }
            if recipe.kind == RecipeKind::Hermes && recipe.protocol != RecipeProtocol::Openai {
                bail!("Hermes recipe '{}' must set protocol = 'openai'", recipe.id);
            }
            for realm in &recipe.allowed_realms {
                if !matches!(realm.as_str(), "local" | "cloud") {
                    bail!(
                        "recipe '{}' has invalid allowed realm '{}'; expected local or cloud",
                        recipe.id,
                        realm
                    );
                }
            }
            let mut slot_ids = HashSet::new();
            let mut classifier_slots = 0;
            let mut compression_slots = 0;
            let mut delegation_slots = 0;
            let mut vision_slots = 0;
            for slot in &recipe.slots {
                validate_namespace_id("recipe slot", &slot.id)?;
                if !slot_ids.insert(slot.id.clone()) {
                    bail!("recipe '{}' has duplicate slot '{}'", recipe.id, slot.id);
                }
                if slot.target.trim().is_empty() {
                    bail!(
                        "recipe '{}' slot '{}' has an empty target",
                        recipe.id,
                        slot.id
                    );
                }
                if slot.max_output_tokens == Some(0) {
                    bail!(
                        "recipe '{}' slot '{}' max_output_tokens must be greater than zero",
                        recipe.id,
                        slot.id
                    );
                }
                if recipe.protocol == RecipeProtocol::Anthropic
                    && slot.reasoning_effort != RecipeReasoningEffort::Inherit
                {
                    bail!(
                        "Anthropic recipe '{}' slot '{}' cannot set reasoning_effort; preserve the harness thinking object with 'inherit'",
                        recipe.id,
                        slot.id
                    );
                }
                if slot.trigger == RecipeSlotTrigger::Compression {
                    compression_slots += 1;
                }
                if slot.trigger == RecipeSlotTrigger::Classifier {
                    classifier_slots += 1;
                }
                if slot.trigger == RecipeSlotTrigger::Delegation {
                    delegation_slots += 1;
                }
                if slot.trigger == RecipeSlotTrigger::Vision {
                    vision_slots += 1;
                }
            }
            if classifier_slots > 1 {
                bail!(
                    "recipe '{}' may configure only one classifier slot",
                    recipe.id
                );
            }
            if compression_slots > 1 {
                bail!(
                    "recipe '{}' may configure only one compression slot",
                    recipe.id
                );
            }
            if delegation_slots > 1 {
                bail!(
                    "recipe '{}' may configure only one delegation slot",
                    recipe.id
                );
            }
            if vision_slots > 1 {
                bail!("recipe '{}' may configure only one vision slot", recipe.id);
            }
            if recipe.selectable_slots().next().is_none() {
                bail!(
                    "recipe '{}' must configure at least one model-triggered selectable slot",
                    recipe.id
                );
            }
        }

        let mut client_ids = HashSet::new();
        let mut client_key_environments = HashSet::new();
        for client in &self.clients {
            validate_namespace_id("client", &client.id)?;
            if !client_ids.insert(client.id.clone()) {
                bail!("duplicate client id '{}'", client.id);
            }
            if client.api_key_env.trim().is_empty() {
                bail!("client '{}' has an empty api_key_env", client.id);
            }
            if !client_key_environments.insert(client.api_key_env.clone()) {
                bail!(
                    "client '{}' reuses api_key_env '{}'; each named client needs a distinct key",
                    client.id,
                    client.api_key_env
                );
            }
            if let Some(recipe) = &client.recipe
                && !recipe_ids.contains(recipe)
            {
                bail!(
                    "client '{}' references unknown recipe '{}'",
                    client.id,
                    recipe
                );
            }
            if client.recipe.is_some() && !client.recipes.is_empty() {
                bail!(
                    "client '{}' cannot combine exclusive recipe with selectable recipes",
                    client.id
                );
            }
            let mut selectable_recipe_ids = HashSet::new();
            for recipe_id in &client.recipes {
                if !selectable_recipe_ids.insert(recipe_id) {
                    bail!(
                        "client '{}' repeats selectable recipe '{}'",
                        client.id,
                        recipe_id
                    );
                }
                if !recipe_ids.contains(recipe_id) {
                    bail!(
                        "client '{}' references unknown selectable recipe '{}'",
                        client.id,
                        recipe_id
                    );
                }
                if self
                    .recipes
                    .iter()
                    .any(|recipe| recipe.id == *recipe_id && recipe.disabled)
                {
                    bail!(
                        "client '{}' references disabled selectable recipe '{}'",
                        client.id,
                        recipe_id
                    );
                }
            }
            if let Some(recipe_id) = &client.recipe
                && self
                    .recipes
                    .iter()
                    .any(|recipe| recipe.id == *recipe_id && recipe.disabled)
            {
                bail!(
                    "client '{}' references disabled recipe '{}'",
                    client.id,
                    recipe_id
                );
            }
        }

        let mut provider_ids: HashSet<String> = HashSet::new();
        let mut provider_aliases: HashSet<String> = HashSet::new();
        for provider in &self.providers {
            validate_namespace_id("provider", &provider.id)?;
            if matches!(provider.id.as_str(), "route" | "recipe" | "alias") {
                bail!("provider id '{}' is reserved", provider.id);
            }
            if !provider_ids.insert(provider.id.clone()) {
                bail!("duplicate provider id '{}'", provider.id);
            }
            validate_namespace_id("provider alias", provider.display_namespace())?;
            if matches!(provider.display_namespace(), "route" | "recipe" | "alias") {
                bail!(
                    "provider alias '{}' is reserved",
                    provider.display_namespace()
                );
            }
            if !provider_aliases.insert(provider.display_namespace().to_owned()) {
                bail!(
                    "duplicate provider alias '{}'",
                    provider.display_namespace()
                );
            }
            if provider.effective_base_url().trim().is_empty() {
                bail!("provider '{}' has an empty base_url", provider.id);
            }
            if matches!(
                provider.kind,
                ProviderKind::Openai
                    | ProviderKind::Xai
                    | ProviderKind::OllamaCloud
                    | ProviderKind::OpenaiCodex
                    | ProviderKind::XaiOauth
            ) && provider.realm != "cloud"
            {
                bail!(
                    "provider '{}' uses a cloud provider kind and must set realm = 'cloud'",
                    provider.id
                );
            }
            if matches!(
                provider.kind,
                ProviderKind::Openai | ProviderKind::Xai | ProviderKind::OllamaCloud
            ) && provider.api_key_env.is_none()
            {
                bail!(
                    "provider '{}' must name an api_key_env for cloud authentication",
                    provider.id
                );
            }
            if provider.kind == ProviderKind::XaiOauth
                && provider.oauth_client_id.as_deref().unwrap_or("").is_empty()
            {
                bail!(
                    "provider '{}' must set oauth_client_id to a client registered for Infernaut with xAI",
                    provider.id
                );
            }
            for pattern in provider
                .include_models
                .iter()
                .chain(&provider.exclude_models)
            {
                if pattern.is_empty() {
                    bail!("provider '{}' contains an empty model filter", provider.id);
                }
            }
            if provider.auto_load && provider.jit_load {
                bail!(
                    "provider '{}' cannot enable both auto_load and jit_load",
                    provider.id
                );
            }
            if provider.is_audio_provider() {
                if provider.models.is_empty() {
                    bail!(
                        "audio provider '{}' must declare at least one static model",
                        provider.id
                    );
                }
                if provider.auto_load || provider.jit_load {
                    bail!(
                        "audio provider '{}' cannot use LLM model-loading controls",
                        provider.id
                    );
                }
                let mut model_ids = HashSet::new();
                for model in &provider.models {
                    if model.id.trim().is_empty() || model.id.contains('/') {
                        bail!(
                            "audio provider '{}' static model id '{}' must be non-empty and contain no '/'",
                            provider.id,
                            model.id
                        );
                    }
                    if model.upstream_id().trim().is_empty() {
                        bail!(
                            "audio provider '{}' model '{}' has an empty upstream_id",
                            provider.id,
                            model.id
                        );
                    }
                    if !model_ids.insert(model.id.clone()) {
                        bail!(
                            "audio provider '{}' contains duplicate static model '{}'",
                            provider.id,
                            model.id
                        );
                    }
                    let unique_services = model.services.iter().copied().collect::<HashSet<_>>();
                    if unique_services.len() != model.services.len() {
                        bail!(
                            "audio provider '{}' model '{}' contains duplicate services",
                            provider.id,
                            model.id
                        );
                    }
                    if provider.kind == ProviderKind::OpenaiAudio && model.services.len() != 1 {
                        bail!(
                            "audio provider '{}' model '{}' must declare exactly one service",
                            provider.id,
                            model.id
                        );
                    }
                    if provider.kind == ProviderKind::OpenaiTranscription
                        && (!model.services.is_empty()
                            && model.services != [AudioService::Transcription])
                    {
                        bail!(
                            "legacy transcription provider '{}' model '{}' may only use service = 'transcription'",
                            provider.id,
                            model.id
                        );
                    }
                }
            } else if !provider.models.is_empty() {
                bail!(
                    "provider '{}' declares static models but kind is not an audio service provider",
                    provider.id
                );
            }
        }

        for provider in &self.providers {
            if provider.display_namespace() != provider.id
                && provider_ids.contains(provider.display_namespace())
            {
                bail!(
                    "provider alias '{}' conflicts with provider id '{}'",
                    provider.display_namespace(),
                    provider.display_namespace()
                );
            }
        }

        if self.routing.session_ttl_seconds == 0 {
            bail!("routing.session_ttl_seconds must be greater than zero");
        }
        for model in &self.routing.models {
            if model.model.trim().is_empty() || !model.model.contains('/') {
                bail!(
                    "routing model matcher '{}' must be provider-qualified",
                    model.model
                );
            }
            for (strength, value) in &model.strengths {
                if strength.trim().is_empty() || !value.is_finite() || !(0.0..=1.0).contains(value)
                {
                    bail!(
                        "routing model '{}' has invalid strength '{}' = {}; strengths must be between 0 and 1",
                        model.model,
                        strength,
                        value
                    );
                }
            }
            if model.traits.latency_tier > 4 || model.traits.cost_tier > 4 {
                bail!(
                    "routing model '{}' latency_tier and cost_tier must be between 0 and 4",
                    model.model
                );
            }
            if model.context_length == Some(0) {
                bail!(
                    "routing model '{}' context_length must be greater than zero",
                    model.model
                );
            }
            if model.traits.preferred_effort.contains(&RoutingEffort::Auto) {
                bail!(
                    "routing model '{}' preferred_effort cannot contain 'auto'",
                    model.model
                );
            }
            let unique_roles = model.roles.iter().copied().collect::<HashSet<_>>();
            if unique_roles.len() != model.roles.len() {
                bail!("routing model '{}' contains duplicate roles", model.model);
            }
            if model
                .roles
                .iter()
                .filter(|role| !role.is_additive())
                .count()
                > 1
            {
                bail!(
                    "routing model '{}' may have one workload role plus additive vision and compression roles",
                    model.model
                );
            }
            if model.roles.iter().any(|role| {
                matches!(
                    role,
                    RoutingModelRole::Transcription | RoutingModelRole::Speech
                )
            }) && model.roles.len() != 1
            {
                bail!(
                    "routing model '{}' uses an exclusive audio service role and cannot combine it with LLM roles",
                    model.model
                );
            }
            if model.roles.contains(&RoutingModelRole::Transcription)
                && !self.is_static_audio_model(&model.model, AudioService::Transcription)
            {
                bail!(
                    "routing model '{}' uses the transcription role but is not a declared static transcription model",
                    model.model
                );
            }
            if model.roles.contains(&RoutingModelRole::Speech)
                && !self.is_static_audio_model(&model.model, AudioService::Speech)
            {
                bail!(
                    "routing model '{}' uses the speech role but is not a declared static speech model",
                    model.model
                );
            }
        }

        for rule in &self.routing.rules {
            if rule.prefer.is_empty() {
                bail!("routing rules must prefer at least one model");
            }
            if matches!(rule.task, Some(RoutingTask::Auto))
                || matches!(rule.effort, Some(RoutingEffort::Auto))
            {
                bail!("routing rule task and effort filters cannot be 'auto'");
            }
            for candidate in &rule.prefer {
                validate_candidate_provider("routing rule", candidate, &provider_ids)?;
            }
        }

        let mut route_ids = HashSet::new();
        for route in &self.routes {
            validate_route_id(&route.id)?;
            if !route_ids.insert(&route.id) {
                bail!("duplicate route id '{}'", route.id);
            }
            if route.context_length == Some(0) {
                bail!(
                    "route '{}' context_length must be greater than zero",
                    route.id
                );
            }
            if let Some(bytes_per_token) = route.token_estimate_bytes_per_token
                && (!bytes_per_token.is_finite() || !(1.0..=8.0).contains(&bytes_per_token))
            {
                bail!(
                    "route '{}' token_estimate_bytes_per_token must be between 1.0 and 8.0",
                    route.id
                );
            }
            if route.compression_max_output_tokens == Some(0) {
                bail!(
                    "route '{}' compression_max_output_tokens must be greater than zero",
                    route.id
                );
            }
            if route.compression_max_output_tokens.is_some()
                && route.strategy != RouteStrategy::Adaptive
            {
                bail!(
                    "route '{}' sets compression_max_output_tokens but does not use strategy = 'adaptive'",
                    route.id
                );
            }
            for candidate in &route.candidates {
                validate_candidate_provider(
                    &format!("route '{}'", route.id),
                    candidate,
                    &provider_ids,
                )?;
                let provider_id = candidate
                    .split_once('/')
                    .map(|(provider, _)| provider)
                    .unwrap_or_default();
                if self
                    .providers
                    .iter()
                    .any(|provider| provider.id == provider_id && provider.is_audio_provider())
                    && self.static_audio_service(candidate).is_none()
                {
                    bail!(
                        "route '{}' references an undeclared or ambiguous audio model '{}'",
                        route.id,
                        candidate
                    );
                }
            }
            let audio_services = route
                .candidates
                .iter()
                .filter_map(|candidate| self.static_audio_service(candidate))
                .collect::<HashSet<_>>();
            let audio_candidates = route
                .candidates
                .iter()
                .filter(|candidate| self.static_audio_service(candidate).is_some())
                .count();
            if audio_candidates > 0 && audio_candidates != route.candidates.len() {
                bail!(
                    "route '{}' mixes audio services with ordinary inference models",
                    route.id
                );
            }
            if audio_services.len() > 1 {
                bail!("route '{}' mixes different audio service types", route.id);
            }
            if audio_candidates > 0 && route.strategy != RouteStrategy::FirstAvailable {
                bail!(
                    "audio route '{}' must use strategy = 'first_available'",
                    route.id
                );
            }
            if route.strategy == RouteStrategy::FirstAvailable
                && (route.task != RoutingTask::Auto || route.effort != RoutingEffort::Auto)
            {
                bail!(
                    "route '{}' sets task or effort but does not use strategy = 'adaptive'",
                    route.id
                );
            }
        }
        for rule in &self.routing.rules {
            if let Some(route) = &rule.route
                && !self.routes.iter().any(|configured| configured.id == *route)
            {
                bail!("routing rule references unknown route '{route}'");
            }
        }
        let mut alias_ids = HashSet::new();
        for alias in &self.aliases {
            validate_route_id(&alias.id)?;
            if !alias_ids.insert(&alias.id) {
                bail!("duplicate alias id '{}'", alias.id);
            }
            if alias.context_length == Some(0) {
                bail!(
                    "alias '{}' context_length must be greater than zero",
                    alias.id
                );
            }
            if let Some(recipe_target) = alias.target.strip_prefix("recipe/") {
                let Some((recipe_id, slot_id)) = recipe_target.split_once('/') else {
                    bail!(
                        "alias '{}' has invalid recipe target '{}'",
                        alias.id,
                        alias.target
                    );
                };
                let Some(recipe) = self.recipes.iter().find(|recipe| recipe.id == recipe_id) else {
                    bail!(
                        "alias '{}' references unknown recipe '{}'",
                        alias.id,
                        recipe_id
                    );
                };
                let Some(slot) = recipe.slots.iter().find(|slot| slot.id == slot_id) else {
                    bail!(
                        "alias '{}' references unknown recipe slot '{}'",
                        alias.id,
                        alias.target
                    );
                };
                if slot.trigger != RecipeSlotTrigger::Model {
                    bail!(
                        "alias '{}' may target only a selectable model-triggered recipe slot",
                        alias.id
                    );
                }
            } else if let Some(route_id) = alias.target.strip_prefix("route/") {
                if !self.routes.iter().any(|route| route.id == route_id) {
                    bail!(
                        "alias '{}' references unknown route '{}'",
                        alias.id,
                        route_id
                    );
                }
                if self.routes.iter().any(|route| {
                    route.id == route_id
                        && (self.static_audio_service_for_route(route, AudioService::Transcription)
                            || self.static_audio_service_for_route(route, AudioService::Speech))
                }) {
                    bail!("alias '{}' cannot target an audio route", alias.id);
                }
            } else {
                validate_candidate_provider(
                    &format!("alias '{}'", alias.id),
                    &alias.target,
                    &provider_ids,
                )?;
                if self.static_audio_service(&alias.target).is_some() {
                    bail!("alias '{}' cannot target an audio service model", alias.id);
                }
            }
        }
        for recipe in &self.recipes {
            for slot in &recipe.slots {
                if let Some(route_id) = slot.target.strip_prefix("route/") {
                    if !self.routes.iter().any(|route| route.id == route_id) {
                        bail!(
                            "recipe '{}' slot '{}' references unknown route '{}'",
                            recipe.id,
                            slot.id,
                            route_id
                        );
                    }
                } else {
                    validate_candidate_provider(
                        &format!("recipe '{}' slot '{}'", recipe.id, slot.id),
                        &slot.target,
                        &provider_ids,
                    )?;
                }
            }
        }
        Ok(())
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|provider| {
            provider.id == id
                && !provider.disabled
                && (self.privacy_lock.mode != PrivacyMode::LocalOnly || provider.realm == "local")
        })
    }

    pub fn route(&self, id: &str) -> Option<&RouteConfig> {
        self.routes.iter().find(|route| route.id == id)
    }

    pub fn model_alias(&self, id: &str) -> Option<&ModelAliasConfig> {
        self.aliases
            .iter()
            .find(|alias| alias.id == id && !alias.disabled)
    }

    pub fn requested_alias(&self, requested: &str) -> Option<&ModelAliasConfig> {
        self.aliases.iter().find(|alias| {
            !alias.disabled
                && (alias.qualified_id() == requested
                    || self.server.model_id_format.publish(&alias.qualified_id()) == requested)
        })
    }

    pub fn recipe(&self, id: &str) -> Option<&RecipeConfig> {
        self.recipes
            .iter()
            .find(|recipe| recipe.id == id && !recipe.disabled)
    }

    pub fn recipe_slot_target(&self, target: &str) -> Option<(&RecipeConfig, &RecipeSlotConfig)> {
        let (recipe_id, slot_id) = target.strip_prefix("recipe/")?.split_once('/')?;
        let recipe = self.recipe(recipe_id)?;
        let slot = recipe
            .slots
            .iter()
            .find(|slot| slot.id == slot_id && slot.trigger == RecipeSlotTrigger::Model)?;
        Some((recipe, slot))
    }

    pub fn requested_route(&self, requested: &str) -> Option<&RouteConfig> {
        self.routes
            .iter()
            .find(|route| {
                route.qualified_id() == requested
                    || self.server.model_id_format.publish(&route.qualified_id()) == requested
            })
            .or_else(|| {
                self.requested_alias(requested)
                    .and_then(|alias| alias.target.strip_prefix("route/"))
                    .and_then(|route_id| self.route(route_id))
            })
    }

    pub fn published_model_id(&self, provider_id: &str, provider_model_id: &str) -> String {
        let namespace = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .map(ProviderConfig::display_namespace)
            .unwrap_or(provider_id);
        self.server
            .model_id_format
            .publish(&format!("{namespace}/{provider_model_id}"))
    }

    pub fn static_audio_service(&self, candidate: &str) -> Option<AudioService> {
        let (provider_id, model_id) = candidate.split_once('/')?;
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id && provider.is_audio_provider())?;
        let model = provider.models.iter().find(|model| model.id == model_id)?;
        [AudioService::Transcription, AudioService::Speech]
            .into_iter()
            .find(|service| model.supports_audio_service(provider.kind, *service))
            .filter(|_| {
                [AudioService::Transcription, AudioService::Speech]
                    .into_iter()
                    .filter(|service| model.supports_audio_service(provider.kind, *service))
                    .count()
                    == 1
            })
    }

    pub fn is_static_audio_model(&self, candidate: &str, service: AudioService) -> bool {
        self.static_audio_service(candidate) == Some(service)
    }

    fn static_audio_service_for_route(&self, route: &RouteConfig, service: AudioService) -> bool {
        !route.candidates.is_empty()
            && route
                .candidates
                .iter()
                .all(|candidate| self.is_static_audio_model(candidate, service))
    }
}

pub(crate) fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let parts = pattern.split('*').collect::<Vec<_>>();
    if parts.len() == 1 {
        return pattern == value;
    }
    let mut remainder = value;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 && !pattern.starts_with('*') {
            let Some(next) = remainder.strip_prefix(part) else {
                return false;
            };
            remainder = next;
            continue;
        }
        let Some(position) = remainder.find(part) else {
            return false;
        };
        remainder = &remainder[position + part.len()..];
    }
    pattern.ends_with('*') || remainder.is_empty()
}

fn validate_namespace_id(kind: &str, id: &str) -> Result<()> {
    if id.is_empty()
        || id.contains('/')
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        bail!("{kind} id '{id}' must contain only letters, numbers, '-' or '_'");
    }
    Ok(())
}

fn validate_route_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("route id cannot be empty");
    }
    for segment in id.split('/') {
        validate_namespace_id("route id segment", segment)?;
    }
    Ok(())
}

fn validate_candidate_provider(
    owner: &str,
    candidate: &str,
    provider_ids: &HashSet<String>,
) -> Result<()> {
    let (provider, model) = candidate.split_once('/').ok_or_else(|| {
        anyhow::anyhow!("{owner} candidate '{candidate}' is not provider-qualified")
    })?;
    if provider == "route" || model.is_empty() || !provider_ids.contains(provider) {
        bail!("{owner} has invalid candidate '{candidate}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_model_patterns_preserve_anchoring() {
        for (pattern, value, expected) in [
            ("*", "lab/model", true),
            ("lab/*", "lab/model", true),
            ("lab/model", "other/lab/model", false),
            ("*model", "model-x", false),
            ("lab/*-*-it", "lab/a-b-it", true),
            ("", "", true),
            ("", "x", false),
        ] {
            assert_eq!(
                wildcard_matches(pattern, value),
                expected,
                "{pattern}: {value}"
            );
        }
    }

    fn base_config() -> Config {
        Config {
            server: ServerConfig::default(),
            privacy_lock: PrivacyLockConfig::default(),
            probes: ProbeConfig::default(),
            routing: RoutingConfig::default(),
            clients: vec![],
            recipes: vec![],
            providers: vec![ProviderConfig {
                id: "lmstudio".into(),
                kind: ProviderKind::OpenaiCompatible,
                title: String::new(),
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
                auto_load: false,
                jit_load: false,
                load_context_length: None,
                disabled: false,
            }],
            routes: vec![],
            aliases: vec![],
            plugins: vec![],
        }
    }

    #[test]
    fn validates_provider_qualified_candidates() {
        let mut config = base_config();
        config.routes.push(RouteConfig {
            id: "private".into(),
            title: String::new(),
            description: String::new(),
            allowed_realms: vec!["local".into()],
            candidates: vec!["lmstudio/qwen/model".into()],
            strategy: RouteStrategy::FirstAvailable,
            task: RoutingTask::Auto,
            effort: RoutingEffort::Auto,
            context_length: None,
            token_estimate_bytes_per_token: None,
            compression_max_output_tokens: None,
            on_no_eligible_model: NoEligibleModel::Deny,
        });
        config.validate().unwrap();
    }

    #[test]
    fn validates_static_transcription_provider_and_dedicated_route() {
        let mut config = base_config();
        config.providers.push(ProviderConfig {
            id: "speech".into(),
            kind: ProviderKind::OpenaiTranscription,
            title: "Parakeet".into(),
            alias: None,
            base_url: "http://agamotto:5090/v1".into(),
            realm: "local".into(),
            api_key_env: None,
            oauth_client_id: None,
            anthropic_compat: None,
            models: vec![StaticModelConfig {
                id: "nemotron-parakeet".into(),
                upstream_id: "parakeet".into(),
                title: "Nemotron Parakeet".into(),
                services: vec![],
            }],
            include_models: vec![],
            exclude_models: vec![],
            priority: 10,
            auto_load: false,
            jit_load: false,
            load_context_length: None,
            disabled: false,
        });
        config.routing.models.push(RoutingModelConfig {
            model: "speech/nemotron-parakeet".into(),
            context_length: None,
            auto_routing: false,
            roles: vec![RoutingModelRole::Transcription],
            strengths: BTreeMap::new(),
            traits: RoutingModelTraits::default(),
            disabled: false,
        });
        config.routes.push(RouteConfig {
            id: "private/transcription".into(),
            title: String::new(),
            description: String::new(),
            allowed_realms: vec!["local".into()],
            candidates: vec!["speech/nemotron-parakeet".into()],
            strategy: RouteStrategy::FirstAvailable,
            task: RoutingTask::Auto,
            effort: RoutingEffort::Auto,
            context_length: None,
            token_estimate_bytes_per_token: None,
            compression_max_output_tokens: None,
            on_no_eligible_model: NoEligibleModel::Deny,
        });
        config.validate().unwrap();

        config.routes[0]
            .candidates
            .push("lmstudio/qwen/model".into());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("mixes audio services")
        );
    }

    #[test]
    fn named_clients_require_distinct_key_environments() {
        let mut config = base_config();
        config.clients = vec![
            ClientConfig {
                id: "claude-code".into(),
                title: "Claude Code".into(),
                api_key_env: "SHARED_CLIENT_KEY".into(),
                recipe: None,
                recipes: vec![],
                disabled: false,
            },
            ClientConfig {
                id: "hermes".into(),
                title: "Hermes".into(),
                api_key_env: "SHARED_CLIENT_KEY".into(),
                recipe: None,
                recipes: vec![],
                disabled: false,
            },
        ];
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("distinct key"));
    }

    #[test]
    fn validates_key_bound_claude_recipe_and_rejects_unknown_binding() {
        let mut config = base_config();
        config.recipes.push(RecipeConfig {
            id: "claude-lab".into(),
            title: "Claude lab".into(),
            kind: RecipeKind::ClaudeCode,
            protocol: RecipeProtocol::Anthropic,
            allowed_realms: vec!["local".into()],
            slots: vec![
                RecipeSlotConfig {
                    id: "primary".into(),
                    title: String::new(),
                    target: "lmstudio/main".into(),
                    trigger: RecipeSlotTrigger::Model,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::Inherit,
                },
                RecipeSlotConfig {
                    id: "compression".into(),
                    title: String::new(),
                    target: "lmstudio/compressor".into(),
                    trigger: RecipeSlotTrigger::Compression,
                    max_output_tokens: Some(16_384),
                    reasoning_effort: RecipeReasoningEffort::Inherit,
                },
            ],
            disabled: false,
        });
        config.clients.push(ClientConfig {
            id: "claude".into(),
            title: String::new(),
            api_key_env: "CLAUDE_KEY".into(),
            recipe: Some("claude-lab".into()),
            recipes: vec![],
            disabled: false,
        });
        config.validate().unwrap();

        config.clients[0].recipe = Some("missing".into());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("unknown recipe")
        );
    }

    #[test]
    fn validates_multiple_selectable_hermes_recipes_without_exclusive_binding() {
        let mut config = base_config();
        for id in ["hermes-private", "hermes-cloud"] {
            config.recipes.push(RecipeConfig {
                id: id.into(),
                title: id.into(),
                kind: RecipeKind::Hermes,
                protocol: RecipeProtocol::Openai,
                allowed_realms: vec![],
                slots: vec![
                    RecipeSlotConfig {
                        id: "primary".into(),
                        title: "Primary".into(),
                        target: "lmstudio/main".into(),
                        trigger: RecipeSlotTrigger::Model,
                        max_output_tokens: None,
                        reasoning_effort: RecipeReasoningEffort::Inherit,
                    },
                    RecipeSlotConfig {
                        id: "compression".into(),
                        title: "Compression".into(),
                        target: "lmstudio/compressor".into(),
                        trigger: RecipeSlotTrigger::Compression,
                        max_output_tokens: Some(12_000),
                        reasoning_effort: RecipeReasoningEffort::None,
                    },
                ],
                disabled: false,
            });
        }
        config.clients.push(ClientConfig {
            id: "hermes".into(),
            title: "Hermes".into(),
            api_key_env: "HERMES_KEY".into(),
            recipe: None,
            recipes: vec!["hermes-private".into(), "hermes-cloud".into()],
            disabled: false,
        });
        config.validate().unwrap();
        assert!(config.clients[0].authorizes_recipe("hermes-private"));

        config.clients[0].recipe = Some("hermes-private".into());
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("cannot combine exclusive recipe")
        );
    }

    #[test]
    fn claude_recipe_slots_accept_the_verified_extended_context_suffix() {
        let recipe = RecipeConfig {
            id: "claude-lab".into(),
            title: String::new(),
            kind: RecipeKind::ClaudeCode,
            protocol: RecipeProtocol::Anthropic,
            allowed_realms: vec!["local".into()],
            slots: vec![RecipeSlotConfig {
                id: "primary".into(),
                title: String::new(),
                target: "lmstudio/main".into(),
                trigger: RecipeSlotTrigger::Model,
                max_output_tokens: None,
                reasoning_effort: RecipeReasoningEffort::Inherit,
            }],
            disabled: false,
        };

        assert_eq!(
            recipe
                .slot_for_request("recipe:claude-lab:primary[1m]")
                .map(|slot| slot.id.as_str()),
            Some("primary")
        );
        assert_eq!(
            recipe
                .slot_for_request("recipe/claude-lab/primary[1m]")
                .map(|slot| slot.id.as_str()),
            Some("primary")
        );

        let mut generic = recipe;
        generic.kind = RecipeKind::Generic;
        assert!(
            generic
                .slot_for_request("recipe:claude-lab:primary[1m]")
                .is_none()
        );
    }

    #[test]
    fn migrates_legacy_claude_classifier_background_split_without_losing_target() {
        let mut config = base_config();
        config.recipes.push(RecipeConfig {
            id: "claude-lab".into(),
            title: "Claude lab".into(),
            kind: RecipeKind::ClaudeCode,
            protocol: RecipeProtocol::Anthropic,
            allowed_realms: vec!["local".into()],
            slots: vec![
                RecipeSlotConfig {
                    id: "primary".into(),
                    title: "Primary".into(),
                    target: "lmstudio/main".into(),
                    trigger: RecipeSlotTrigger::Model,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::Inherit,
                },
                RecipeSlotConfig {
                    id: "classifier".into(),
                    title: "Classifier / background".into(),
                    target: "lmstudio/classifier-8k".into(),
                    trigger: RecipeSlotTrigger::Model,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::Inherit,
                },
                RecipeSlotConfig {
                    id: "subagent".into(),
                    title: "Subagent".into(),
                    target: "lmstudio/worker".into(),
                    trigger: RecipeSlotTrigger::Model,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::Inherit,
                },
            ],
            disabled: false,
        });

        assert!(config.migrate_legacy_recipes());
        assert!(!config.migrate_legacy_recipes());
        let recipe = &config.recipes[0];
        let background = recipe
            .slots
            .iter()
            .find(|slot| slot.id == "background")
            .unwrap();
        let classifier = recipe.classifier_slot().unwrap();
        assert_eq!(background.target, "lmstudio/worker");
        assert_eq!(classifier.id, "classifier");
        assert_eq!(classifier.target, "lmstudio/classifier-8k");
        assert_eq!(classifier.max_output_tokens, Some(1024));
        config.validate().unwrap();
    }

    #[test]
    fn migrates_legacy_hermes_recipe_with_primary_delegation_target() {
        let mut config = base_config();
        config.recipes.push(RecipeConfig {
            id: "hermes-private".into(),
            title: "Hermes private".into(),
            kind: RecipeKind::Hermes,
            protocol: RecipeProtocol::Openai,
            allowed_realms: vec!["local".into()],
            slots: vec![
                RecipeSlotConfig {
                    id: "primary".into(),
                    title: "Primary".into(),
                    target: "lmstudio/main".into(),
                    trigger: RecipeSlotTrigger::Model,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::Inherit,
                },
                RecipeSlotConfig {
                    id: "compression".into(),
                    title: "Compression".into(),
                    target: "lmstudio/compressor".into(),
                    trigger: RecipeSlotTrigger::Compression,
                    max_output_tokens: Some(12_000),
                    reasoning_effort: RecipeReasoningEffort::None,
                },
            ],
            disabled: false,
        });

        assert!(config.migrate_legacy_recipes());
        assert!(!config.migrate_legacy_recipes());
        let recipe = &config.recipes[0];
        assert_eq!(recipe.slots[1].id, "delegation");
        let delegation = recipe.delegation_slot().unwrap();
        assert_eq!(delegation.target, "lmstudio/main");
        assert_eq!(delegation.trigger, RecipeSlotTrigger::Delegation);
        let vision = recipe.vision_slot().unwrap();
        assert_eq!(vision.target, "lmstudio/main");
        assert_eq!(vision.trigger, RecipeSlotTrigger::Vision);
        assert_eq!(vision.reasoning_effort, RecipeReasoningEffort::None);
        config.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_candidate_provider() {
        let mut config = base_config();
        config.routes.push(RouteConfig {
            id: "private".into(),
            title: String::new(),
            description: String::new(),
            allowed_realms: vec!["local".into()],
            candidates: vec!["unknown/qwen".into()],
            strategy: RouteStrategy::FirstAvailable,
            task: RoutingTask::Auto,
            effort: RoutingEffort::Auto,
            context_length: None,
            token_estimate_bytes_per_token: None,
            compression_max_output_tokens: None,
            on_no_eligible_model: NoEligibleModel::Deny,
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_zero_route_context_contract() {
        let mut config = base_config();
        config.routes.push(RouteConfig {
            id: "private/auto".into(),
            title: String::new(),
            description: String::new(),
            allowed_realms: vec!["local".into()],
            candidates: vec![],
            strategy: RouteStrategy::Adaptive,
            task: RoutingTask::Auto,
            effort: RoutingEffort::Auto,
            context_length: Some(0),
            token_estimate_bytes_per_token: None,
            compression_max_output_tokens: None,
            on_no_eligible_model: NoEligibleModel::Deny,
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn validates_route_token_estimate_override_range() {
        let mut config = base_config();
        config.routes.push(RouteConfig {
            id: "private/auto".into(),
            title: String::new(),
            description: String::new(),
            allowed_realms: vec!["local".into()],
            candidates: vec![],
            strategy: RouteStrategy::Adaptive,
            task: RoutingTask::Auto,
            effort: RoutingEffort::Auto,
            context_length: None,
            token_estimate_bytes_per_token: Some(3.5),
            compression_max_output_tokens: Some(16_384),
            on_no_eligible_model: NoEligibleModel::Deny,
        });
        config.validate().unwrap();

        config.routes[0].token_estimate_bytes_per_token = Some(0.5);
        assert!(config.validate().is_err());
        config.routes[0].token_estimate_bytes_per_token = Some(8.5);
        assert!(config.validate().is_err());
    }

    #[test]
    fn compression_output_limit_requires_positive_adaptive_route_setting() {
        let mut config = base_config();
        config.routes.push(RouteConfig {
            id: "private/auto".into(),
            title: String::new(),
            description: String::new(),
            allowed_realms: vec!["local".into()],
            candidates: vec![],
            strategy: RouteStrategy::Adaptive,
            task: RoutingTask::Auto,
            effort: RoutingEffort::Auto,
            context_length: None,
            token_estimate_bytes_per_token: None,
            compression_max_output_tokens: Some(0),
            on_no_eligible_model: NoEligibleModel::Deny,
        });
        assert!(config.validate().is_err());

        config.routes[0].compression_max_output_tokens = Some(16_384);
        config.routes[0].strategy = RouteStrategy::FirstAvailable;
        assert!(config.validate().is_err());
    }

    #[test]
    fn validates_nested_adaptive_route_and_manual_strengths() {
        let mut config = base_config();
        config.routing.mode = RoutingMode::Shadow;
        config.routing.models.push(RoutingModelConfig {
            model: "lmstudio/qwen/model".into(),
            context_length: None,
            auto_routing: true,
            roles: Vec::new(),
            strengths: BTreeMap::from([
                ("code.inspect".into(), 0.8),
                ("code.implement".into(), 0.95),
            ]),
            traits: RoutingModelTraits {
                architecture: "dense".into(),
                latency_tier: 2,
                cost_tier: 0,
                preferred_effort: vec![RoutingEffort::Heavy],
            },
            disabled: false,
        });
        config.routes.push(RouteConfig {
            id: "private/auto".into(),
            title: String::new(),
            description: String::new(),
            allowed_realms: vec!["local".into()],
            candidates: vec![],
            strategy: RouteStrategy::Adaptive,
            task: RoutingTask::Auto,
            effort: RoutingEffort::Auto,
            context_length: None,
            token_estimate_bytes_per_token: None,
            compression_max_output_tokens: None,
            on_no_eligible_model: NoEligibleModel::Deny,
        });
        config.routing.rules.push(RoutingRuleConfig {
            route: Some("private/auto".into()),
            task: Some(RoutingTask::Code),
            operation: Some(RoutingOperation::Implement),
            effort: Some(RoutingEffort::Heavy),
            prefer: vec!["lmstudio/qwen/model".into()],
        });
        config.validate().unwrap();
    }

    #[test]
    fn rejects_out_of_range_routing_strength() {
        let mut config = base_config();
        config.routing.models.push(RoutingModelConfig {
            model: "lmstudio/model".into(),
            context_length: None,
            auto_routing: true,
            roles: Vec::new(),
            strengths: BTreeMap::from([("code".into(), 1.1)]),
            traits: RoutingModelTraits::default(),
            disabled: false,
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn loaded_model_preference_defaults_to_balanced_and_parses_explicit_modes() {
        let config = base_config();
        assert_eq!(
            config.routing.loaded_model_preference,
            LoadedModelPreference::Balanced
        );
        assert_eq!(
            LoadedModelPreference::QualityFirst.bonus(RoutingEffort::Light),
            0.001
        );
        assert_eq!(
            LoadedModelPreference::Balanced.bonus(RoutingEffort::Light),
            3.0
        );
        assert_eq!(
            LoadedModelPreference::WarmFirst.bonus(RoutingEffort::Light),
            8.0
        );

        let config: Config = toml::from_str(
            r#"
            [routing]
            loaded_model_preference = "quality_first"

            [[providers]]
            id = "lab"
            base_url = "http://127.0.0.1:8000/v1"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.routing.loaded_model_preference,
            LoadedModelPreference::QualityFirst
        );
    }

    #[test]
    fn routing_profiles_default_to_auto_eligible_and_parse_reservations() {
        let config: Config = toml::from_str(
            r#"
            [[routing.models]]
            model = "lab/compressor"
            context_length = 262144
            auto_routing = false

            [[routing.models]]
            model = "lab/general"
            roles = ["heavy", "vision", "compression"]

            [[providers]]
            id = "lab"
            base_url = "http://127.0.0.1:8000/v1"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.routing.models[0].context_length, Some(262144));
        assert!(!config.routing.models[0].auto_routing);
        assert!(config.routing.models[1].auto_routing);
        assert_eq!(
            config.routing.models[1].roles,
            vec![
                RoutingModelRole::Heavy,
                RoutingModelRole::Vision,
                RoutingModelRole::Compression
            ]
        );
    }

    #[test]
    fn privacy_lock_disables_cloud_provider_before_routing() {
        let mut config = base_config();
        config.providers.push(ProviderConfig {
            id: "cloud".into(),
            kind: ProviderKind::OpenaiCompatible,
            title: String::new(),
            alias: None,
            base_url: "https://example.com/v1".into(),
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
        });
        config.privacy_lock.mode = PrivacyMode::LocalOnly;
        assert!(config.provider("local").is_none());
        assert!(config.provider("cloud").is_none());
        assert!(config.provider("lmstudio").is_some());
    }

    #[test]
    fn validates_observer_plugin_configuration() {
        let mut config = base_config();
        config.plugins.push(PluginConfig {
            id: "sniffer".into(),
            mode: PluginMode::Observe,
            command: "infernaut-sniffer".into(),
            args: vec!["sniffer.toml".into()],
            events: vec![PluginEventKind::RequestContent],
            queue_capacity: 128,
            disabled: false,
        });
        config.validate().unwrap();
    }

    #[test]
    fn rejects_enabled_plugin_without_event_subscriptions() {
        let mut config = base_config();
        config.plugins.push(PluginConfig {
            id: "sniffer".into(),
            mode: PluginMode::Observe,
            command: "infernaut-sniffer".into(),
            args: vec![],
            events: vec![],
            queue_capacity: 128,
            disabled: false,
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_conflicting_lm_studio_load_strategies() {
        let mut config = base_config();
        config.providers[0].auto_load = true;
        config.providers[0].jit_load = true;
        assert!(config.validate().is_err());
    }

    #[test]
    fn model_filters_support_globs_and_explicit_exclusions() {
        let mut provider = base_config().providers.remove(0);
        provider.include_models = vec!["gpt-*".into(), "o3".into()];
        provider.exclude_models = vec!["*-audio-*".into()];
        assert!(provider.model_allowed("gpt-5"));
        assert!(provider.model_allowed("o3"));
        assert!(!provider.model_allowed("gpt-4o-audio-preview"));
        assert!(!provider.model_allowed("text-embedding-3-large"));
    }

    #[test]
    fn cloud_presets_supply_urls_but_require_cloud_realm_and_key_env() {
        let mut config = base_config();
        config.providers[0].kind = ProviderKind::Openai;
        config.providers[0].base_url.clear();
        config.providers[0].realm = "cloud".into();
        config.providers[0].api_key_env = Some("OPENAI_API_KEY".into());
        assert_eq!(
            config.providers[0].effective_base_url(),
            "https://api.openai.com/v1"
        );
        config.validate().unwrap();

        config.providers[0].kind = ProviderKind::OllamaCloud;
        assert_eq!(
            config.providers[0].effective_base_url(),
            "https://ollama.com/v1"
        );
    }

    #[test]
    fn anthropic_messages_support_is_explicit_for_generic_providers() {
        let mut provider = base_config().providers.remove(0);
        assert!(!provider.supports_anthropic_messages());

        provider.anthropic_compat = Some(true);
        assert!(provider.supports_anthropic_messages());

        provider.kind = ProviderKind::LmStudio;
        provider.anthropic_compat = None;
        assert!(provider.supports_anthropic_messages());

        provider.anthropic_compat = Some(false);
        assert!(!provider.supports_anthropic_messages());

        provider.kind = ProviderKind::OllamaCloud;
        provider.anthropic_compat = None;
        assert!(provider.supports_anthropic_messages());
    }

    #[test]
    fn vllm_kind_parses_and_enables_anthropic_messages() {
        let config: Config = toml::from_str(
            r#"
                [[providers]]
                id = "vllm"
                kind = "vllm"
                base_url = "http://127.0.0.1:8000/v1"
                realm = "local"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.providers[0].kind, ProviderKind::Vllm);
        assert!(config.providers[0].supports_anthropic_messages());
    }

    #[test]
    fn colon_qualified_ids_preserve_provider_identity_without_slashes() {
        assert_eq!(
            ModelIdFormat::ColonQualified.publish("lmstudio/org/model:tag"),
            "lmstudio:org:model:tag"
        );
    }

    #[test]
    fn rejects_provider_alias_collisions() {
        let mut config = base_config();
        config.providers[0].alias = Some("cloud".into());
        let mut second = config.providers[0].clone();
        second.id = "cloud".into();
        second.alias = Some("remote".into());
        config.providers.push(second);
        assert!(config.validate().is_err());
    }

    #[test]
    fn validates_deterministic_model_aliases_and_rejects_invalid_contracts() {
        let mut config = base_config();
        config.aliases.push(ModelAliasConfig {
            id: "cron".into(),
            title: "Cron automation".into(),
            target: "lmstudio/model".into(),
            context_length: Some(131_072),
            disabled: false,
        });
        config.validate().unwrap();
        assert_eq!(config.aliases[0].qualified_id(), "alias/cron");
        let published = config.server.model_id_format.publish("alias/cron");
        assert!(config.requested_alias(&published).is_some());

        config.aliases[0].context_length = Some(0);
        assert!(config.validate().is_err());
    }

    #[test]
    fn alias_namespace_is_reserved_for_virtual_model_pointers() {
        let mut config = base_config();
        config.providers[0].alias = Some("alias".into());
        assert!(config.validate().is_err());
    }
}
