use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::Serialize;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

use crate::{
    auth::AuthManager,
    catalog::Catalog,
    config::{
        AudioService, ClientConfig, Config, LoadedModelPreference, ModelAliasConfig,
        NoEligibleModel, PrivacyMode, ProviderConfig, ProviderKind, RecipeConfig, RecipeKind,
        RecipeProtocol, RecipeReasoningEffort, RecipeSlotConfig, RecipeSlotTrigger, RouteConfig,
        RouteStrategy, RoutingEffort, RoutingMode, RoutingModelConfig, RoutingModelRole,
        RoutingModelTraits, RoutingTask,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Archetype {
    FastGeneralist,
    SmallCodeInspector,
    Balanced,
    HeavyDevelopment,
    VisionSpecialist,
    LegacyHeavyCoder,
    LegacyDataSpecialist,
}

#[derive(Clone, Copy)]
pub struct ArchetypeSuggestion {
    pub archetype: Archetype,
    pub vision: bool,
    pub reason: &'static str,
}

pub fn suggest_archetype(model: &crate::catalog::DiscoveredModel) -> ArchetypeSuggestion {
    let name = model.id.to_ascii_lowercase();
    let looks_multimodal = ["vision", "-vl", "_vl", "pixtral", "llava"]
        .iter()
        .any(|marker| name.contains(marker))
        || model.capabilities.get("vision").is_some_and(|vision| {
            vision.as_bool().unwrap_or(false)
                || vision.get("support").and_then(serde_json::Value::as_str) == Some("supported")
        });
    if ["sql", "data", "analytics"]
        .iter()
        .any(|marker| name.contains(marker))
    {
        return ArchetypeSuggestion {
            archetype: Archetype::HeavyDevelopment,
            vision: looks_multimodal,
            reason: "data-specialist family; deep development and data share one role",
        };
    }
    let code = ["code", "coder", "devstral"]
        .iter()
        .any(|marker| name.contains(marker));
    let large = [
        "24b", "27b", "30b", "32b", "34b", "35b", "70b", "72b", "120b",
    ]
    .iter()
    .any(|marker| contains_parameter_marker(&name, marker));
    if code && large {
        return ArchetypeSuggestion {
            archetype: Archetype::HeavyDevelopment,
            vision: looks_multimodal,
            reason: "coding family with a large parameter class",
        };
    }
    if code {
        return ArchetypeSuggestion {
            archetype: Archetype::SmallCodeInspector,
            vision: looks_multimodal,
            reason: "coding model-family marker",
        };
    }
    let small = [
        "0.5b", "1b", "1.2b", "1.5b", "2b", "3b", "4b", "7b", "8b", "9b",
    ]
    .iter()
    .any(|marker| contains_parameter_marker(&name, marker));
    if small {
        ArchetypeSuggestion {
            archetype: Archetype::FastGeneralist,
            vision: looks_multimodal,
            reason: "small parameter class",
        }
    } else {
        ArchetypeSuggestion {
            archetype: Archetype::Balanced,
            vision: looks_multimodal,
            reason: if looks_multimodal {
                "conservative workload default plus reported vision capability"
            } else {
                "conservative default; no specialist marker was reported"
            },
        }
    }
}

fn contains_parameter_marker(name: &str, marker: &str) -> bool {
    name.match_indices(marker).any(|(start, matched)| {
        let before = name[..start].chars().next_back();
        let after = name[start + matched.len()..].chars().next();
        !before.is_some_and(|character| character.is_ascii_digit() || character == '.')
            && !after.is_some_and(|character| character.is_ascii_alphanumeric())
    })
}

const CONTEXT_TIERS: [Option<u64>; 7] = [
    None,
    Some(32 * 1024),
    Some(64 * 1024),
    Some(128 * 1024),
    Some(200_000),
    Some(256 * 1024),
    Some(1024 * 1024),
];

fn next_context_tier(current: Option<u64>, forward: bool) -> Option<u64> {
    let current = current.unwrap_or(0);
    let exact = CONTEXT_TIERS
        .iter()
        .position(|tier| tier.unwrap_or(0) == current);
    let next = if let Some(position) = exact {
        if forward {
            (position + 1) % CONTEXT_TIERS.len()
        } else {
            position.checked_sub(1).unwrap_or(CONTEXT_TIERS.len() - 1)
        }
    } else if forward {
        CONTEXT_TIERS
            .iter()
            .position(|tier| tier.unwrap_or(0) > current)
            .unwrap_or(0)
    } else {
        CONTEXT_TIERS
            .iter()
            .rposition(|tier| tier.unwrap_or(0) < current)
            .unwrap_or(CONTEXT_TIERS.len() - 1)
    };
    CONTEXT_TIERS[next]
}

impl Archetype {
    pub const ALL: [Self; 5] = [
        Self::FastGeneralist,
        Self::SmallCodeInspector,
        Self::Balanced,
        Self::HeavyDevelopment,
        Self::VisionSpecialist,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::FastGeneralist => "Fast generalist",
            Self::SmallCodeInspector => "Small code inspector",
            Self::Balanced => "Balanced",
            Self::HeavyDevelopment => "Heavy development + data",
            Self::VisionSpecialist => "Vision",
            Self::LegacyHeavyCoder => "Heavy coder (legacy)",
            Self::LegacyDataSpecialist => "Data specialist (legacy)",
        }
    }

    pub fn profile(self, model: String) -> RoutingModelConfig {
        let strengths = match self {
            Self::FastGeneralist => strengths(&[
                ("chat", 0.95),
                ("code.inspect", 0.78),
                ("code.review", 0.62),
                ("code.debug", 0.60),
                ("code.implement", 0.42),
                ("code.architect", 0.30),
                ("data.analyze", 0.62),
                ("default", 0.55),
            ]),
            Self::SmallCodeInspector => strengths(&[
                ("chat", 0.78),
                ("code.inspect", 0.94),
                ("code.review", 0.82),
                ("code.debug", 0.76),
                ("code.modify", 0.68),
                ("code.implement", 0.52),
                ("code.architect", 0.35),
                ("data.analyze", 0.62),
                ("default", 0.58),
            ]),
            Self::Balanced => strengths(&[
                ("chat", 0.82),
                ("code.inspect", 0.84),
                ("code.review", 0.84),
                ("code.debug", 0.84),
                ("code.modify", 0.82),
                ("code.implement", 0.80),
                ("code.architect", 0.72),
                ("data.analyze", 0.82),
                ("data.summarize", 0.85),
                ("default", 0.76),
            ]),
            Self::HeavyDevelopment => strengths(&[
                ("chat", 0.68),
                ("code.inspect", 0.80),
                ("code.review", 0.95),
                ("code.debug", 0.94),
                ("code.modify", 0.94),
                ("code.implement", 0.99),
                ("code.architect", 1.00),
                ("code.migrate", 0.98),
                ("data.analyze", 1.00),
                ("data.summarize", 0.96),
                ("analyze", 0.98),
                ("summarize", 0.92),
                ("default", 0.72),
            ]),
            Self::LegacyHeavyCoder => strengths(&[
                ("chat", 0.58),
                ("code.inspect", 0.80),
                ("code.review", 0.95),
                ("code.debug", 0.94),
                ("code.modify", 0.94),
                ("code.implement", 0.99),
                ("code.architect", 1.00),
                ("code.migrate", 0.98),
                ("data.analyze", 0.86),
                ("default", 0.72),
            ]),
            Self::LegacyDataSpecialist => strengths(&[
                ("chat", 0.68),
                ("code.inspect", 0.70),
                ("code.implement", 0.65),
                ("data.analyze", 1.00),
                ("data.summarize", 0.96),
                ("analyze", 0.98),
                ("summarize", 0.92),
                ("default", 0.70),
            ]),
            Self::VisionSpecialist => strengths(&[
                ("chat", 0.82),
                ("code.inspect", 0.72),
                ("code.review", 0.70),
                ("data.analyze", 0.84),
                ("default", 0.74),
            ]),
        };
        let (architecture, latency_tier, cost_tier, preferred_effort) = match self {
            Self::FastGeneralist => ("small", 0, 0, vec![RoutingEffort::Light]),
            Self::SmallCodeInspector => ("moe", 1, 0, vec![RoutingEffort::Light]),
            Self::Balanced => (
                "general",
                2,
                1,
                vec![RoutingEffort::Light, RoutingEffort::Standard],
            ),
            Self::HeavyDevelopment | Self::LegacyHeavyCoder => (
                "dense",
                4,
                3,
                vec![RoutingEffort::Standard, RoutingEffort::Heavy],
            ),
            Self::LegacyDataSpecialist => (
                "specialist",
                2,
                1,
                vec![RoutingEffort::Standard, RoutingEffort::Heavy],
            ),
            Self::VisionSpecialist => (
                "multimodal",
                3,
                2,
                vec![RoutingEffort::Standard, RoutingEffort::Heavy],
            ),
        };
        RoutingModelConfig {
            model,
            context_length: None,
            auto_routing: true,
            roles: match self {
                Self::FastGeneralist => vec![RoutingModelRole::Fast],
                Self::SmallCodeInspector => vec![RoutingModelRole::Inspect],
                Self::Balanced => vec![RoutingModelRole::Balanced],
                Self::HeavyDevelopment => vec![RoutingModelRole::Heavy],
                Self::VisionSpecialist => {
                    vec![RoutingModelRole::Balanced, RoutingModelRole::Vision]
                }
                Self::LegacyHeavyCoder | Self::LegacyDataSpecialist => Vec::new(),
            },
            strengths,
            traits: RoutingModelTraits {
                architecture: architecture.to_owned(),
                latency_tier,
                cost_tier,
                preferred_effort,
            },
            disabled: false,
        }
    }
}

pub fn suggested_roles_name(suggestion: ArchetypeSuggestion) -> String {
    if suggestion.vision {
        format!("{} + Vision", suggestion.archetype.name())
    } else {
        suggestion.archetype.name().to_owned()
    }
}

pub fn applied_roles_name(profile: &RoutingModelConfig) -> String {
    let roles = effective_roles(profile);
    let primary = roles.iter().find_map(|role| match role {
        RoutingModelRole::Fast => Some("Fast"),
        RoutingModelRole::Inspect => Some("Inspect"),
        RoutingModelRole::Balanced => Some("Balanced"),
        RoutingModelRole::Heavy => Some("Heavy + data"),
        RoutingModelRole::Transcription => Some("Transcription"),
        RoutingModelRole::Speech => Some("Speech"),
        RoutingModelRole::Vision | RoutingModelRole::Compression => None,
    });
    let vision = roles.contains(&RoutingModelRole::Vision);
    let compression = roles.contains(&RoutingModelRole::Compression);
    let mut label = primary.unwrap_or("Custom").to_owned();
    if vision {
        label.push_str(" + Vision");
    }
    if compression {
        label.push_str(" + Compression");
    }
    if profile.roles.iter().any(|role| !role.is_additive())
        && !profile_matches_declared_role(profile)
    {
        label.push('*');
    }
    label
}

fn effective_roles(profile: &RoutingModelConfig) -> Vec<RoutingModelRole> {
    if !profile.roles.is_empty() {
        return profile.roles.clone();
    }
    for (archetype, roles) in [
        (Archetype::FastGeneralist, vec![RoutingModelRole::Fast]),
        (
            Archetype::SmallCodeInspector,
            vec![RoutingModelRole::Inspect],
        ),
        (Archetype::Balanced, vec![RoutingModelRole::Balanced]),
        (Archetype::HeavyDevelopment, vec![RoutingModelRole::Heavy]),
        (Archetype::LegacyHeavyCoder, vec![RoutingModelRole::Heavy]),
        (
            Archetype::LegacyDataSpecialist,
            vec![RoutingModelRole::Heavy],
        ),
        (
            Archetype::VisionSpecialist,
            vec![RoutingModelRole::Balanced, RoutingModelRole::Vision],
        ),
    ] {
        if profile_matches_archetype(profile, archetype) {
            return roles;
        }
    }
    Vec::new()
}

fn profile_matches_declared_role(profile: &RoutingModelConfig) -> bool {
    let Some(role) = profile.roles.iter().find(|role| !role.is_additive()) else {
        return false;
    };
    let archetype = match role {
        RoutingModelRole::Fast => Archetype::FastGeneralist,
        RoutingModelRole::Inspect => Archetype::SmallCodeInspector,
        RoutingModelRole::Balanced => Archetype::Balanced,
        RoutingModelRole::Heavy => Archetype::HeavyDevelopment,
        RoutingModelRole::Transcription | RoutingModelRole::Speech => return true,
        RoutingModelRole::Vision | RoutingModelRole::Compression => unreachable!(),
    };
    profile_matches_archetype(profile, archetype)
}

fn profile_matches_archetype(profile: &RoutingModelConfig, archetype: Archetype) -> bool {
    let expected = archetype.profile(profile.model.clone());
    profile.strengths == expected.strengths && profile.traits == expected.traits
}

fn strengths(entries: &[(&str, f64)]) -> BTreeMap<String, f64> {
    entries
        .iter()
        .map(|(key, score)| ((*key).to_owned(), *score))
        .collect()
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelProfileState {
    Exact,
    Wildcard,
    Unclassified,
    Dormant,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelReconciliation {
    pub model: String,
    pub provider: String,
    pub state: ModelProfileState,
    pub matched_profile: Option<String>,
    pub auto_routing: Option<bool>,
    pub loaded: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorIssue {
    pub severity: &'static str,
    pub code: &'static str,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub config: String,
    pub providers: Vec<crate::catalog::ProviderStatus>,
    pub models: Vec<ModelReconciliation>,
    pub issues: Vec<DoctorIssue>,
}

pub struct ConfigEditor {
    path: PathBuf,
    original: String,
    document: DocumentMut,
    pub config: Config,
}

impl ConfigEditor {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let original = fs::read_to_string(&path)
            .with_context(|| format!("failed to read configuration at {}", path.display()))?;
        let document = original
            .parse::<DocumentMut>()
            .with_context(|| format!("failed to parse editable TOML at {}", path.display()))?;
        let mut config: Config = toml::from_str(&original)
            .with_context(|| format!("failed to parse configuration at {}", path.display()))?;
        let migrated = config.migrate_legacy_recipes();
        config.validate()?;
        let mut editor = Self {
            path,
            original,
            document,
            config,
        };
        if migrated {
            editor.sync_recipes();
        }
        Ok(editor)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_dirty(&self) -> bool {
        self.rendered() != self.original
    }

    pub fn set_routing_mode(&mut self, mode: RoutingMode) {
        self.config.routing.mode = mode;
        self.ensure_top_level_table("routing");
        self.document["routing"]["mode"] = value(routing_mode_name(mode));
    }

    pub fn cycle_routing_mode(&mut self) {
        let mode = match self.config.routing.mode {
            RoutingMode::Off => RoutingMode::Shadow,
            RoutingMode::Shadow => RoutingMode::Active,
            RoutingMode::Active => RoutingMode::Off,
        };
        self.set_routing_mode(mode);
    }

    pub fn cycle_loaded_model_preference(&mut self) {
        self.config.routing.loaded_model_preference =
            match self.config.routing.loaded_model_preference {
                LoadedModelPreference::QualityFirst => LoadedModelPreference::Balanced,
                LoadedModelPreference::Balanced => LoadedModelPreference::WarmFirst,
                LoadedModelPreference::WarmFirst => LoadedModelPreference::QualityFirst,
            };
        self.ensure_top_level_table("routing");
        self.document["routing"]["loaded_model_preference"] = value(loaded_model_preference_name(
            self.config.routing.loaded_model_preference,
        ));
    }

    pub fn cycle_privacy_mode(&mut self) {
        self.config.privacy_lock.mode = match self.config.privacy_lock.mode {
            PrivacyMode::Off => PrivacyMode::Selective,
            PrivacyMode::Selective => PrivacyMode::LocalOnly,
            PrivacyMode::LocalOnly => PrivacyMode::Off,
        };
        self.ensure_top_level_table("privacy_lock");
        self.document["privacy_lock"]["mode"] =
            value(privacy_mode_name(self.config.privacy_lock.mode));
    }

    pub fn set_route_realms(&mut self, index: usize, realms: Vec<String>) {
        if let Some(route) = self.config.routes.get_mut(index) {
            route.allowed_realms = realms;
            self.sync_routes();
        }
    }

    pub fn cycle_route_task(&mut self, index: usize) {
        if let Some(route) = self.config.routes.get_mut(index) {
            route.task = match route.task {
                RoutingTask::Auto => RoutingTask::Code,
                RoutingTask::Code => RoutingTask::Data,
                RoutingTask::Data => RoutingTask::Chat,
                RoutingTask::Chat => RoutingTask::Auto,
            };
            self.sync_routes();
        }
    }

    pub fn cycle_route_effort(&mut self, index: usize) {
        if let Some(route) = self.config.routes.get_mut(index) {
            route.effort = match route.effort {
                RoutingEffort::Auto => RoutingEffort::Light,
                RoutingEffort::Light => RoutingEffort::Standard,
                RoutingEffort::Standard => RoutingEffort::Heavy,
                RoutingEffort::Heavy => RoutingEffort::Auto,
            };
            self.sync_routes();
        }
    }

    pub fn toggle_route_strategy(&mut self, index: usize) {
        if let Some(route) = self.config.routes.get_mut(index) {
            route.strategy = match route.strategy {
                RouteStrategy::FirstAvailable => RouteStrategy::Adaptive,
                RouteStrategy::Adaptive => RouteStrategy::FirstAvailable,
            };
            self.sync_routes();
        }
    }

    pub fn cycle_route_context_length(&mut self, index: usize, forward: bool) {
        if let Some(route) = self.config.routes.get_mut(index) {
            route.context_length = next_context_tier(route.context_length, forward);
            self.sync_routes();
        }
    }

    pub fn cycle_model_context_length(&mut self, model: &str, forward: bool) -> Option<u64> {
        self.ensure_exact_model_profile(model);
        let profile = self
            .config
            .routing
            .models
            .iter_mut()
            .find(|profile| profile.model == model)
            .expect("an exact model profile was created");
        profile.context_length = next_context_tier(profile.context_length, forward);
        let context_length = profile.context_length;
        self.sync_routing_models();
        context_length
    }

    pub fn cycle_route_token_estimate(&mut self, index: usize) {
        const ESTIMATES: [Option<f64>; 6] = [
            None,
            Some(3.0),
            Some(3.25),
            Some(3.5),
            Some(3.75),
            Some(4.0),
        ];
        if let Some(route) = self.config.routes.get_mut(index) {
            let current = route.token_estimate_bytes_per_token;
            let position = ESTIMATES
                .iter()
                .position(|estimate| *estimate == current)
                .unwrap_or(0);
            route.token_estimate_bytes_per_token = ESTIMATES[(position + 1) % ESTIMATES.len()];
            self.sync_routes();
        }
    }

    pub fn cycle_route_compression_output_limit(&mut self, index: usize) {
        const LIMITS: [Option<u64>; 6] = [
            None,
            Some(4 * 1024),
            Some(8 * 1024),
            Some(16 * 1024),
            Some(24 * 1024),
            Some(32 * 1024),
        ];
        if let Some(route) = self.config.routes.get_mut(index) {
            let current = route.compression_max_output_tokens;
            let position = LIMITS
                .iter()
                .position(|limit| *limit == current)
                .unwrap_or(0);
            route.compression_max_output_tokens = LIMITS[(position + 1) % LIMITS.len()];
            self.sync_routes();
        }
    }

    pub fn apply_archetype(&mut self, model: &str, archetype: Archetype) {
        if archetype == Archetype::VisionSpecialist {
            self.toggle_vision_role(model);
            return;
        }
        let additive_roles = self
            .config
            .routing
            .models
            .iter()
            .find(|profile| profile.model == model)
            .map(effective_roles)
            .unwrap_or_default()
            .into_iter()
            .filter(|role| role.is_additive())
            .collect::<Vec<_>>();
        let mut replacement = archetype.profile(model.to_owned());
        replacement.roles.extend(additive_roles);
        if let Some(position) = self
            .config
            .routing
            .models
            .iter()
            .position(|profile| profile.model == model)
        {
            replacement.context_length = self.config.routing.models[position].context_length;
            replacement.auto_routing = self.config.routing.models[position].auto_routing;
            replacement.disabled = self.config.routing.models[position].disabled;
            self.config.routing.models[position] = replacement;
        } else {
            let insertion = self
                .config
                .routing
                .models
                .iter()
                .position(|profile| profile.model.contains('*'))
                .unwrap_or(self.config.routing.models.len());
            self.config.routing.models.insert(insertion, replacement);
        }
        self.sync_routing_models();
    }

    pub fn apply_suggestion(&mut self, model: &str, suggestion: ArchetypeSuggestion) {
        self.apply_archetype(model, suggestion.archetype);
        self.set_vision_role(model, suggestion.vision);
    }

    fn toggle_vision_role(&mut self, model: &str) {
        self.toggle_additive_role(model, RoutingModelRole::Vision);
    }

    pub fn toggle_compression_role(&mut self, model: &str) {
        self.toggle_additive_role(model, RoutingModelRole::Compression);
    }

    fn toggle_additive_role(&mut self, model: &str, role: RoutingModelRole) {
        let enabled = self
            .config
            .routing
            .models
            .iter()
            .find(|profile| profile.model == model)
            .is_some_and(|profile| effective_roles(profile).contains(&role));
        self.set_additive_role(model, role, !enabled);
    }

    fn set_vision_role(&mut self, model: &str, enabled: bool) {
        self.set_additive_role(model, RoutingModelRole::Vision, enabled);
    }

    fn set_additive_role(&mut self, model: &str, role: RoutingModelRole, enabled: bool) {
        debug_assert!(role.is_additive());
        if !self
            .config
            .routing
            .models
            .iter()
            .any(|profile| profile.model == model)
        {
            self.apply_archetype(model, Archetype::Balanced);
        }
        let profile = self
            .config
            .routing
            .models
            .iter_mut()
            .find(|profile| profile.model == model)
            .expect("an exact model profile was created");
        if profile.roles.is_empty() {
            profile.roles = effective_roles(profile);
        }
        profile.roles.retain(|current| *current != role);
        if enabled {
            profile.roles.push(role);
        }
        self.sync_routing_models();
    }

    pub fn remove_exact_profile(&mut self, model: &str) {
        self.config
            .routing
            .models
            .retain(|profile| profile.model != model);
        self.sync_routing_models();
    }

    pub fn toggle_model_auto_routing(&mut self, model: &str) -> bool {
        self.ensure_exact_model_profile(model);
        let profile = self
            .config
            .routing
            .models
            .iter_mut()
            .find(|profile| profile.model == model)
            .expect("an exact model profile was created");
        profile.auto_routing = !profile.auto_routing;
        if !profile.auto_routing {
            profile.disabled = false;
        }
        let enabled = profile.auto_routing;
        self.sync_routing_models();
        enabled
    }

    fn ensure_exact_model_profile(&mut self, model: &str) {
        if self
            .config
            .routing
            .models
            .iter()
            .any(|profile| profile.model == model)
        {
            return;
        }
        let inherited = self
            .config
            .routing
            .models
            .iter()
            .find(|profile| !profile.disabled && wildcard_matches(&profile.model, model))
            .cloned();
        let insertion = self
            .config
            .routing
            .models
            .iter()
            .position(|profile| profile.model.contains('*'))
            .unwrap_or(self.config.routing.models.len());
        self.config.routing.models.insert(insertion, {
            let mut profile = inherited.unwrap_or_else(|| RoutingModelConfig {
                model: model.to_owned(),
                context_length: None,
                auto_routing: true,
                roles: Vec::new(),
                strengths: BTreeMap::new(),
                traits: RoutingModelTraits::default(),
                disabled: false,
            });
            profile.model = model.to_owned();
            profile
        });
    }

    pub fn adjust_strength(&mut self, model: &str, key: &str, delta: f64) {
        let Some(profile) = self
            .config
            .routing
            .models
            .iter_mut()
            .find(|profile| profile.model == model)
        else {
            return;
        };
        let score = profile.strengths.entry(key.to_owned()).or_insert(0.5);
        *score = (*score + delta).clamp(0.0, 1.0);
        self.sync_routing_models();
    }

    pub fn adjust_latency(&mut self, model: &str, delta: i8) {
        let Some(profile) = self
            .config
            .routing
            .models
            .iter_mut()
            .find(|profile| profile.model == model)
        else {
            return;
        };
        profile.traits.latency_tier = (profile.traits.latency_tier as i8 + delta).clamp(0, 4) as u8;
        self.sync_routing_models();
    }

    pub fn adjust_cost(&mut self, model: &str, delta: i8) {
        let Some(profile) = self
            .config
            .routing
            .models
            .iter_mut()
            .find(|profile| profile.model == model)
        else {
            return;
        };
        profile.traits.cost_tier = (profile.traits.cost_tier as i8 + delta).clamp(0, 4) as u8;
        self.sync_routing_models();
    }

    pub fn upsert_provider(
        &mut self,
        provider: ProviderConfig,
        original_id: Option<&str>,
    ) -> Result<()> {
        let original_id = original_id.unwrap_or(&provider.id);
        let mut candidate = self.config.clone();
        if let Some(position) = candidate
            .providers
            .iter()
            .position(|current| current.id == original_id)
        {
            candidate.providers[position] = provider.clone();
        } else {
            candidate.providers.push(provider.clone());
        }
        candidate.validate()?;
        self.config = candidate;
        self.sync_provider(&provider, original_id);
        Ok(())
    }

    pub fn upsert_transcription_provider(
        &mut self,
        provider: ProviderConfig,
        original_id: Option<&str>,
        route_id: &str,
        original_route_id: Option<&str>,
    ) -> Result<()> {
        let static_model = provider
            .models
            .first()
            .context("a transcription provider needs a static model")?;
        let original_id = original_id.unwrap_or(&provider.id);
        let model_id = format!("{}/{}", provider.id, static_model.id);
        let old_prefix = format!("{original_id}/");
        let mut candidate = self.config.clone();
        if let Some(position) = candidate
            .providers
            .iter()
            .position(|current| current.id == original_id)
        {
            candidate.providers[position] = provider.clone();
        } else {
            candidate.providers.push(provider.clone());
        }
        candidate.routing.models.retain(|profile| {
            !(profile.model.starts_with(&old_prefix)
                && profile.roles.contains(&RoutingModelRole::Transcription))
        });
        candidate.routing.models.push(RoutingModelConfig {
            model: model_id.clone(),
            context_length: None,
            auto_routing: false,
            roles: vec![RoutingModelRole::Transcription],
            strengths: BTreeMap::new(),
            traits: RoutingModelTraits::default(),
            disabled: false,
        });
        if let Some(original_route_id) = original_route_id
            && original_route_id != route_id
        {
            candidate
                .routes
                .retain(|route| route.id != original_route_id);
        }
        let route = RouteConfig {
            id: route_id.to_owned(),
            title: "Private transcription".to_owned(),
            description: "Explicit OpenAI-compatible speech-to-text service route.".to_owned(),
            allowed_realms: vec![provider.realm.clone()],
            candidates: vec![model_id],
            strategy: RouteStrategy::FirstAvailable,
            task: RoutingTask::Auto,
            effort: RoutingEffort::Auto,
            context_length: None,
            token_estimate_bytes_per_token: None,
            compression_max_output_tokens: None,
            on_no_eligible_model: NoEligibleModel::Deny,
        };
        if let Some(position) = candidate
            .routes
            .iter()
            .position(|configured| configured.id == route_id)
        {
            let replacing_original = original_route_id == Some(route_id);
            if !replacing_original
                && !candidate.routes[position]
                    .candidates
                    .iter()
                    .any(|model| model.starts_with(&old_prefix))
            {
                anyhow::bail!(
                    "route '{route_id}' already exists and is not owned by this transcription provider"
                );
            }
            candidate.routes[position] = route;
        } else {
            candidate.routes.push(route);
        }
        candidate.validate()?;
        self.config = candidate;
        self.sync_provider(&provider, original_id);
        self.sync_routing_models();
        self.sync_routes();
        Ok(())
    }

    pub fn upsert_audio_provider(
        &mut self,
        provider: ProviderConfig,
        original_id: Option<&str>,
        transcription_route_id: &str,
        original_transcription_route_id: Option<&str>,
        speech_route_id: &str,
        original_speech_route_id: Option<&str>,
    ) -> Result<()> {
        let original_id = original_id.unwrap_or(&provider.id);
        let old_prefix = format!("{original_id}/");
        let mut candidate = self.config.clone();
        if let Some(position) = candidate
            .providers
            .iter()
            .position(|current| current.id == original_id)
        {
            candidate.providers[position] = provider.clone();
        } else {
            candidate.providers.push(provider.clone());
        }
        candidate.routing.models.retain(|profile| {
            !(profile.model.starts_with(&old_prefix)
                && profile.roles.iter().any(|role| {
                    matches!(
                        role,
                        RoutingModelRole::Transcription | RoutingModelRole::Speech
                    )
                }))
        });

        let route_setups = [
            (
                AudioService::Transcription,
                transcription_route_id,
                original_transcription_route_id,
                "Private transcription",
                "Explicit OpenAI-compatible speech-to-text service route.",
            ),
            (
                AudioService::Speech,
                speech_route_id,
                original_speech_route_id,
                "Private speech synthesis",
                "Explicit OpenAI-compatible text-to-speech service route.",
            ),
        ];
        for (service, route_id, original_route_id, title, description) in route_setups {
            let static_model = provider
                .models
                .iter()
                .find(|model| model.supports_audio_service(provider.kind, service))
                .with_context(|| {
                    format!(
                        "an OpenAI audio provider needs a static {} model",
                        service.as_str()
                    )
                })?;
            let model_id = format!("{}/{}", provider.id, static_model.id);
            candidate.routing.models.push(RoutingModelConfig {
                model: model_id.clone(),
                context_length: None,
                auto_routing: false,
                roles: vec![service.routing_role()],
                strengths: BTreeMap::new(),
                traits: RoutingModelTraits::default(),
                disabled: false,
            });
            if let Some(original_route_id) = original_route_id
                && original_route_id != route_id
            {
                candidate
                    .routes
                    .retain(|route| route.id != original_route_id);
            }
            let route = RouteConfig {
                id: route_id.to_owned(),
                title: title.to_owned(),
                description: description.to_owned(),
                allowed_realms: vec![provider.realm.clone()],
                candidates: vec![model_id],
                strategy: RouteStrategy::FirstAvailable,
                task: RoutingTask::Auto,
                effort: RoutingEffort::Auto,
                context_length: None,
                token_estimate_bytes_per_token: None,
                compression_max_output_tokens: None,
                on_no_eligible_model: NoEligibleModel::Deny,
            };
            if let Some(position) = candidate
                .routes
                .iter()
                .position(|configured| configured.id == route_id)
            {
                let replacing_original = original_route_id == Some(route_id);
                if !replacing_original
                    && !candidate.routes[position]
                        .candidates
                        .iter()
                        .any(|model| model.starts_with(&old_prefix))
                {
                    anyhow::bail!(
                        "route '{route_id}' already exists and is not owned by this audio provider"
                    );
                }
                candidate.routes[position] = route;
            } else {
                candidate.routes.push(route);
            }
        }
        candidate.validate()?;
        self.config = candidate;
        self.sync_provider(&provider, original_id);
        self.sync_routing_models();
        self.sync_routes();
        Ok(())
    }

    pub fn toggle_provider_jit(&mut self, index: usize) -> Result<()> {
        let mut provider = self
            .config
            .providers
            .get(index)
            .cloned()
            .context("no provider is selected")?;
        provider.jit_load = !provider.jit_load;
        if provider.jit_load {
            provider.auto_load = false;
        }
        let id = provider.id.clone();
        self.upsert_provider(provider, Some(&id))
    }

    pub fn toggle_provider_auto_load(&mut self, index: usize) -> Result<()> {
        let mut provider = self
            .config
            .providers
            .get(index)
            .cloned()
            .context("no provider is selected")?;
        provider.auto_load = !provider.auto_load;
        if provider.auto_load {
            provider.jit_load = false;
        }
        let id = provider.id.clone();
        self.upsert_provider(provider, Some(&id))
    }

    pub fn toggle_provider_disabled(&mut self, index: usize) -> Result<()> {
        let mut provider = self
            .config
            .providers
            .get(index)
            .cloned()
            .context("no provider is selected")?;
        provider.disabled = !provider.disabled;
        let id = provider.id.clone();
        self.upsert_provider(provider, Some(&id))
    }

    pub fn upsert_client(&mut self, client: ClientConfig, original_id: Option<&str>) -> Result<()> {
        let original_id = original_id.unwrap_or(&client.id);
        let mut candidate = self.config.clone();
        if let Some(position) = candidate
            .clients
            .iter()
            .position(|current| current.id == original_id)
        {
            candidate.clients[position] = client.clone();
        } else {
            candidate.clients.push(client.clone());
        }
        candidate.validate()?;
        self.config = candidate;
        self.sync_client(&client, original_id);
        Ok(())
    }

    pub fn toggle_client_disabled(&mut self, index: usize) -> Result<()> {
        let mut client = self
            .config
            .clients
            .get(index)
            .cloned()
            .context("no client is selected")?;
        client.disabled = !client.disabled;
        let id = client.id.clone();
        self.upsert_client(client, Some(&id))
    }

    pub fn remove_client(&mut self, index: usize) -> Result<ClientConfig> {
        let mut candidate = self.config.clone();
        if index >= candidate.clients.len() {
            anyhow::bail!("no client is selected");
        }
        let removed = candidate.clients.remove(index);
        candidate.validate()?;
        self.config = candidate;
        if let Some(clients) = self
            .document
            .get_mut("clients")
            .and_then(Item::as_array_of_tables_mut)
        {
            let position = clients.iter().position(|table| {
                table
                    .get("id")
                    .and_then(Item::as_str)
                    .is_some_and(|id| id == removed.id)
            });
            if let Some(position) = position {
                clients.remove(position);
            }
        }
        Ok(removed)
    }

    pub fn add_claude_recipe(&mut self, id: String, default_target: String) -> Result<()> {
        let recipe = RecipeConfig {
            id,
            title: "Claude Code".to_owned(),
            kind: RecipeKind::ClaudeCode,
            protocol: RecipeProtocol::Anthropic,
            allowed_realms: vec![],
            slots: vec![
                RecipeSlotConfig {
                    id: "primary".to_owned(),
                    title: "Primary".to_owned(),
                    target: default_target.clone(),
                    trigger: RecipeSlotTrigger::Model,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::Inherit,
                },
                RecipeSlotConfig {
                    id: "background".to_owned(),
                    title: "Background / Haiku".to_owned(),
                    target: default_target.clone(),
                    trigger: RecipeSlotTrigger::Model,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::Inherit,
                },
                RecipeSlotConfig {
                    id: "subagent".to_owned(),
                    title: "Subagent".to_owned(),
                    target: default_target.clone(),
                    trigger: RecipeSlotTrigger::Model,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::Inherit,
                },
                RecipeSlotConfig {
                    id: "classifier".to_owned(),
                    title: "Classifier".to_owned(),
                    target: default_target.clone(),
                    trigger: RecipeSlotTrigger::Classifier,
                    max_output_tokens: Some(1024),
                    reasoning_effort: RecipeReasoningEffort::Inherit,
                },
                RecipeSlotConfig {
                    id: "compression".to_owned(),
                    title: "Compression".to_owned(),
                    target: default_target,
                    trigger: RecipeSlotTrigger::Compression,
                    max_output_tokens: Some(16_384),
                    reasoning_effort: RecipeReasoningEffort::Inherit,
                },
            ],
            disabled: false,
        };
        let mut candidate = self.config.clone();
        candidate.recipes.push(recipe);
        candidate.validate()?;
        self.config = candidate;
        self.sync_recipes();
        Ok(())
    }

    pub fn add_hermes_recipe(&mut self, id: String, default_target: String) -> Result<()> {
        let recipe = RecipeConfig {
            id,
            title: "Hermes".to_owned(),
            kind: RecipeKind::Hermes,
            protocol: RecipeProtocol::Openai,
            allowed_realms: vec![],
            slots: vec![
                RecipeSlotConfig {
                    id: "primary".to_owned(),
                    title: "Primary".to_owned(),
                    target: default_target.clone(),
                    trigger: RecipeSlotTrigger::Model,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::Inherit,
                },
                RecipeSlotConfig {
                    id: "delegation".to_owned(),
                    title: "Delegation / subagent".to_owned(),
                    target: default_target.clone(),
                    trigger: RecipeSlotTrigger::Delegation,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::None,
                },
                RecipeSlotConfig {
                    id: "vision".to_owned(),
                    title: "Vision analysis".to_owned(),
                    target: default_target.clone(),
                    trigger: RecipeSlotTrigger::Vision,
                    max_output_tokens: None,
                    reasoning_effort: RecipeReasoningEffort::None,
                },
                RecipeSlotConfig {
                    id: "compression".to_owned(),
                    title: "Compression".to_owned(),
                    target: default_target,
                    trigger: RecipeSlotTrigger::Compression,
                    max_output_tokens: Some(12_000),
                    reasoning_effort: RecipeReasoningEffort::None,
                },
            ],
            disabled: false,
        };
        let mut candidate = self.config.clone();
        candidate.recipes.push(recipe);
        candidate.validate()?;
        self.config = candidate;
        self.sync_recipes();
        Ok(())
    }

    pub fn upsert_recipe(&mut self, recipe: RecipeConfig, original_id: Option<&str>) -> Result<()> {
        let original_id = original_id
            .map(str::to_owned)
            .unwrap_or_else(|| recipe.id.clone());
        let replacement_id = recipe.id.clone();
        let mut candidate = self.config.clone();
        if let Some(position) = candidate
            .recipes
            .iter()
            .position(|current| current.id == original_id)
        {
            candidate.recipes[position] = recipe.clone();
        } else {
            candidate.recipes.push(recipe);
        }
        if original_id != replacement_id {
            for client in &mut candidate.clients {
                if client.recipe.as_deref() == Some(original_id.as_str()) {
                    client.recipe = Some(replacement_id.clone());
                }
                for recipe_id in &mut client.recipes {
                    if recipe_id == &original_id {
                        *recipe_id = replacement_id.clone();
                    }
                }
            }
        }

        candidate.validate()?;
        self.config = candidate;
        self.sync_recipes();
        let clients = self.config.clients.clone();
        for client in &clients {
            self.sync_client(client, &client.id);
        }
        Ok(())
    }

    pub fn cycle_recipe_slot_target(
        &mut self,
        recipe_index: usize,
        slot_index: usize,
        targets: &[String],
        delta: isize,
    ) -> Result<()> {
        let recipe = self
            .config
            .recipes
            .get_mut(recipe_index)
            .context("no recipe is selected")?;
        let slot = recipe
            .slots
            .get_mut(slot_index)
            .context("no recipe slot is selected")?;
        if targets.is_empty() {
            anyhow::bail!("no model or route targets are available");
        }
        let current = targets
            .iter()
            .position(|target| target == &slot.target)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(targets.len() as isize) as usize;
        slot.target = targets[next].clone();
        self.config.validate()?;
        self.sync_recipes();
        Ok(())
    }

    pub fn set_recipe_slot_target(
        &mut self,
        recipe_index: usize,
        slot_index: usize,
        target: String,
    ) -> Result<()> {
        let slot = self
            .config
            .recipes
            .get_mut(recipe_index)
            .context("no recipe is selected")?
            .slots
            .get_mut(slot_index)
            .context("no recipe slot is selected")?;
        slot.target = target;
        self.config.validate()?;
        self.sync_recipes();
        Ok(())
    }

    pub fn set_recipe_realms(&mut self, index: usize, realms: Vec<String>) -> Result<()> {
        let recipe = self
            .config
            .recipes
            .get_mut(index)
            .context("no recipe is selected")?;
        recipe.allowed_realms = realms;
        self.config.validate()?;
        self.sync_recipes();
        Ok(())
    }

    pub fn cycle_recipe_slot_output_limit(
        &mut self,
        recipe_index: usize,
        slot_index: usize,
    ) -> Result<Option<u64>> {
        const LIMITS: [Option<u64>; 6] = [
            None,
            Some(4_096),
            Some(8_192),
            Some(16_384),
            Some(32_000),
            Some(65_536),
        ];
        let slot = self
            .config
            .recipes
            .get_mut(recipe_index)
            .context("no recipe is selected")?
            .slots
            .get_mut(slot_index)
            .context("no recipe slot is selected")?;
        let current = LIMITS
            .iter()
            .position(|limit| *limit == slot.max_output_tokens)
            .unwrap_or(0);
        slot.max_output_tokens = LIMITS[(current + 1) % LIMITS.len()];
        let selected = slot.max_output_tokens;
        self.config.validate()?;
        self.sync_recipes();
        Ok(selected)
    }

    pub fn cycle_recipe_slot_reasoning_effort(
        &mut self,
        recipe_index: usize,
        slot_index: usize,
    ) -> Result<RecipeReasoningEffort> {
        const EFFORTS: [RecipeReasoningEffort; 9] = [
            RecipeReasoningEffort::Inherit,
            RecipeReasoningEffort::None,
            RecipeReasoningEffort::Minimal,
            RecipeReasoningEffort::Low,
            RecipeReasoningEffort::Medium,
            RecipeReasoningEffort::High,
            RecipeReasoningEffort::Xhigh,
            RecipeReasoningEffort::Max,
            RecipeReasoningEffort::Ultra,
        ];
        let recipe = self
            .config
            .recipes
            .get_mut(recipe_index)
            .context("no recipe is selected")?;
        if recipe.protocol == RecipeProtocol::Anthropic {
            anyhow::bail!(
                "Anthropic recipes inherit the harness thinking object; reasoning_effort applies only to OpenAI-compatible recipes"
            );
        }
        let slot = recipe
            .slots
            .get_mut(slot_index)
            .context("no recipe slot is selected")?;
        let current = EFFORTS
            .iter()
            .position(|effort| *effort == slot.reasoning_effort)
            .unwrap_or(0);
        slot.reasoning_effort = EFFORTS[(current + 1) % EFFORTS.len()];
        let selected = slot.reasoning_effort;
        self.config.validate()?;
        self.sync_recipes();
        Ok(selected)
    }

    pub fn toggle_recipe_disabled(&mut self, index: usize) -> Result<()> {
        let recipe = self
            .config
            .recipes
            .get(index)
            .cloned()
            .context("no recipe is selected")?;
        if !recipe.disabled
            && self
                .config
                .clients
                .iter()
                .any(|client| client.authorizes_recipe(&recipe.id))
        {
            anyhow::bail!(
                "recipe '{}' is bound to an enabled client; unbind it before disabling",
                recipe.id
            );
        }
        let recipe = self
            .config
            .recipes
            .get_mut(index)
            .expect("recipe index was already checked");
        recipe.disabled = !recipe.disabled;
        self.sync_recipes();
        Ok(())
    }

    pub fn remove_recipe(&mut self, index: usize) -> Result<RecipeConfig> {
        let recipe = self
            .config
            .recipes
            .get(index)
            .cloned()
            .context("no recipe is selected")?;
        if self
            .config
            .clients
            .iter()
            .any(|client| client.authorizes_recipe(&recipe.id))
        {
            anyhow::bail!(
                "recipe '{}' is still bound to a named client; unbind it first",
                recipe.id
            );
        }
        let removed = self.config.recipes.remove(index);
        self.config.validate()?;
        self.sync_recipes();
        Ok(removed)
    }

    pub fn upsert_alias(
        &mut self,
        alias: ModelAliasConfig,
        original_id: Option<&str>,
    ) -> Result<()> {
        let original_id = original_id
            .map(str::to_owned)
            .unwrap_or_else(|| alias.id.clone());
        let mut candidate = self.config.clone();
        if let Some(position) = candidate
            .aliases
            .iter()
            .position(|current| current.id == original_id)
        {
            candidate.aliases[position] = alias;
        } else {
            candidate.aliases.push(alias);
        }
        candidate.validate()?;
        self.config = candidate;
        self.sync_aliases();
        Ok(())
    }

    pub fn set_alias_target(&mut self, index: usize, target: String) -> Result<()> {
        let alias = self
            .config
            .aliases
            .get_mut(index)
            .context("no alias is selected")?;
        alias.target = target;
        self.config.validate()?;
        self.sync_aliases();
        Ok(())
    }

    pub fn cycle_alias_context_length(
        &mut self,
        index: usize,
        forward: bool,
    ) -> Result<Option<u64>> {
        let alias = self
            .config
            .aliases
            .get_mut(index)
            .context("no alias is selected")?;
        alias.context_length = next_context_tier(alias.context_length, forward);
        let selected = alias.context_length;
        self.config.validate()?;
        self.sync_aliases();
        Ok(selected)
    }

    pub fn toggle_alias_disabled(&mut self, index: usize) -> Result<()> {
        let alias = self
            .config
            .aliases
            .get_mut(index)
            .context("no alias is selected")?;
        alias.disabled = !alias.disabled;
        self.sync_aliases();
        Ok(())
    }

    pub fn remove_alias(&mut self, index: usize) -> Result<ModelAliasConfig> {
        if index >= self.config.aliases.len() {
            anyhow::bail!("no alias is selected");
        }
        let removed = self.config.aliases.remove(index);
        self.config.validate()?;
        self.sync_aliases();
        Ok(removed)
    }

    pub fn rendered(&self) -> String {
        self.document.to_string()
    }

    pub fn validate_rendered(&self) -> Result<Config> {
        let rendered = self.rendered();
        let config: Config = toml::from_str(&rendered).context("generated TOML is invalid")?;
        config.validate()?;
        Ok(config)
    }

    pub fn diff(&self) -> Vec<String> {
        line_diff(&self.original, &self.rendered())
    }

    pub fn save(&mut self) -> Result<PathBuf> {
        let _lock = crate::storage::lock(&self.path.with_extension("toml.lock"))?;
        anyhow::ensure!(
            fs::read_to_string(&self.path)? == self.original,
            "configuration changed on disk; reopen the editor before saving to avoid overwriting another edit"
        );
        let config = self.validate_rendered()?;
        let rendered = self.rendered();
        let timestamp = format!("{}-{:032x}", unix_time(), rand::random::<u128>());
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("infernaut.toml");
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let backup = parent.join(format!("{file_name}.bak-{timestamp}"));
        fs::copy(&self.path, &backup)
            .with_context(|| format!("failed to create backup {}", backup.display()))?;

        let temporary = parent.join(format!(".{file_name}.tmp-{:032x}", rand::random::<u128>()));
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        output.set_permissions(fs::metadata(&self.path)?.permissions())?;
        output.write_all(rendered.as_bytes())?;
        output.sync_all()?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("failed to atomically replace {}", self.path.display()))?;

        self.original = rendered;
        self.document = self.original.parse::<DocumentMut>()?;
        self.config = config;
        Ok(backup)
    }

    fn sync_routing_models(&mut self) {
        let mut profiles = ArrayOfTables::new();
        for profile in &self.config.routing.models {
            let mut table = Table::new();
            table["model"] = value(&profile.model);
            if let Some(context_length) = profile.context_length {
                table["context_length"] = value(context_length as i64);
            }
            if profile.disabled {
                table["disabled"] = value(true);
            }
            if !profile.auto_routing {
                table["auto_routing"] = value(false);
            }
            if !profile.roles.is_empty() {
                let mut roles = Array::new();
                for role in &profile.roles {
                    roles.push(role.as_str());
                }
                table["roles"] = value(roles);
            }

            let mut strengths = Table::new();
            for (key, score) in &profile.strengths {
                strengths[key] = value(*score);
            }
            table["strengths"] = Item::Table(strengths);

            let mut traits = Table::new();
            if !profile.traits.architecture.is_empty() {
                traits["architecture"] = value(&profile.traits.architecture);
            }
            traits["latency_tier"] = value(i64::from(profile.traits.latency_tier));
            traits["cost_tier"] = value(i64::from(profile.traits.cost_tier));
            let mut efforts = Array::new();
            for effort in &profile.traits.preferred_effort {
                efforts.push(effort.as_str());
            }
            if !efforts.is_empty() {
                traits["preferred_effort"] = value(efforts);
            }
            table["traits"] = Item::Table(traits);
            profiles.push(table);
        }
        self.ensure_top_level_table("routing");
        self.document["routing"]["models"] = Item::ArrayOfTables(profiles);
    }

    fn ensure_top_level_table(&mut self, key: &str) {
        if !self.document.get(key).is_some_and(Item::is_table) {
            self.document[key] = Item::Table(Table::new());
        }
    }

    fn sync_routes(&mut self) {
        let mut routes = ArrayOfTables::new();
        for route in &self.config.routes {
            let mut table = Table::new();
            table["id"] = value(&route.id);
            if !route.title.is_empty() {
                table["title"] = value(&route.title);
            }
            if !route.description.is_empty() {
                table["description"] = value(&route.description);
            }
            table["strategy"] = value(match route.strategy {
                RouteStrategy::FirstAvailable => "first_available",
                RouteStrategy::Adaptive => "adaptive",
            });
            table["task"] = value(route.task.as_str());
            table["effort"] = value(route.effort.as_str());
            if let Some(context_length) = route.context_length {
                table["context_length"] = value(context_length as i64);
            }
            if let Some(bytes_per_token) = route.token_estimate_bytes_per_token {
                table["token_estimate_bytes_per_token"] = value(bytes_per_token);
            }
            if let Some(tokens) = route.compression_max_output_tokens {
                table["compression_max_output_tokens"] = value(tokens as i64);
            }

            let mut realms = Array::new();
            for realm in &route.allowed_realms {
                realms.push(realm.as_str());
            }
            if !realms.is_empty() {
                table["allowed_realms"] = value(realms);
            }

            let mut candidates = Array::new();
            for candidate in &route.candidates {
                candidates.push(candidate.as_str());
            }
            if !candidates.is_empty() {
                table["candidates"] = value(candidates);
            }
            table["on_no_eligible_model"] = value("deny");
            routes.push(table);
        }
        self.document["routes"] = Item::ArrayOfTables(routes);
    }

    fn sync_aliases(&mut self) {
        let mut aliases = ArrayOfTables::new();
        for alias in &self.config.aliases {
            let mut table = Table::new();
            table["id"] = value(&alias.id);
            if !alias.title.is_empty() {
                table["title"] = value(&alias.title);
            }
            table["target"] = value(&alias.target);
            if let Some(context_length) = alias.context_length {
                table["context_length"] = value(context_length as i64);
            }
            set_bool(&mut table, "disabled", alias.disabled);
            aliases.push(table);
        }
        self.document["aliases"] = Item::ArrayOfTables(aliases);
    }

    fn sync_recipes(&mut self) {
        let mut recipes = ArrayOfTables::new();
        for recipe in &self.config.recipes {
            let mut table = Table::new();
            table["id"] = value(&recipe.id);
            if !recipe.title.is_empty() {
                table["title"] = value(&recipe.title);
            }
            table["kind"] = value(match recipe.kind {
                RecipeKind::Generic => "generic",
                RecipeKind::ClaudeCode => "claude_code",
                RecipeKind::Hermes => "hermes",
            });
            table["protocol"] = value(match recipe.protocol {
                RecipeProtocol::Any => "any",
                RecipeProtocol::Openai => "openai",
                RecipeProtocol::Anthropic => "anthropic",
            });
            let mut realms = Array::new();
            for realm in &recipe.allowed_realms {
                realms.push(realm.as_str());
            }
            if !realms.is_empty() {
                table["allowed_realms"] = value(realms);
            }
            set_bool(&mut table, "disabled", recipe.disabled);
            let mut slots = ArrayOfTables::new();
            for slot in &recipe.slots {
                let mut slot_table = Table::new();
                slot_table["id"] = value(&slot.id);
                if !slot.title.is_empty() {
                    slot_table["title"] = value(&slot.title);
                }
                slot_table["target"] = value(&slot.target);
                slot_table["trigger"] = value(match slot.trigger {
                    RecipeSlotTrigger::Model => "model",
                    RecipeSlotTrigger::Classifier => "classifier",
                    RecipeSlotTrigger::Compression => "compression",
                    RecipeSlotTrigger::Delegation => "delegation",
                    RecipeSlotTrigger::Vision => "vision",
                });
                if let Some(tokens) = slot.max_output_tokens {
                    slot_table["max_output_tokens"] = value(tokens as i64);
                }
                if slot.reasoning_effort != RecipeReasoningEffort::Inherit {
                    slot_table["reasoning_effort"] = value(slot.reasoning_effort.as_str());
                }
                slots.push(slot_table);
            }
            table["slots"] = Item::ArrayOfTables(slots);
            recipes.push(table);
        }
        self.document["recipes"] = Item::ArrayOfTables(recipes);
    }

    fn sync_provider(&mut self, provider: &ProviderConfig, original_id: &str) {
        if !self
            .document
            .get("providers")
            .is_some_and(Item::is_array_of_tables)
        {
            self.document["providers"] = Item::ArrayOfTables(ArrayOfTables::new());
        }
        let providers = self
            .document
            .get_mut("providers")
            .and_then(Item::as_array_of_tables_mut)
            .expect("providers is an array of tables");
        let position = providers.iter().position(|table| {
            table
                .get("id")
                .and_then(Item::as_str)
                .is_some_and(|id| id == original_id)
        });
        let table = if let Some(position) = position {
            providers
                .get_mut(position)
                .expect("provider position exists")
        } else {
            providers.push(Table::new());
            providers
                .get_mut(providers.len() - 1)
                .expect("new provider exists")
        };
        table["id"] = value(&provider.id);
        table["kind"] = value(provider_kind_name(provider.kind));
        table["title"] = value(&provider.title);
        table["base_url"] = value(&provider.base_url);
        table["realm"] = value(&provider.realm);
        table["priority"] = value(i64::from(provider.priority));
        set_optional_string(table, "alias", provider.alias.as_deref());
        set_optional_string(table, "api_key_env", provider.api_key_env.as_deref());
        set_optional_string(
            table,
            "oauth_client_id",
            provider.oauth_client_id.as_deref(),
        );
        if let Some(enabled) = provider.anthropic_compat {
            table["anthropic_compat"] = value(enabled);
        } else {
            table.remove("anthropic_compat");
        }
        set_bool(table, "auto_load", provider.auto_load);
        set_bool(table, "jit_load", provider.jit_load);
        if let Some(context) = provider.load_context_length {
            table["load_context_length"] = value(context as i64);
        } else {
            table.remove("load_context_length");
        }
        set_bool(table, "disabled", provider.disabled);
        if provider.models.is_empty() {
            table.remove("models");
        } else {
            let mut models = ArrayOfTables::new();
            for model in &provider.models {
                let mut model_table = Table::new();
                model_table["id"] = value(&model.id);
                if !model.upstream_id.is_empty() {
                    model_table["upstream_id"] = value(&model.upstream_id);
                }
                if !model.title.is_empty() {
                    model_table["title"] = value(&model.title);
                }
                if !model.services.is_empty() {
                    let mut services = Array::new();
                    for service in &model.services {
                        services.push(service.as_str());
                    }
                    model_table["services"] = value(services);
                }
                models.push(model_table);
            }
            table["models"] = Item::ArrayOfTables(models);
        }
    }

    fn sync_client(&mut self, client: &ClientConfig, original_id: &str) {
        if !self
            .document
            .get("clients")
            .is_some_and(Item::is_array_of_tables)
        {
            self.document["clients"] = Item::ArrayOfTables(ArrayOfTables::new());
        }
        let clients = self
            .document
            .get_mut("clients")
            .and_then(Item::as_array_of_tables_mut)
            .expect("clients is an array of tables");
        let position = clients.iter().position(|table| {
            table
                .get("id")
                .and_then(Item::as_str)
                .is_some_and(|id| id == original_id)
        });
        let table = if let Some(position) = position {
            clients.get_mut(position).expect("client position exists")
        } else {
            clients.push(Table::new());
            clients
                .get_mut(clients.len() - 1)
                .expect("new client exists")
        };
        table["id"] = value(&client.id);
        table["title"] = value(&client.title);
        table["api_key_env"] = value(&client.api_key_env);
        set_optional_string(table, "recipe", client.recipe.as_deref());
        if client.recipes.is_empty() {
            table.remove("recipes");
        } else {
            let mut recipes = Array::new();
            for recipe in &client.recipes {
                recipes.push(recipe.as_str());
            }
            table["recipes"] = value(recipes);
        }
        set_bool(table, "disabled", client.disabled);
    }
}

fn set_optional_string(table: &mut Table, key: &str, value_: Option<&str>) {
    if let Some(value_) = value_.filter(|value_| !value_.is_empty()) {
        table[key] = value(value_);
    } else {
        table.remove(key);
    }
}

fn set_bool(table: &mut Table, key: &str, enabled: bool) {
    if enabled {
        table[key] = value(true);
    } else {
        table.remove(key);
    }
}

pub fn reconcile_models(config: &Config, catalog: &Catalog) -> Vec<ModelReconciliation> {
    let mut rows = Vec::new();
    let mut matched_profiles = HashSet::new();
    for model in &catalog.models {
        let matched = config
            .routing
            .models
            .iter()
            .enumerate()
            .find(|(_, profile)| !profile.disabled && wildcard_matches(&profile.model, &model.id));
        let (state, matched_profile, auto_routing) = match matched {
            Some((index, profile)) => {
                matched_profiles.insert(index);
                (
                    if profile.model == model.id {
                        ModelProfileState::Exact
                    } else {
                        ModelProfileState::Wildcard
                    },
                    Some(profile.model.clone()),
                    Some(profile.auto_routing),
                )
            }
            None => (ModelProfileState::Unclassified, None, None),
        };
        rows.push(ModelReconciliation {
            model: model.id.clone(),
            provider: model.provider.clone(),
            state,
            matched_profile,
            auto_routing,
            loaded: model.loaded,
        });
    }
    for (index, profile) in config.routing.models.iter().enumerate() {
        if !matched_profiles.contains(&index) {
            rows.push(ModelReconciliation {
                model: profile.model.clone(),
                provider: profile.model.split('/').next().unwrap_or("").to_owned(),
                state: ModelProfileState::Dormant,
                matched_profile: Some(profile.model.clone()),
                auto_routing: Some(profile.auto_routing),
                loaded: None,
            });
        }
    }
    rows.sort_by(|left, right| {
        state_order(&left.state)
            .cmp(&state_order(&right.state))
            .then_with(|| left.model.cmp(&right.model))
    });
    rows
}

pub async fn doctor(path: impl AsRef<Path>) -> Result<(Config, Catalog, DoctorReport)> {
    let path = path.as_ref();
    let config = Config::load(path)?;
    doctor_config(config, path.display().to_string()).await
}

pub async fn doctor_config(
    config: Config,
    config_label: impl Into<String>,
) -> Result<(Config, Catalog, DoctorReport)> {
    let config_label = config_label.into();
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .user_agent(format!("infernaut-config/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let auth = AuthManager::new()?;
    let catalog = Catalog::discover(Arc::new(config.clone()), client, auth).await;
    let models = reconcile_models(&config, &catalog);
    let mut issues = Vec::new();

    if let Some(environment) = &config.server.api_key_env
        && std::env::var(environment).map_or(true, |value| value.is_empty())
    {
        issues.push(DoctorIssue {
            severity: "error",
            code: "server_api_key_missing",
            message: format!("server API key environment variable '{environment}' is not set"),
        });
    }
    for provider in &catalog.providers {
        if !provider.available {
            issues.push(DoctorIssue {
                severity: "warning",
                code: "provider_unavailable",
                message: format!(
                    "provider '{}' is unavailable: {}",
                    provider.id,
                    provider.error.as_deref().unwrap_or("unknown error")
                ),
            });
        }
    }
    let unclassified = models
        .iter()
        .filter(|model| model.state == ModelProfileState::Unclassified)
        .count();
    if unclassified > 0 {
        issues.push(DoctorIssue {
            severity: "warning",
            code: "models_unclassified",
            message: format!("{unclassified} discovered models have no active routing profile"),
        });
    }
    let dormant = models
        .iter()
        .filter(|model| model.state == ModelProfileState::Dormant)
        .count();
    if dormant > 0 {
        issues.push(DoctorIssue {
            severity: "info",
            code: "profiles_dormant",
            message: format!(
                "{dormant} routing profiles currently match no discovered model; they were retained"
            ),
        });
    }
    let reserved = models
        .iter()
        .filter(|model| model.auto_routing == Some(false))
        .count();
    if reserved > 0 {
        issues.push(DoctorIssue {
            severity: "info",
            code: "models_reserved_for_direct_use",
            message: format!(
                "{reserved} model profiles are excluded from adaptive routing but remain directly addressable"
            ),
        });
    }
    for route in &config.routes {
        let Some(context_length) = route.context_length else {
            continue;
        };
        let pool = catalog.route_context_pool(&config, route);
        if pool.eligible == 0 {
            issues.push(DoctorIssue {
                severity: "error",
                code: "route_context_contract_unsatisfied",
                message: format!(
                    "route '{}' requires context_length = {context_length}, but none of its {} candidate models report enough context",
                    route.id, pool.total
                ),
            });
        }
        if pool.unknown > 0 {
            issues.push(DoctorIssue {
                severity: "warning",
                code: "route_context_unknown_models_excluded",
                message: format!(
                    "route '{}' excludes {} candidate models whose context length is unknown",
                    route.id, pool.unknown
                ),
            });
        }
        if pool.below_contract > 0 {
            issues.push(DoctorIssue {
                severity: "info",
                code: "route_context_small_models_excluded",
                message: format!(
                    "route '{}' excludes {} candidate models below its context contract",
                    route.id, pool.below_contract
                ),
            });
        }
    }
    for alias in config.aliases.iter().filter(|alias| !alias.disabled) {
        if !catalog.target_available(&config, &alias.target) {
            issues.push(DoctorIssue {
                severity: "warning",
                code: "alias_target_unavailable",
                message: format!(
                    "alias '{}' target '{}' is not currently available; requests will fail closed",
                    alias.id, alias.target
                ),
            });
            continue;
        }
        if let Some(contract) = alias.context_length {
            match catalog.target_context_length(&config, &alias.target) {
                Some(limit) if limit < contract => issues.push(DoctorIssue {
                    severity: "error",
                    code: "alias_context_contract_unsatisfied",
                    message: format!(
                        "alias '{}' requires context_length = {contract}, but target '{}' reports {limit}",
                        alias.id, alias.target
                    ),
                }),
                None => issues.push(DoctorIssue {
                    severity: "warning",
                    code: "alias_context_unknown",
                    message: format!(
                        "alias '{}' has a context contract of {contract}, but target '{}' reports no context length",
                        alias.id, alias.target
                    ),
                }),
                _ => {}
            }
        }
    }
    for recipe in config.recipes.iter().filter(|recipe| !recipe.disabled) {
        for slot in &recipe.slots {
            if !catalog.target_available(&config, &slot.target) {
                issues.push(DoctorIssue {
                    severity: "warning",
                    code: "recipe_target_unavailable",
                    message: format!(
                        "recipe '{}' slot '{}' target '{}' is not currently available; the durable mapping was retained",
                        recipe.id, slot.id, slot.target
                    ),
                });
            }
        }
        let primary_context = recipe
            .slots
            .iter()
            .find(|slot| slot.id == "primary")
            .and_then(|slot| catalog.target_context_length(&config, &slot.target));
        for slot_id in ["background", "classifier", "subagent", "compression"] {
            let Some(slot) = recipe.slots.iter().find(|slot| slot.id == slot_id) else {
                continue;
            };
            let slot_context = catalog.target_context_length(&config, &slot.target);
            if let (Some(primary), Some(slot_limit)) = (primary_context, slot_context)
                && slot_limit < primary
            {
                issues.push(DoctorIssue {
                    severity: "warning",
                    code: "recipe_slot_context_smaller_than_primary",
                    message: format!(
                        "recipe '{}' {} slot advertises {} tokens while primary advertises {}; full-history {} calls may fail",
                        recipe.id, slot_id, slot_limit, primary, slot_id
                    ),
                });
            }
        }
    }
    let report = DoctorReport {
        config: config_label,
        providers: catalog.providers.clone(),
        models,
        issues,
    };
    Ok((config, catalog, report))
}

pub fn provider_kind_name(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenaiCompatible => "openai_compatible",
        ProviderKind::LmStudio => "lm_studio",
        ProviderKind::LlamaCpp => "llama_cpp",
        ProviderKind::Vllm => "vllm",
        ProviderKind::Openai => "openai",
        ProviderKind::Xai => "xai",
        ProviderKind::OllamaCloud => "ollama_cloud",
        ProviderKind::OpenaiCodex => "openai_codex",
        ProviderKind::XaiOauth => "xai_oauth",
        ProviderKind::OpenaiTranscription => "openai_transcription",
        ProviderKind::OpenaiAudio => "openai_audio",
    }
}

pub fn routing_mode_name(mode: RoutingMode) -> &'static str {
    match mode {
        RoutingMode::Off => "off",
        RoutingMode::Shadow => "shadow",
        RoutingMode::Active => "active",
    }
}

pub fn loaded_model_preference_name(preference: LoadedModelPreference) -> &'static str {
    preference.as_str()
}

pub fn privacy_mode_name(mode: PrivacyMode) -> &'static str {
    match mode {
        PrivacyMode::Off => "off",
        PrivacyMode::Selective => "selective",
        PrivacyMode::LocalOnly => "local_only",
    }
}

fn state_order(state: &ModelProfileState) -> u8 {
    match state {
        ModelProfileState::Unclassified => 0,
        ModelProfileState::Exact => 1,
        ModelProfileState::Wildcard => 2,
        ModelProfileState::Dormant => 3,
    }
}

use crate::config::wildcard_matches;

fn line_diff(old: &str, new: &str) -> Vec<String> {
    let old = old.lines().collect::<Vec<_>>();
    let new = new.lines().collect::<Vec<_>>();
    let mut lengths = vec![vec![0usize; new.len() + 1]; old.len() + 1];
    for left in (0..old.len()).rev() {
        for right in (0..new.len()).rev() {
            lengths[left][right] = if old[left] == new[right] {
                lengths[left + 1][right + 1] + 1
            } else {
                lengths[left + 1][right].max(lengths[left][right + 1])
            };
        }
    }
    let (mut left, mut right) = (0, 0);
    let mut diff = Vec::new();
    while left < old.len() || right < new.len() {
        if left < old.len() && right < new.len() && old[left] == new[right] {
            left += 1;
            right += 1;
        } else if right < new.len()
            && (left == old.len() || lengths[left][right + 1] > lengths[left + 1][right])
        {
            diff.push(format!("+ {}", new[right]));
            right += 1;
        } else {
            diff.push(format!("- {}", old[left]));
            left += 1;
        }
    }
    if diff.is_empty() {
        vec!["No changes.".to_owned()]
    } else {
        diff
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
    use crate::catalog::DiscoveredModel;
    use serde_json::json;

    fn model(id: &str) -> DiscoveredModel {
        DiscoveredModel {
            id: id.to_owned(),
            provider: id.split('/').next().unwrap().to_owned(),
            provider_title: String::new(),
            upstream_id: id.split_once('/').unwrap().1.to_owned(),
            management_id: None,
            loaded: Some(true),
            realm: "local".to_owned(),
            created: 0,
            provider_priority: 1,
            capabilities: json!({}),
            provider_metadata: json!({}),
        }
    }

    #[test]
    fn reconciliation_retains_dormant_and_surfaces_new_models() {
        let mut config: Config = toml::from_str(
            r#"
            [[providers]]
            id = "lab"
            base_url = "http://lab/v1"

            [[routing.models]]
            model = "lab/old"
            [routing.models.strengths]
            default = 0.8
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        config
            .routing
            .models
            .push(Archetype::Balanced.profile("lab/known".to_owned()));
        let catalog = Catalog {
            models: vec![model("lab/known"), model("lab/new")],
            providers: vec![],
            refreshed_at: 0,
        };
        let rows = reconcile_models(&config, &catalog);
        assert!(
            rows.iter()
                .any(|row| { row.model == "lab/old" && row.state == ModelProfileState::Dormant })
        );
        assert!(
            rows.iter().any(|row| {
                row.model == "lab/new" && row.state == ModelProfileState::Unclassified
            })
        );
    }

    #[test]
    fn archetypes_produce_valid_strengths() {
        for archetype in Archetype::ALL {
            let profile = archetype.profile("lab/model".to_owned());
            assert!(
                profile
                    .strengths
                    .values()
                    .all(|score| (0.0..=1.0).contains(score))
            );
        }
        let heavy = Archetype::HeavyDevelopment.profile("lab/heavy".to_owned());
        assert_eq!(heavy.strengths["code.implement"], 0.99);
        assert_eq!(heavy.strengths["data.analyze"], 1.0);
        assert_eq!(applied_roles_name(&heavy), "Heavy + data");
        let mut multimodal_compressor = heavy.clone();
        multimodal_compressor
            .roles
            .extend([RoutingModelRole::Vision, RoutingModelRole::Compression]);
        assert_eq!(
            applied_roles_name(&multimodal_compressor),
            "Heavy + data + Vision + Compression"
        );

        let legacy = Archetype::LegacyDataSpecialist.profile("lab/legacy".to_owned());
        assert_eq!(applied_roles_name(&legacy), "Heavy + data");
    }

    #[test]
    fn suggestions_are_conservative_and_specialist_markers_win() {
        let mut discovered = model("lab/qwen-coder-32b");
        let suggestion = suggest_archetype(&discovered);
        assert_eq!(suggestion.archetype, Archetype::HeavyDevelopment);
        assert!(!suggestion.vision);

        discovered.id = "lab/pixtral-12b".to_owned();
        discovered.capabilities = json!({"vision": {"support": "supported"}});
        let suggestion = suggest_archetype(&discovered);
        assert_eq!(suggestion.archetype, Archetype::Balanced);
        assert!(suggestion.vision);

        discovered.id = "lab/unknown-next-model".to_owned();
        discovered.capabilities = json!({});
        let suggestion = suggest_archetype(&discovered);
        assert_eq!(suggestion.archetype, Archetype::Balanced);
        assert!(!suggestion.vision);
    }

    #[test]
    fn save_preserves_untouched_comments_and_creates_valid_backup() {
        let directory = std::env::temp_dir().join(format!(
            "infernaut-config-editor-test-{}-{}",
            std::process::id(),
            unix_time()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("infernaut.toml");
        fs::write(
            &path,
            r#"# keep this operator note
[privacy_lock]
mode = "selective"

[[providers]]
id = "lab"
base_url = "http://127.0.0.1:8000/v1"
"#,
        )
        .unwrap();
        let mut editor = ConfigEditor::load(&path).unwrap();
        let mut stale_editor = ConfigEditor::load(&path).unwrap();
        editor.cycle_privacy_mode();
        assert!(!editor.toggle_model_auto_routing("lab/compressor"));
        assert_eq!(
            editor.cycle_model_context_length("lab/compressor", true),
            Some(32 * 1024)
        );
        editor.apply_archetype("lab/compressor", Archetype::FastGeneralist);
        editor.apply_archetype("lab/compressor", Archetype::VisionSpecialist);
        editor.toggle_compression_role("lab/compressor");
        assert!(
            !editor
                .config
                .routing
                .models
                .iter()
                .find(|profile| profile.model == "lab/compressor")
                .unwrap()
                .auto_routing
        );
        assert_eq!(
            editor
                .config
                .routing
                .models
                .iter()
                .find(|profile| profile.model == "lab/compressor")
                .unwrap()
                .roles,
            vec![
                RoutingModelRole::Fast,
                RoutingModelRole::Vision,
                RoutingModelRole::Compression
            ]
        );
        let backup = editor.save().unwrap();
        assert!(
            stale_editor
                .save()
                .unwrap_err()
                .to_string()
                .contains("changed on disk")
        );
        let second_backup = editor.save().unwrap();
        assert_ne!(backup, second_backup);

        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# keep this operator note"));
        assert!(saved.contains("mode = \"local_only\""));
        assert!(saved.contains("model = \"lab/compressor\""));
        assert!(saved.contains("context_length = 32768"));
        assert!(saved.contains("auto_routing = false"));
        assert!(saved.contains("roles = [\"fast\", \"vision\", \"compression\"]"));
        Config::load(&backup).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn named_client_edits_validate_and_preserve_the_toml_boundary() {
        let directory = std::env::temp_dir().join(format!(
            "infernaut-client-editor-test-{}-{}",
            std::process::id(),
            unix_time()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("infernaut.toml");
        fs::write(
            &path,
            r#"[[clients]]
# keep the client note
id = "old-client"
title = "Old Client"
api_key_env = "INFERNAUT_OLD_CLIENT_KEY"

[[providers]]
id = "lab"
base_url = "http://127.0.0.1:8000/v1"
"#,
        )
        .unwrap();
        let mut editor = ConfigEditor::load(&path).unwrap();
        editor
            .upsert_client(
                ClientConfig {
                    id: "claude-code".into(),
                    title: "Claude Code".into(),
                    api_key_env: "INFERNAUT_CLAUDE_CODE_KEY".into(),
                    recipe: None,
                    recipes: vec![],
                    disabled: false,
                },
                Some("old-client"),
            )
            .unwrap();
        editor.toggle_client_disabled(0).unwrap();
        editor
            .upsert_client(
                ClientConfig {
                    id: "hermes".into(),
                    title: "Hermes Telegram".into(),
                    api_key_env: "INFERNAUT_HERMES_KEY".into(),
                    recipe: None,
                    recipes: vec![],
                    disabled: false,
                },
                None,
            )
            .unwrap();

        let rendered = editor.rendered();
        assert!(rendered.contains("# keep the client note"));
        assert!(rendered.contains("id = \"claude-code\""));
        assert!(rendered.contains("disabled = true"));
        assert!(rendered.contains("api_key_env = \"INFERNAUT_HERMES_KEY\""));
        editor.validate_rendered().unwrap();

        let removed = editor.remove_client(0).unwrap();
        assert_eq!(removed.id, "claude-code");
        assert!(!editor.rendered().contains("id = \"claude-code\""));
        assert!(editor.rendered().contains("id = \"hermes\""));
        editor.validate_rendered().unwrap();
        editor.remove_client(0).unwrap();
        assert!(editor.validate_rendered().unwrap().clients.is_empty());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn first_named_client_creates_the_missing_toml_array() {
        let directory = std::env::temp_dir().join(format!(
            "infernaut-first-client-editor-test-{}-{}",
            std::process::id(),
            unix_time()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("infernaut.toml");
        fs::write(
            &path,
            r#"[[providers]]
id = "lab"
base_url = "http://127.0.0.1:8000/v1"
"#,
        )
        .unwrap();
        let mut editor = ConfigEditor::load(&path).unwrap();
        editor
            .upsert_client(
                ClientConfig {
                    id: "claude-code".into(),
                    title: "Claude Code".into(),
                    api_key_env: "INFERNAUT_CLAUDE_CODE_KEY".into(),
                    recipe: None,
                    recipes: vec![],
                    disabled: false,
                },
                None,
            )
            .unwrap();

        let rendered = editor.rendered();
        assert!(rendered.contains("[[clients]]"));
        assert!(rendered.contains("id = \"claude-code\""));
        assert!(rendered.contains("title = \"Claude Code\""));
        assert!(rendered.contains("api_key_env = \"INFERNAUT_CLAUDE_CODE_KEY\""));
        let validated = editor.validate_rendered().unwrap();
        assert_eq!(validated.clients.len(), 1);
        assert_eq!(validated.clients[0].id, "claude-code");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn transcription_provider_builder_persists_static_model_profile_and_route() {
        let directory = std::env::temp_dir().join(format!(
            "infernaut-transcription-editor-test-{}-{}",
            std::process::id(),
            unix_time()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("infernaut.toml");
        fs::write(
            &path,
            r#"[[providers]]
id = "lab"
base_url = "http://127.0.0.1:8000/v1"
"#,
        )
        .unwrap();
        let mut editor = ConfigEditor::load(&path).unwrap();
        editor
            .upsert_transcription_provider(
                ProviderConfig {
                    id: "speech".into(),
                    kind: ProviderKind::OpenaiTranscription,
                    title: "Nemotron Parakeet".into(),
                    alias: Some("stt".into()),
                    base_url: "http://agamotto:5090/v1".into(),
                    realm: "local".into(),
                    api_key_env: None,
                    oauth_client_id: None,
                    anthropic_compat: None,
                    models: vec![crate::config::StaticModelConfig {
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
                },
                None,
                "private/transcription",
                None,
            )
            .unwrap();

        let rendered = editor.rendered();
        assert!(rendered.contains("kind = \"openai_transcription\""));
        assert!(rendered.contains("[[providers.models]]"));
        assert!(rendered.contains("upstream_id = \"parakeet\""));
        assert!(rendered.contains("roles = [\"transcription\"]"));
        assert!(rendered.contains("id = \"private/transcription\""));
        let validated = editor.validate_rendered().unwrap();
        let provider = validated.provider("speech").unwrap();
        assert_eq!(provider.models[0].id, "nemotron-parakeet");
        assert_eq!(validated.routes[0].candidates, ["speech/nemotron-parakeet"]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn audio_provider_builder_persists_independent_stt_and_tts_routes() {
        let directory = std::env::temp_dir().join(format!(
            "infernaut-audio-editor-test-{}-{}",
            std::process::id(),
            unix_time()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("infernaut.toml");
        fs::write(
            &path,
            r#"[[providers]]
id = "lab"
base_url = "http://127.0.0.1:8000/v1"
"#,
        )
        .unwrap();
        let mut editor = ConfigEditor::load(&path).unwrap();
        editor
            .upsert_audio_provider(
                ProviderConfig {
                    id: "speech".into(),
                    kind: ProviderKind::OpenaiAudio,
                    title: "Agamotto Audio".into(),
                    alias: Some("audio".into()),
                    base_url: "http://agamotto:8080/v1".into(),
                    realm: "local".into(),
                    api_key_env: None,
                    oauth_client_id: None,
                    anthropic_compat: None,
                    models: vec![
                        crate::config::StaticModelConfig {
                            id: "nemotron-3.5-asr".into(),
                            upstream_id: "nemotron-3.5-asr".into(),
                            title: "Nemotron 3.5 ASR".into(),
                            services: vec![AudioService::Transcription],
                        },
                        crate::config::StaticModelConfig {
                            id: "tts-1".into(),
                            upstream_id: "tts-1".into(),
                            title: "Agamotto TTS".into(),
                            services: vec![AudioService::Speech],
                        },
                    ],
                    include_models: vec![],
                    exclude_models: vec![],
                    priority: 10,
                    auto_load: false,
                    jit_load: false,
                    load_context_length: None,
                    disabled: false,
                },
                None,
                "private/transcription",
                None,
                "private/speech",
                None,
            )
            .unwrap();

        let rendered = editor.rendered();
        assert!(rendered.contains("kind = \"openai_audio\""));
        assert!(rendered.contains("services = [\"transcription\"]"));
        assert!(rendered.contains("services = [\"speech\"]"));
        assert!(rendered.contains("roles = [\"transcription\"]"));
        assert!(rendered.contains("roles = [\"speech\"]"));
        assert!(rendered.contains("id = \"private/transcription\""));
        assert!(rendered.contains("id = \"private/speech\""));
        let validated = editor.validate_rendered().unwrap();
        assert_eq!(validated.provider("speech").unwrap().models.len(), 2);
        assert_eq!(validated.routes.len(), 2);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn claude_recipe_builder_writes_nested_slots_and_client_binding() {
        let directory = std::env::temp_dir().join(format!(
            "infernaut-recipe-editor-test-{}-{}",
            std::process::id(),
            unix_time()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("infernaut.toml");
        fs::write(
            &path,
            r#"[[providers]]
id = "lab"
base_url = "http://127.0.0.1:8000/v1"
"#,
        )
        .unwrap();
        let mut editor = ConfigEditor::load(&path).unwrap();
        editor
            .add_claude_recipe("claude-lab".into(), "lab/main".into())
            .unwrap();
        editor
            .upsert_client(
                ClientConfig {
                    id: "claude-code".into(),
                    title: "Claude Code".into(),
                    api_key_env: "INFERNAUT_CLAUDE_CODE_KEY".into(),
                    recipe: Some("claude-lab".into()),
                    recipes: vec![],
                    disabled: false,
                },
                None,
            )
            .unwrap();

        let rendered = editor.rendered();
        assert!(rendered.contains("[[recipes]]"));
        assert!(rendered.contains("[[recipes.slots]]"));
        assert!(rendered.contains("trigger = \"compression\""));
        assert!(rendered.contains("recipe = \"claude-lab\""));
        let validated = editor.validate_rendered().unwrap();
        assert_eq!(validated.recipes[0].slots.len(), 5);
        assert!(rendered.contains("id = \"background\""));
        assert!(rendered.contains("trigger = \"classifier\""));
        assert_eq!(validated.clients[0].recipe.as_deref(), Some("claude-lab"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn hermes_recipe_builder_persists_multiple_selectable_recipes() {
        let directory = std::env::temp_dir().join(format!(
            "infernaut-hermes-recipe-editor-test-{}-{}",
            std::process::id(),
            unix_time()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("infernaut.toml");
        fs::write(
            &path,
            r#"[[providers]]
id = "lab"
base_url = "http://127.0.0.1:8000/v1"
"#,
        )
        .unwrap();
        let mut editor = ConfigEditor::load(&path).unwrap();
        editor
            .add_hermes_recipe("hermes-private".into(), "lab/main".into())
            .unwrap();
        editor
            .add_hermes_recipe("hermes-fast".into(), "lab/main".into())
            .unwrap();
        editor
            .upsert_client(
                ClientConfig {
                    id: "hermes".into(),
                    title: "Hermes".into(),
                    api_key_env: "INFERNAUT_HERMES_KEY".into(),
                    recipe: None,
                    recipes: vec!["hermes-private".into(), "hermes-fast".into()],
                    disabled: false,
                },
                None,
            )
            .unwrap();

        let rendered = editor.rendered();
        assert!(rendered.contains("kind = \"hermes\""));
        assert!(rendered.contains("protocol = \"openai\""));
        assert!(rendered.contains("recipes = [\"hermes-private\", \"hermes-fast\"]"));
        let validated = editor.validate_rendered().unwrap();
        assert_eq!(validated.recipes.len(), 2);
        assert_eq!(validated.recipes[0].slots.len(), 4);
        assert!(rendered.contains("trigger = \"delegation\""));
        assert!(rendered.contains("trigger = \"vision\""));
        assert_eq!(
            validated.recipes[0].slots[0].reasoning_effort,
            RecipeReasoningEffort::Inherit
        );
        assert_eq!(
            validated.recipes[0].slots[1].reasoning_effort,
            RecipeReasoningEffort::None
        );
        assert_eq!(
            validated.recipes[0].slots[2].reasoning_effort,
            RecipeReasoningEffort::None
        );
        assert_eq!(
            validated.recipes[0].slots[3].reasoning_effort,
            RecipeReasoningEffort::None
        );
        assert!(rendered.contains("reasoning_effort = \"none\""));
        assert_eq!(
            validated.clients[0].recipes,
            vec!["hermes-private", "hermes-fast"]
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn renaming_recipe_updates_exclusive_and_selectable_client_bindings() {
        let directory = std::env::temp_dir().join(format!(
            "infernaut-recipe-rename-test-{}-{}",
            std::process::id(),
            unix_time()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("infernaut.toml");
        fs::write(
            &path,
            r#"[[providers]]
id = "lab"
base_url = "http://127.0.0.1:8000/v1"
"#,
        )
        .unwrap();
        let mut editor = ConfigEditor::load(&path).unwrap();
        editor
            .add_hermes_recipe("hermes-private".into(), "lab/main".into())
            .unwrap();
        editor
            .upsert_client(
                ClientConfig {
                    id: "exclusive".into(),
                    title: "Exclusive".into(),
                    api_key_env: "INFERNAUT_EXCLUSIVE_KEY".into(),
                    recipe: Some("hermes-private".into()),
                    recipes: vec![],
                    disabled: false,
                },
                None,
            )
            .unwrap();
        editor
            .upsert_client(
                ClientConfig {
                    id: "selectable".into(),
                    title: "Selectable".into(),
                    api_key_env: "INFERNAUT_SELECTABLE_KEY".into(),
                    recipe: None,
                    recipes: vec!["hermes-private".into()],
                    disabled: false,
                },
                None,
            )
            .unwrap();

        let mut recipe = editor.config.recipes[0].clone();
        recipe.id = "hermes-coding".into();
        recipe.title = "Hermes Coding".into();
        editor
            .upsert_recipe(recipe, Some("hermes-private"))
            .unwrap();

        assert_eq!(editor.config.recipes[0].id, "hermes-coding");
        assert_eq!(editor.config.recipes[0].title, "Hermes Coding");
        assert_eq!(
            editor.config.clients[0].recipe.as_deref(),
            Some("hermes-coding")
        );
        assert_eq!(editor.config.clients[1].recipes, vec!["hermes-coding"]);
        let rendered = editor.rendered();
        assert!(rendered.contains("id = \"hermes-coding\""));
        assert!(rendered.contains("recipe = \"hermes-coding\""));
        assert!(rendered.contains("recipes = [\"hermes-coding\"]"));
        editor.validate_rendered().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn line_diff_reports_only_changed_lines() {
        assert_eq!(
            line_diff("a\nb\nc\n", "a\nB\nc\n"),
            vec!["- b".to_owned(), "+ B".to_owned()]
        );
    }

    #[test]
    fn exact_model_alias_editor_persists_target_and_contract() {
        let directory = std::env::temp_dir().join(format!(
            "infernaut-alias-editor-test-{}-{}",
            std::process::id(),
            unix_time()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("infernaut.toml");
        fs::write(
            &path,
            r#"[[providers]]
id = "lab"
base_url = "http://127.0.0.1:8000/v1"
"#,
        )
        .unwrap();
        let mut editor = ConfigEditor::load(&path).unwrap();
        editor
            .upsert_alias(
                ModelAliasConfig {
                    id: "cron".into(),
                    title: "Cron automation".into(),
                    target: "lab/main".into(),
                    context_length: Some(131_072),
                    disabled: false,
                },
                None,
            )
            .unwrap();
        editor.set_alias_target(0, "lab/nightly".into()).unwrap();
        let rendered = editor.rendered();
        assert!(rendered.contains("[[aliases]]"));
        assert!(rendered.contains("id = \"cron\""));
        assert!(rendered.contains("target = \"lab/nightly\""));
        assert!(rendered.contains("context_length = 131072"));
        let validated = editor.validate_rendered().unwrap();
        assert_eq!(validated.aliases[0].target, "lab/nightly");
        fs::remove_dir_all(directory).unwrap();
    }
}
