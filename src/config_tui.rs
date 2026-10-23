use std::{
    io::{self, Stdout},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Tabs, Wrap},
};
use serde_json::json;

use crate::{
    catalog::{Catalog, ResolvedModel},
    config::{
        AudioService, ClientConfig, Config, ModelAliasConfig, PrivacyMode, ProviderConfig,
        ProviderKind, RecipeConfig, RecipeKind, StaticModelConfig,
    },
    config_editor::{
        Archetype, ConfigEditor, DoctorReport, ModelProfileState, applied_roles_name, doctor,
        doctor_config, loaded_model_preference_name, privacy_mode_name, provider_kind_name,
        reconcile_models, routing_mode_name, suggest_archetype, suggested_roles_name,
    },
    routing::{RoutingDecision, RoutingEngine},
};

const STRENGTH_KEYS: [&str; 8] = [
    "code.inspect",
    "code.review",
    "code.debug",
    "code.implement",
    "code.architect",
    "data.analyze",
    "chat",
    "default",
];

const PROVIDER_KINDS: [ProviderKind; 11] = [
    ProviderKind::OpenaiCompatible,
    ProviderKind::LmStudio,
    ProviderKind::LlamaCpp,
    ProviderKind::Vllm,
    ProviderKind::Openai,
    ProviderKind::Xai,
    ProviderKind::OllamaCloud,
    ProviderKind::OpenaiCodex,
    ProviderKind::XaiOauth,
    ProviderKind::OpenaiTranscription,
    ProviderKind::OpenaiAudio,
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Providers,
    Clients,
    Recipes,
    Aliases,
    Models,
    Ranking,
    Routes,
    Privacy,
    Simulator,
    Review,
}

impl Tab {
    const ALL: [Self; 10] = [
        Self::Providers,
        Self::Clients,
        Self::Recipes,
        Self::Aliases,
        Self::Models,
        Self::Ranking,
        Self::Routes,
        Self::Privacy,
        Self::Simulator,
        Self::Review,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Providers => "Providers",
            Self::Clients => "Clients",
            Self::Recipes => "Recipes",
            Self::Aliases => "Aliases",
            Self::Models => "Models",
            Self::Ranking => "Ranking",
            Self::Routes => "Routes",
            Self::Privacy => "Privacy",
            Self::Simulator => "Simulator",
            Self::Review => "Review & Save",
        }
    }
}

struct Scenario {
    title: &'static str,
    prompt: &'static str,
    tools: ScenarioTools,
}

#[derive(Clone, Copy)]
enum ScenarioTools {
    None,
    Optional,
    Required,
}

impl ScenarioTools {
    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Optional => "optional (object-shaped auto)",
            Self::Required => "required (Anthropic any)",
        }
    }
}

const SCENARIOS: [Scenario; 10] = [
    Scenario {
        title: "Casual conversation",
        prompt: "Explain why leaves change color in two short paragraphs.",
        tools: ScenarioTools::None,
    },
    Scenario {
        title: "Simple chat with optional tools",
        prompt: "Hi!",
        tools: ScenarioTools::Optional,
    },
    Scenario {
        title: "Explicitly required tool",
        prompt: "Use the configured lookup tool.",
        tools: ScenarioTools::Required,
    },
    Scenario {
        title: "Light code inspection",
        prompt: "Read src/config.rs and explain what the validate function does.",
        tools: ScenarioTools::None,
    },
    Scenario {
        title: "Debug a failure",
        prompt: "Diagnose this Rust stack trace and identify the most likely bug without rewriting the system.",
        tools: ScenarioTools::None,
    },
    Scenario {
        title: "Small implementation",
        prompt: "Implement a small parser function with unit tests in one file.",
        tools: ScenarioTools::None,
    },
    Scenario {
        title: "Heavy architecture",
        prompt: "Architect and implement a new authentication component across the codebase with integration tests and concurrency controls.",
        tools: ScenarioTools::None,
    },
    Scenario {
        title: "Data analysis",
        prompt: "Analyze this dataset, identify the main drivers, and produce a concise statistical summary.",
        tools: ScenarioTools::None,
    },
    Scenario {
        title: "Migration",
        prompt: "Migrate the persistence layer across the entire codebase and include a safe database migration and rollback plan.",
        tools: ScenarioTools::None,
    },
    Scenario {
        title: "Claude Code compaction",
        prompt: "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n- Your entire response must be plain text: an <analysis> block followed by a <summary> block.\nYour task is to create a detailed summary of the conversation so far, preserving technical decisions.",
        tools: ScenarioTools::None,
    },
];

struct App {
    editor: ConfigEditor,
    catalog: Catalog,
    report: DoctorReport,
    tab: usize,
    provider_index: usize,
    client_index: usize,
    recipe_index: usize,
    recipe_slot_index: usize,
    alias_index: usize,
    model_index: usize,
    ranking_model_index: usize,
    strength_index: usize,
    route_index: usize,
    scenario_index: usize,
    review_scroll: u16,
    simulation: Option<RoutingDecision>,
    simulation_service: Option<ResolvedModel>,
    simulation_error: Option<String>,
    status: String,
    provider_form: Option<ProviderForm>,
    client_form: Option<ClientForm>,
    recipe_form: Option<RecipeForm>,
    recipe_target_picker: Option<RecipeTargetPicker>,
    alias_form: Option<AliasForm>,
    alias_target_picker: Option<AliasTargetPicker>,
    quit_armed: bool,
}

impl App {
    fn tab(&self) -> Tab {
        Tab::ALL[self.tab]
    }

    fn discovered_model_ids(&self) -> Vec<String> {
        self.catalog
            .models
            .iter()
            .map(|model| model.id.clone())
            .collect()
    }

    fn recipe_targets(&self) -> Vec<String> {
        let mut targets = self.discovered_model_ids();
        targets.extend(
            self.editor
                .config
                .routing
                .models
                .iter()
                .filter(|profile| !profile.model.contains('*'))
                .map(|profile| profile.model.clone()),
        );
        targets.extend(
            self.editor
                .config
                .recipes
                .iter()
                .flat_map(|recipe| recipe.slots.iter().map(|slot| slot.target.clone())),
        );
        targets.extend(
            self.editor
                .config
                .routes
                .iter()
                .map(|route| route.qualified_id()),
        );
        targets.sort();
        targets.dedup();
        targets
    }

    fn alias_targets(&self) -> Vec<String> {
        let mut targets = self
            .catalog
            .models
            .iter()
            .filter(|model| !model.is_audio_service())
            .map(|model| model.id.clone())
            .collect::<Vec<_>>();
        targets.extend(
            self.editor
                .config
                .routing
                .models
                .iter()
                .filter(|profile| {
                    !profile.model.contains('*')
                        && self
                            .editor
                            .config
                            .static_audio_service(&profile.model)
                            .is_none()
                })
                .map(|profile| profile.model.clone()),
        );
        targets.extend(
            self.editor
                .config
                .aliases
                .iter()
                .map(|alias| alias.target.clone()),
        );
        targets.extend(
            self.editor
                .config
                .routes
                .iter()
                .filter(|route| {
                    ![AudioService::Transcription, AudioService::Speech]
                        .into_iter()
                        .any(|service| {
                            self.catalog
                                .route_is_audio(&self.editor.config, route, service)
                        })
                })
                .map(|route| route.qualified_id()),
        );
        targets.extend(
            self.editor
                .config
                .recipes
                .iter()
                .filter(|recipe| !recipe.disabled)
                .flat_map(|recipe| {
                    recipe
                        .selectable_slots()
                        .map(|slot| recipe.qualified_slot_id(slot))
                }),
        );
        targets.sort();
        targets.dedup();
        targets
    }

    fn selected_model(&self) -> Option<String> {
        reconcile_models(&self.editor.config, &self.catalog)
            .get(self.model_index)
            .map(|model| model.model.clone())
    }

    fn selected_ranking_model(&self) -> Option<String> {
        self.discovered_model_ids()
            .get(self.ranking_model_index)
            .cloned()
    }

    async fn refresh(&mut self) {
        self.status = "Refreshing provider discovery…".to_owned();
        match doctor_config(
            self.editor.config.clone(),
            format!("{} (staged)", self.editor.path().display()),
        )
        .await
        {
            Ok((_config, catalog, report)) => {
                self.catalog = catalog;
                self.report = report;
                self.clamp_selections();
                self.status = "Provider and model discovery refreshed.".to_owned();
                self.refresh_simulation().await;
            }
            Err(error) => self.status = format!("Refresh failed: {error:#}"),
        }
    }

    fn clamp_selections(&mut self) {
        self.provider_index = clamp_index(self.provider_index, self.editor.config.providers.len());
        self.client_index = clamp_index(self.client_index, self.editor.config.clients.len());
        self.recipe_index = clamp_index(self.recipe_index, self.editor.config.recipes.len());
        self.recipe_slot_index = clamp_index(
            self.recipe_slot_index,
            self.editor
                .config
                .recipes
                .get(self.recipe_index)
                .map_or(0, |recipe| recipe.slots.len()),
        );
        self.alias_index = clamp_index(self.alias_index, self.editor.config.aliases.len());
        self.model_index = clamp_index(
            self.model_index,
            reconcile_models(&self.editor.config, &self.catalog).len(),
        );
        self.ranking_model_index = clamp_index(self.ranking_model_index, self.catalog.models.len());
        self.route_index = clamp_index(self.route_index, self.editor.config.routes.len());
    }

    async fn refresh_simulation(&mut self) {
        let Some(route) = self.editor.config.routes.get(self.route_index) else {
            self.simulation = None;
            self.simulation_service = None;
            self.simulation_error = Some("No route selected.".to_owned());
            return;
        };
        let audio_service = [AudioService::Transcription, AudioService::Speech]
            .into_iter()
            .find(|service| {
                self.catalog
                    .route_is_audio(&self.editor.config, route, *service)
            });
        if let Some(service) = audio_service {
            match self
                .catalog
                .resolve_audio(&self.editor.config, &route.qualified_id(), service)
            {
                Ok(resolved) => {
                    self.simulation = None;
                    self.simulation_service = Some(resolved);
                    self.simulation_error = None;
                }
                Err(error) => {
                    self.simulation = None;
                    self.simulation_service = None;
                    self.simulation_error = Some(error.to_string());
                }
            }
            return;
        }
        let scenario = &SCENARIOS[self.scenario_index];
        let mut payload = json!({
            "model": route.qualified_id(),
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": scenario.prompt}]
        });
        if !matches!(scenario.tools, ScenarioTools::None) {
            payload["tools"] = json!([{
                "type": "function",
                "function": {
                    "name": "lookup",
                    "description": "Look up a value",
                    "parameters": {"type": "object", "properties": {}}
                }
            }]);
            payload["tool_choice"] = match scenario.tools {
                ScenarioTools::Optional => json!({"type": "auto"}),
                ScenarioTools::Required => json!({"type": "any"}),
                ScenarioTools::None => unreachable!(),
            };
        }
        let mut catalog = self.catalog.clone();
        catalog.apply_context_overrides(&self.editor.config);
        match RoutingEngine::default()
            .evaluate(&self.editor.config, &catalog, route, &payload, false)
            .await
        {
            Ok(outcome) => {
                self.simulation = Some(outcome.decision);
                self.simulation_service = None;
                self.simulation_error = None;
            }
            Err(error) => {
                self.simulation = None;
                self.simulation_service = None;
                self.simulation_error = Some(error.to_string());
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        match self.tab() {
            Tab::Providers => {
                self.provider_index = move_index(
                    self.provider_index,
                    self.editor.config.providers.len(),
                    delta,
                )
            }
            Tab::Clients => {
                self.client_index =
                    move_index(self.client_index, self.editor.config.clients.len(), delta)
            }
            Tab::Recipes => {
                self.recipe_index =
                    move_index(self.recipe_index, self.editor.config.recipes.len(), delta);
                self.recipe_slot_index = 0;
            }
            Tab::Aliases => {
                self.alias_index =
                    move_index(self.alias_index, self.editor.config.aliases.len(), delta)
            }
            Tab::Models => {
                self.model_index = move_index(
                    self.model_index,
                    reconcile_models(&self.editor.config, &self.catalog).len(),
                    delta,
                )
            }
            Tab::Ranking => {
                self.ranking_model_index =
                    move_index(self.ranking_model_index, self.catalog.models.len(), delta)
            }
            Tab::Routes => {
                self.route_index =
                    move_index(self.route_index, self.editor.config.routes.len(), delta)
            }
            Tab::Privacy => {}
            Tab::Simulator => {
                self.scenario_index = move_index(self.scenario_index, SCENARIOS.len(), delta)
            }
            Tab::Review => {
                self.review_scroll = self.review_scroll.saturating_add_signed(delta as i16)
            }
        }
    }

    fn start_alias_form(&mut self, existing: bool) {
        let (alias, original_id) = if existing {
            let Some(alias) = self.editor.config.aliases.get(self.alias_index).cloned() else {
                self.status = "Select an alias first.".to_owned();
                return;
            };
            let id = alias.id.clone();
            (alias, Some(id))
        } else {
            let Some(target) = self.alias_targets().into_iter().next() else {
                self.status =
                    "Discover or retain at least one model before adding an alias.".to_owned();
                return;
            };
            let id = unique_alias_id(&self.editor.config.aliases);
            (
                ModelAliasConfig {
                    title: title_from_id(&id),
                    id,
                    target,
                    context_length: None,
                    disabled: false,
                },
                None,
            )
        };
        self.alias_form = Some(AliasForm {
            alias,
            original_id,
            field: 0,
            error: None,
        });
    }

    fn start_alias_target_picker(&mut self) {
        let targets = self.alias_targets();
        let Some(current) = self
            .editor
            .config
            .aliases
            .get(self.alias_index)
            .map(|alias| alias.target.clone())
        else {
            self.status = "Select an alias first.".to_owned();
            return;
        };
        if targets.is_empty() {
            self.status = "No physical model or route targets are available.".to_owned();
            return;
        }
        let selected = targets
            .iter()
            .position(|target| target == &current)
            .unwrap_or(0);
        self.alias_target_picker = Some(AliasTargetPicker {
            alias_index: self.alias_index,
            targets,
            query: String::new(),
            selected,
        });
        self.status = "Choose the alias's one exact target; type to filter.".to_owned();
    }

    fn start_client_form(&mut self, existing: bool) {
        let (client, original_id) = if existing {
            let Some(client) = self.editor.config.clients.get(self.client_index).cloned() else {
                return;
            };
            let id = client.id.clone();
            (client, Some(id))
        } else {
            let id = unique_client_id(&self.editor.config.clients);
            (
                ClientConfig {
                    title: title_from_id(&id),
                    api_key_env: client_environment_name(&id),
                    id,
                    recipe: None,
                    recipes: Vec::new(),
                    disabled: false,
                },
                None,
            )
        };
        self.client_form = Some(ClientForm {
            client,
            original_id,
            recipe_ids: self
                .editor
                .config
                .recipes
                .iter()
                .filter(|recipe| !recipe.disabled)
                .map(|recipe| recipe.id.clone())
                .collect(),
            selectable_recipe_index: 0,
            field: 0,
            error: None,
        });
    }

    fn start_recipe_form(&mut self) {
        let Some(recipe) = self.editor.config.recipes.get(self.recipe_index).cloned() else {
            self.status = "Select a recipe first.".to_owned();
            return;
        };
        self.recipe_form = Some(RecipeForm {
            original_id: recipe.id.clone(),
            recipe,
            field: 0,
            error: None,
        });
    }

    fn start_recipe_target_picker(&mut self) {
        let targets = self.recipe_targets();
        let Some(current) = self
            .editor
            .config
            .recipes
            .get(self.recipe_index)
            .and_then(|recipe| recipe.slots.get(self.recipe_slot_index))
            .map(|slot| slot.target.clone())
        else {
            self.status = "Select a recipe slot first.".to_owned();
            return;
        };
        if targets.is_empty() {
            self.status =
                "No discovered models, durable profiles, or routes are available.".to_owned();
            return;
        }
        let selected = targets
            .iter()
            .position(|target| target == &current)
            .unwrap_or(0);
        self.recipe_target_picker = Some(RecipeTargetPicker {
            recipe_index: self.recipe_index,
            slot_index: self.recipe_slot_index,
            targets,
            query: String::new(),
            selected,
        });
        self.status = "Choose a target model or route; type to filter.".to_owned();
    }

    fn start_provider_form(&mut self, existing: bool) {
        let (provider, original_id) = if existing {
            let Some(provider) = self
                .editor
                .config
                .providers
                .get(self.provider_index)
                .cloned()
            else {
                return;
            };
            let id = provider.id.clone();
            (provider, Some(id))
        } else {
            (
                ProviderConfig {
                    id: unique_provider_id(&self.editor.config.providers),
                    kind: ProviderKind::OpenaiCompatible,
                    title: "New provider".to_owned(),
                    alias: None,
                    base_url: "http://127.0.0.1:8000/v1".to_owned(),
                    realm: "local".to_owned(),
                    api_key_env: None,
                    oauth_client_id: None,
                    anthropic_compat: None,
                    include_models: Vec::new(),
                    exclude_models: Vec::new(),
                    models: Vec::new(),
                    priority: 100,
                    auto_load: false,
                    jit_load: false,
                    load_context_length: None,
                    disabled: false,
                },
                None,
            )
        };
        let transcription_model = provider
            .models
            .iter()
            .find(|model| model.supports_audio_service(provider.kind, AudioService::Transcription));
        let transcription_model_id = transcription_model
            .map(|model| model.id.clone())
            .unwrap_or_else(|| "nemotron-3.5-asr".to_owned());
        let transcription_upstream_id = transcription_model
            .map(|model| model.upstream_id().to_owned())
            .unwrap_or_else(|| "nemotron-3.5-asr".to_owned());
        let qualified_model = format!("{}/{}", provider.id, transcription_model_id);
        let configured_transcription_route = self
            .editor
            .config
            .routes
            .iter()
            .find(|route| route.candidates.contains(&qualified_model))
            .map(|route| route.id.clone());
        let original_transcription_route_id = provider
            .is_audio_provider()
            .then(|| configured_transcription_route.clone())
            .flatten();
        let transcription_route_id =
            configured_transcription_route.unwrap_or_else(|| "private/transcription".to_owned());
        let speech_model = provider
            .models
            .iter()
            .find(|model| model.supports_audio_service(provider.kind, AudioService::Speech));
        let speech_model_id = speech_model
            .map(|model| model.id.clone())
            .unwrap_or_else(|| "tts-1".to_owned());
        let speech_upstream_id = speech_model
            .map(|model| model.upstream_id().to_owned())
            .unwrap_or_else(|| "tts-1".to_owned());
        let qualified_speech_model = format!("{}/{}", provider.id, speech_model_id);
        let configured_speech_route = self
            .editor
            .config
            .routes
            .iter()
            .find(|route| route.candidates.contains(&qualified_speech_model))
            .map(|route| route.id.clone());
        let original_speech_route_id = (provider.kind == ProviderKind::OpenaiAudio)
            .then(|| configured_speech_route.clone())
            .flatten();
        let speech_route_id =
            configured_speech_route.unwrap_or_else(|| "private/speech".to_owned());
        self.provider_form = Some(ProviderForm {
            provider,
            original_id,
            original_transcription_route_id,
            original_speech_route_id,
            transcription_model_id,
            transcription_upstream_id,
            transcription_route_id,
            speech_model_id,
            speech_upstream_id,
            speech_route_id,
            field: 0,
            error: None,
        });
    }

    fn apply_archetype(&mut self, index: usize) {
        let Some(model) = self.selected_model() else {
            return;
        };
        if self
            .catalog
            .models
            .iter()
            .find(|known| known.id == model)
            .is_some_and(|known| known.is_audio_service())
        {
            self.status =
                "Audio services use exclusive roles; their guided provider setup owns these profiles."
                    .to_owned();
            return;
        }
        if !self.catalog.models.iter().any(|known| known.id == model) {
            self.status = "Dormant profiles are retained; select a discovered model to reclassify."
                .to_owned();
            return;
        }
        let archetype = Archetype::ALL[index];
        self.editor.apply_archetype(&model, archetype);
        self.status = if archetype == Archetype::VisionSpecialist {
            format!("Toggled the additive Vision role for {model}.")
        } else {
            format!("Applied '{}' to {model}.", archetype.name())
        };
    }

    fn apply_suggested_archetype(&mut self) {
        let Some(model_id) = self.selected_model() else {
            return;
        };
        let Some(model) = self
            .catalog
            .models
            .iter()
            .find(|model| model.id == model_id)
        else {
            self.status = "Dormant profiles do not receive new suggestions.".to_owned();
            return;
        };
        let suggestion = suggest_archetype(model);
        self.editor.apply_suggestion(&model.id, suggestion);
        self.status = format!(
            "Applied suggested '{}' to {} ({})",
            suggested_roles_name(suggestion),
            model.id,
            suggestion.reason
        );
    }

    fn toggle_selected_model_auto_routing(&mut self) {
        let Some(model) = self.selected_model() else {
            return;
        };
        if self
            .catalog
            .models
            .iter()
            .find(|known| known.id == model)
            .is_some_and(|known| known.is_audio_service())
        {
            self.status =
                "Audio services use explicit endpoint routes and cannot join adaptive LLM routing."
                    .to_owned();
            return;
        }
        let enabled = self.editor.toggle_model_auto_routing(&model);
        self.status = if enabled {
            format!("{model} is eligible for adaptive routing.")
        } else {
            format!("{model} is reserved for direct physical-ID requests.")
        };
    }

    fn cycle_selected_model_context_length(&mut self, forward: bool) {
        let Some(model) = self.selected_model() else {
            return;
        };
        let context_length = self.editor.cycle_model_context_length(&model, forward);
        self.status = match context_length {
            Some(tokens) => format!(
                "Context override for {model} staged at {}.",
                format_context_length(tokens)
            ),
            None => {
                format!("Context override for {model} cleared; provider discovery will be used.")
            }
        };
    }

    fn toggle_selected_model_compression_role(&mut self) {
        let Some(model) = self.selected_model() else {
            return;
        };
        if !self.catalog.models.iter().any(|known| known.id == model) {
            self.status =
                "Dormant profiles are retained; select a discovered model to change roles."
                    .to_owned();
            return;
        }
        self.editor.toggle_compression_role(&model);
        let applied = self
            .editor
            .config
            .routing
            .models
            .iter()
            .find(|profile| profile.model == model)
            .map(applied_roles_name)
            .unwrap_or_else(|| "-".to_owned());
        self.status = format!("Compression role toggled for {model}: {applied}.");
    }

    fn ensure_exact_ranking_profile(&mut self, model: &str) {
        if !self
            .editor
            .config
            .routing
            .models
            .iter()
            .any(|profile| profile.model == model)
        {
            self.editor.apply_archetype(model, Archetype::Balanced);
            self.status = format!("Created a Balanced profile for {model}; adjust as needed.");
        }
    }
}

struct ProviderForm {
    provider: ProviderConfig,
    original_id: Option<String>,
    original_transcription_route_id: Option<String>,
    original_speech_route_id: Option<String>,
    transcription_model_id: String,
    transcription_upstream_id: String,
    transcription_route_id: String,
    speech_model_id: String,
    speech_upstream_id: String,
    speech_route_id: String,
    field: usize,
    error: Option<String>,
}

impl ProviderForm {
    const FIELD_NAMES: [&'static str; 14] = [
        "ID",
        "Kind",
        "Title",
        "Base URL",
        "Realm",
        "Alias",
        "API key env",
        "Priority",
        "STT model ID",
        "STT upstream",
        "STT route ID",
        "TTS model ID",
        "TTS upstream",
        "TTS route ID",
    ];

    fn value(&self, field: usize) -> String {
        match field {
            0 => self.provider.id.clone(),
            1 => provider_kind_name(self.provider.kind).to_owned(),
            2 => self.provider.title.clone(),
            3 => self.provider.base_url.clone(),
            4 => self.provider.realm.clone(),
            5 => self.provider.alias.clone().unwrap_or_default(),
            6 => self.provider.api_key_env.clone().unwrap_or_default(),
            7 => self.provider.priority.to_string(),
            8 if self.provider.is_audio_provider() => self.transcription_model_id.clone(),
            9 if self.provider.is_audio_provider() => self.transcription_upstream_id.clone(),
            10 if self.provider.is_audio_provider() => self.transcription_route_id.clone(),
            11 if self.provider.kind == ProviderKind::OpenaiAudio => self.speech_model_id.clone(),
            12 if self.provider.kind == ProviderKind::OpenaiAudio => {
                self.speech_upstream_id.clone()
            }
            13 if self.provider.kind == ProviderKind::OpenaiAudio => self.speech_route_id.clone(),
            8..=13 => "— OpenAI audio providers only".to_owned(),
            _ => String::new(),
        }
    }

    fn edit_text(&mut self, code: KeyCode) {
        if self.field == 1
            || ((8..=10).contains(&self.field) && !self.provider.is_audio_provider())
            || (self.field >= 11 && self.provider.kind != ProviderKind::OpenaiAudio)
        {
            return;
        }
        let mut text = self.value(self.field);
        match code {
            KeyCode::Char(character) => text.push(character),
            KeyCode::Backspace => {
                text.pop();
            }
            _ => return,
        }
        match self.field {
            0 => self.provider.id = text,
            2 => self.provider.title = text,
            3 => self.provider.base_url = text,
            4 => self.provider.realm = text,
            5 => self.provider.alias = nonempty(text),
            6 => self.provider.api_key_env = nonempty(text),
            7 => {
                if text.is_empty() {
                    self.provider.priority = 0;
                } else if let Ok(priority) = text.parse() {
                    self.provider.priority = priority;
                }
            }
            8 => self.transcription_model_id = text,
            9 => self.transcription_upstream_id = text,
            10 => self.transcription_route_id = text,
            11 => self.speech_model_id = text,
            12 => self.speech_upstream_id = text,
            13 => self.speech_route_id = text,
            _ => {}
        }
    }

    fn cycle_kind(&mut self, delta: isize) {
        let previous = self.provider.kind;
        let current = PROVIDER_KINDS
            .iter()
            .position(|kind| *kind == self.provider.kind)
            .unwrap_or(0);
        self.provider.kind = PROVIDER_KINDS[move_index(current, PROVIDER_KINDS.len(), delta)];
        let known_default = self.provider.base_url.is_empty()
            || matches!(
                self.provider.base_url.as_str(),
                "http://127.0.0.1:1234/v1"
                    | "http://127.0.0.1:8080/v1"
                    | "http://127.0.0.1:8000/v1"
            );
        if known_default {
            self.provider.base_url = match self.provider.kind {
                ProviderKind::LmStudio => "http://127.0.0.1:1234/v1",
                ProviderKind::LlamaCpp => "http://127.0.0.1:8080/v1",
                ProviderKind::Vllm | ProviderKind::OpenaiCompatible => "http://127.0.0.1:8000/v1",
                ProviderKind::OpenaiTranscription | ProviderKind::OpenaiAudio => {
                    "http://127.0.0.1:8080/v1"
                }
                _ => "",
            }
            .to_owned();
        }
        if previous != self.provider.kind
            && matches!(
                self.provider.kind,
                ProviderKind::LmStudio | ProviderKind::LlamaCpp
            )
        {
            self.provider.jit_load = true;
            self.provider.auto_load = false;
        }
        if matches!(
            self.provider.kind,
            ProviderKind::Openai
                | ProviderKind::Xai
                | ProviderKind::OllamaCloud
                | ProviderKind::OpenaiCodex
                | ProviderKind::XaiOauth
        ) {
            self.provider.realm = "cloud".to_owned();
        }
        if self.provider.is_audio_provider() {
            self.provider.auto_load = false;
            self.provider.jit_load = false;
            if self.transcription_model_id.is_empty() {
                self.transcription_model_id = "nemotron-3.5-asr".to_owned();
            }
            if self.transcription_upstream_id.is_empty() {
                self.transcription_upstream_id = "nemotron-3.5-asr".to_owned();
            }
            if self.transcription_route_id.is_empty() {
                self.transcription_route_id = "private/transcription".to_owned();
            }
            if self.speech_model_id.is_empty() {
                self.speech_model_id = "tts-1".to_owned();
            }
            if self.speech_upstream_id.is_empty() {
                self.speech_upstream_id = "tts-1".to_owned();
            }
            if self.speech_route_id.is_empty() {
                self.speech_route_id = "private/speech".to_owned();
            }
        } else {
            self.provider.models.clear();
        }
    }
}

struct ClientForm {
    client: ClientConfig,
    original_id: Option<String>,
    recipe_ids: Vec<String>,
    selectable_recipe_index: usize,
    field: usize,
    error: Option<String>,
}

struct RecipeForm {
    recipe: RecipeConfig,
    original_id: String,
    field: usize,
    error: Option<String>,
}

struct RecipeTargetPicker {
    recipe_index: usize,
    slot_index: usize,
    targets: Vec<String>,
    query: String,
    selected: usize,
}

struct AliasForm {
    alias: ModelAliasConfig,
    original_id: Option<String>,
    field: usize,
    error: Option<String>,
}

struct AliasTargetPicker {
    alias_index: usize,
    targets: Vec<String>,
    query: String,
    selected: usize,
}

impl RecipeTargetPicker {
    fn filtered_targets(&self) -> Vec<&str> {
        let query = self.query.to_ascii_lowercase();
        self.targets
            .iter()
            .filter(|target| query.is_empty() || target.to_ascii_lowercase().contains(&query))
            .map(String::as_str)
            .collect()
    }

    fn clamp(&mut self) {
        self.selected = clamp_index(self.selected, self.filtered_targets().len());
    }
}

impl AliasTargetPicker {
    fn filtered_targets(&self) -> Vec<&str> {
        let query = self.query.to_ascii_lowercase();
        self.targets
            .iter()
            .filter(|target| query.is_empty() || target.to_ascii_lowercase().contains(&query))
            .map(String::as_str)
            .collect()
    }

    fn clamp(&mut self) {
        self.selected = clamp_index(self.selected, self.filtered_targets().len());
    }
}

enum TargetPickerAction {
    Continue,
    Cancel,
    Select(String),
}

impl ClientForm {
    const FIELD_NAMES: [&'static str; 5] = [
        "ID",
        "Display title",
        "API key env",
        "Exclusive recipe",
        "Selectable recipes",
    ];

    fn value(&self, field: usize) -> String {
        match field {
            0 => self.client.id.clone(),
            1 => self.client.title.clone(),
            2 => self.client.api_key_env.clone(),
            3 => self
                .client
                .recipe
                .clone()
                .unwrap_or_else(|| "none".to_owned()),
            4 => self
                .recipe_ids
                .get(self.selectable_recipe_index)
                .map(|recipe| {
                    let selected = self.client.recipes.iter().any(|id| id == recipe);
                    format!(
                        "[{}] {} ({}/{})",
                        if selected { "x" } else { " " },
                        recipe,
                        self.selectable_recipe_index + 1,
                        self.recipe_ids.len()
                    )
                })
                .unwrap_or_else(|| "none available".to_owned()),
            _ => String::new(),
        }
    }

    fn edit_text(&mut self, code: KeyCode) {
        let mut text = self.value(self.field);
        match code {
            KeyCode::Char(character) => text.push(character),
            KeyCode::Backspace => {
                text.pop();
            }
            _ => return,
        }
        match self.field {
            0 => {
                let previous_id = self.client.id.clone();
                let generated_title = title_from_id(&previous_id);
                let generated_environment = client_environment_name(&previous_id);
                self.client.id = text;
                if self.client.title == generated_title {
                    self.client.title = title_from_id(&self.client.id);
                }
                if self.client.api_key_env == generated_environment {
                    self.client.api_key_env = client_environment_name(&self.client.id);
                }
            }
            1 => self.client.title = text,
            2 => self.client.api_key_env = text,
            _ => {}
        }
    }

    fn cycle_recipe(&mut self, delta: isize) {
        let mut choices = vec![None];
        choices.extend(self.recipe_ids.iter().cloned().map(Some));
        let current = choices
            .iter()
            .position(|choice| *choice == self.client.recipe)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(choices.len() as isize) as usize;
        self.client.recipe = choices[next].clone();
        if self.client.recipe.is_some() {
            self.client.recipes.clear();
        }
    }

    fn cycle_selectable_recipe(&mut self, delta: isize) {
        self.selectable_recipe_index =
            move_index(self.selectable_recipe_index, self.recipe_ids.len(), delta);
    }

    fn toggle_selectable_recipe(&mut self) {
        let Some(recipe) = self.recipe_ids.get(self.selectable_recipe_index).cloned() else {
            return;
        };
        self.client.recipe = None;
        if let Some(position) = self.client.recipes.iter().position(|id| id == &recipe) {
            self.client.recipes.remove(position);
        } else {
            self.client.recipes.push(recipe);
            self.client.recipes.sort();
        }
    }
}

impl RecipeForm {
    const FIELD_NAMES: [&'static str; 2] = ["ID", "Display title"];

    fn value(&self, field: usize) -> String {
        match field {
            0 => self.recipe.id.clone(),
            1 => self.recipe.title.clone(),
            _ => String::new(),
        }
    }

    fn edit_text(&mut self, code: KeyCode) {
        let mut text = self.value(self.field);
        match code {
            KeyCode::Char(character) => text.push(character),
            KeyCode::Backspace => {
                text.pop();
            }
            _ => return,
        }
        match self.field {
            0 => {
                let previous_id = self.recipe.id.clone();
                let generated_title = title_from_id(&previous_id);
                self.recipe.id = text;
                if self.recipe.title == generated_title {
                    self.recipe.title = title_from_id(&self.recipe.id);
                }
            }
            1 => self.recipe.title = text,
            _ => {}
        }
    }
}

impl AliasForm {
    const FIELD_NAMES: [&'static str; 2] = ["ID", "Display title"];

    fn value(&self, field: usize) -> String {
        match field {
            0 => self.alias.id.clone(),
            1 => self.alias.title.clone(),
            _ => String::new(),
        }
    }

    fn edit_text(&mut self, code: KeyCode) {
        let mut text = self.value(self.field);
        match code {
            KeyCode::Char(character) => text.push(character),
            KeyCode::Backspace => {
                text.pop();
            }
            _ => return,
        }
        match self.field {
            0 => {
                let previous_id = self.alias.id.clone();
                let generated_title = title_from_id(&previous_id);
                self.alias.id = text;
                if self.alias.title == generated_title {
                    self.alias.title = title_from_id(&self.alias.id);
                }
            }
            1 => self.alias.title = text,
            _ => {}
        }
    }
}

pub async fn run(path: impl AsRef<Path>, offline: bool) -> Result<()> {
    let path = path.as_ref();
    let editor = ConfigEditor::load(path)?;
    let recipe_migration_staged = editor.is_dirty();
    let (catalog, report) = if offline {
        let catalog = Catalog::default();
        let report = DoctorReport {
            config: path.display().to_string(),
            providers: Vec::new(),
            models: reconcile_models(&editor.config, &catalog),
            issues: vec![crate::config_editor::DoctorIssue {
                severity: "info",
                code: "offline_mode",
                message: "provider discovery was skipped; press r when connectivity is available"
                    .to_owned(),
            }],
        };
        (catalog, report)
    } else {
        let (_config, catalog, report) = doctor(path).await?;
        (catalog, report)
    };
    let mut app = App {
        editor,
        catalog,
        report,
        tab: 0,
        provider_index: 0,
        client_index: 0,
        recipe_index: 0,
        recipe_slot_index: 0,
        alias_index: 0,
        model_index: 0,
        ranking_model_index: 0,
        strength_index: 0,
        route_index: 0,
        scenario_index: 0,
        review_scroll: 0,
        simulation: None,
        simulation_service: None,
        simulation_error: None,
        status: if recipe_migration_staged {
            "Claude recipe upgrade staged: classifier preserved; background inherited from subagent. Review and save to persist.".to_owned()
        } else {
            "Ready. No provider inference was performed.".to_owned()
        },
        provider_form: None,
        client_form: None,
        recipe_form: None,
        recipe_target_picker: None,
        alias_form: None,
        alias_target_picker: None,
        quit_armed: false,
    };
    app.refresh_simulation().await;

    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, &mut app).await;
    restore_terminal(&mut terminal)?;
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if let Some(form) = app.provider_form.as_mut() {
            match handle_provider_form(form, key) {
                FormAction::Continue => {}
                FormAction::Cancel => {
                    app.provider_form = None;
                    app.status = "Provider edit cancelled.".to_owned();
                }
                FormAction::Save => {
                    let form = app.provider_form.take().expect("provider form exists");
                    let mut provider = form.provider.clone();
                    let result = if provider.kind == ProviderKind::OpenaiTranscription {
                        provider.models = vec![StaticModelConfig {
                            id: form.transcription_model_id.clone(),
                            upstream_id: form.transcription_upstream_id.clone(),
                            title: title_from_id(&form.transcription_model_id),
                            services: vec![],
                        }];
                        app.editor.upsert_transcription_provider(
                            provider,
                            form.original_id.as_deref(),
                            &form.transcription_route_id,
                            form.original_transcription_route_id.as_deref(),
                        )
                    } else if provider.kind == ProviderKind::OpenaiAudio {
                        provider.models = vec![
                            StaticModelConfig {
                                id: form.transcription_model_id.clone(),
                                upstream_id: form.transcription_upstream_id.clone(),
                                title: title_from_id(&form.transcription_model_id),
                                services: vec![AudioService::Transcription],
                            },
                            StaticModelConfig {
                                id: form.speech_model_id.clone(),
                                upstream_id: form.speech_upstream_id.clone(),
                                title: title_from_id(&form.speech_model_id),
                                services: vec![AudioService::Speech],
                            },
                        ];
                        app.editor.upsert_audio_provider(
                            provider,
                            form.original_id.as_deref(),
                            &form.transcription_route_id,
                            form.original_transcription_route_id.as_deref(),
                            &form.speech_route_id,
                            form.original_speech_route_id.as_deref(),
                        )
                    } else {
                        provider.models.clear();
                        app.editor
                            .upsert_provider(provider, form.original_id.as_deref())
                    };
                    match result {
                        Ok(()) => {
                            app.status = if form.provider.kind == ProviderKind::OpenaiTranscription
                            {
                                format!(
                                    "Transcription provider, static model, and route '{}' staged. Review and save.",
                                    form.transcription_route_id
                                )
                            } else if form.provider.kind == ProviderKind::OpenaiAudio {
                                format!(
                                    "OpenAI Audio provider staged with STT route '{}' and TTS route '{}'. Review and save.",
                                    form.transcription_route_id, form.speech_route_id
                                )
                            } else {
                                "Provider staged. Press r to test, then review and save.".to_owned()
                            };
                            app.clamp_selections();
                        }
                        Err(error) => {
                            let mut form = form;
                            form.error = Some(error.to_string());
                            app.provider_form = Some(form);
                        }
                    }
                }
            }
            continue;
        }
        if let Some(form) = app.client_form.as_mut() {
            match handle_client_form(form, key) {
                FormAction::Continue => {}
                FormAction::Cancel => {
                    app.client_form = None;
                    app.status = "Client edit cancelled.".to_owned();
                }
                FormAction::Save => {
                    let form = app.client_form.take().expect("client form exists");
                    let client = form.client.clone();
                    match app
                        .editor
                        .upsert_client(client, form.original_id.as_deref())
                    {
                        Ok(()) => {
                            app.status =
                                "Client staged. Set its environment variable before restarting Infernaut."
                                    .to_owned();
                            app.clamp_selections();
                        }
                        Err(error) => {
                            let mut form = form;
                            form.error = Some(error.to_string());
                            app.client_form = Some(form);
                        }
                    }
                }
            }
            continue;
        }
        if let Some(form) = app.recipe_form.as_mut() {
            match handle_recipe_form(form, key) {
                FormAction::Continue => {}
                FormAction::Cancel => {
                    app.recipe_form = None;
                    app.status = "Recipe edit cancelled.".to_owned();
                }
                FormAction::Save => {
                    let form = app.recipe_form.take().expect("recipe form exists");
                    let recipe = form.recipe.clone();
                    match app.editor.upsert_recipe(recipe, Some(&form.original_id)) {
                        Ok(()) => {
                            app.status = format!(
                                "Recipe '{}' staged; bound client references were kept in sync.",
                                form.recipe.id
                            );
                            app.clamp_selections();
                        }
                        Err(error) => {
                            let mut form = form;
                            form.error = Some(error.to_string());
                            app.recipe_form = Some(form);
                        }
                    }
                }
            }
            continue;
        }
        if let Some(picker) = app.recipe_target_picker.as_mut() {
            match handle_recipe_target_picker(picker, key) {
                TargetPickerAction::Continue => {}
                TargetPickerAction::Cancel => {
                    app.recipe_target_picker = None;
                    app.status = "Recipe target selection cancelled.".to_owned();
                }
                TargetPickerAction::Select(target) => {
                    let picker = app
                        .recipe_target_picker
                        .take()
                        .expect("recipe target picker exists");
                    match app.editor.set_recipe_slot_target(
                        picker.recipe_index,
                        picker.slot_index,
                        target.clone(),
                    ) {
                        Ok(()) => app.status = format!("Recipe slot target staged as '{target}'."),
                        Err(error) => {
                            app.status = format!("Could not update recipe target: {error:#}")
                        }
                    }
                }
            }
            continue;
        }
        if let Some(form) = app.alias_form.as_mut() {
            match handle_alias_form(form, key) {
                FormAction::Continue => {}
                FormAction::Cancel => {
                    app.alias_form = None;
                    app.status = "Alias edit cancelled.".to_owned();
                }
                FormAction::Save => {
                    let form = app.alias_form.take().expect("alias form exists");
                    let alias = form.alias.clone();
                    match app.editor.upsert_alias(alias, form.original_id.as_deref()) {
                        Ok(()) => {
                            if form.original_id.is_none() {
                                app.alias_index = app.editor.config.aliases.len().saturating_sub(1);
                            }
                            app.status = format!(
                                "Alias '{}' staged as a deterministic pointer to '{}'.",
                                form.alias.id, form.alias.target
                            );
                            app.clamp_selections();
                        }
                        Err(error) => {
                            let mut form = form;
                            form.error = Some(error.to_string());
                            app.alias_form = Some(form);
                        }
                    }
                }
            }
            continue;
        }
        if let Some(picker) = app.alias_target_picker.as_mut() {
            match handle_alias_target_picker(picker, key) {
                TargetPickerAction::Continue => {}
                TargetPickerAction::Cancel => {
                    app.alias_target_picker = None;
                    app.status = "Alias target selection cancelled.".to_owned();
                }
                TargetPickerAction::Select(target) => {
                    let picker = app
                        .alias_target_picker
                        .take()
                        .expect("alias target picker exists");
                    match app
                        .editor
                        .set_alias_target(picker.alias_index, target.clone())
                    {
                        Ok(()) => {
                            app.status = format!(
                                "Alias target staged as '{target}'. No fallback will be attempted."
                            )
                        }
                        Err(error) => {
                            app.status = format!("Could not update alias target: {error:#}")
                        }
                    }
                }
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') => {
                if app.editor.is_dirty() && !app.quit_armed {
                    app.quit_armed = true;
                    app.status =
                        "Unsaved changes. Press uppercase Q to discard and quit.".to_owned();
                } else if !app.editor.is_dirty() {
                    break;
                }
            }
            KeyCode::Char('Q') if app.quit_armed => break,
            KeyCode::Tab | KeyCode::Right if key.modifiers.is_empty() => {
                app.tab = (app.tab + 1) % Tab::ALL.len();
                app.quit_armed = false;
            }
            KeyCode::BackTab | KeyCode::Left if key.modifiers.is_empty() => {
                app.tab = app.tab.checked_sub(1).unwrap_or(Tab::ALL.len() - 1);
                app.quit_armed = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.move_selection(1);
                if app.tab() == Tab::Simulator {
                    app.refresh_simulation().await;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.move_selection(-1);
                if app.tab() == Tab::Simulator {
                    app.refresh_simulation().await;
                }
            }
            KeyCode::Char('r') => app.refresh().await,
            KeyCode::Char('m') if matches!(app.tab(), Tab::Routes | Tab::Simulator) => {
                app.editor.cycle_routing_mode();
                app.status = format!(
                    "Routing mode staged as {}.",
                    routing_mode_name(app.editor.config.routing.mode)
                );
                app.refresh_simulation().await;
            }
            KeyCode::Char('w') if matches!(app.tab(), Tab::Routes | Tab::Simulator) => {
                app.editor.cycle_loaded_model_preference();
                app.status = format!(
                    "Loaded-model preference staged as {}.",
                    loaded_model_preference_name(app.editor.config.routing.loaded_model_preference)
                );
                app.refresh_simulation().await;
            }
            KeyCode::Char('p') if app.tab() == Tab::Privacy => {
                app.editor.cycle_privacy_mode();
                app.status = format!(
                    "Privacy lock staged as {}.",
                    privacy_mode_name(app.editor.config.privacy_lock.mode)
                );
                app.refresh_simulation().await;
            }
            KeyCode::Char('l') if app.tab() == Tab::Routes => {
                app.editor
                    .set_route_realms(app.route_index, vec!["local".to_owned()]);
                app.status = "Selected route constrained to local providers.".to_owned();
                app.refresh_simulation().await;
            }
            KeyCode::Char('c') if app.tab() == Tab::Routes => {
                app.editor
                    .set_route_realms(app.route_index, vec!["cloud".to_owned()]);
                app.status = "Selected route constrained to cloud providers.".to_owned();
                app.refresh_simulation().await;
            }
            KeyCode::Char('b') if app.tab() == Tab::Routes => {
                app.editor.set_route_realms(
                    app.route_index,
                    vec!["local".to_owned(), "cloud".to_owned()],
                );
                app.status = "Selected route allows local and cloud providers.".to_owned();
                app.refresh_simulation().await;
            }
            KeyCode::Char('t') if app.tab() == Tab::Routes => {
                app.editor.cycle_route_task(app.route_index);
                app.refresh_simulation().await;
            }
            KeyCode::Char('e') if app.tab() == Tab::Routes => {
                app.editor.cycle_route_effort(app.route_index);
                app.refresh_simulation().await;
            }
            KeyCode::Char('a') if app.tab() == Tab::Routes => {
                app.editor.toggle_route_strategy(app.route_index);
                app.refresh_simulation().await;
            }
            KeyCode::Char('[') if app.tab() == Tab::Routes => {
                app.editor
                    .cycle_route_context_length(app.route_index, false);
                app.status = staged_route_context_status(app);
                app.refresh_simulation().await;
            }
            KeyCode::Char(']') if app.tab() == Tab::Routes => {
                app.editor.cycle_route_context_length(app.route_index, true);
                app.status = staged_route_context_status(app);
                app.refresh_simulation().await;
            }
            KeyCode::Char('i') if app.tab() == Tab::Routes => {
                app.editor.cycle_route_token_estimate(app.route_index);
                app.status = app
                    .editor
                    .config
                    .routes
                    .get(app.route_index)
                    .map(|route| match route.token_estimate_bytes_per_token {
                        Some(ratio) => {
                            format!("Route input estimate staged as {ratio} bytes/token.")
                        }
                        None => "Route input estimate restored to conservative default (3.0 bytes/token).".to_owned(),
                    })
                    .unwrap_or_else(|| "No route selected.".to_owned());
                app.refresh_simulation().await;
            }
            KeyCode::Char('o') if app.tab() == Tab::Routes => {
                app.editor
                    .cycle_route_compression_output_limit(app.route_index);
                app.status = app
                    .editor
                    .config
                    .routes
                    .get(app.route_index)
                    .map(|route| match route.compression_max_output_tokens {
                        Some(tokens) => format!(
                            "Detected compression output staged at {} maximum.",
                            format_context_length(tokens)
                        ),
                        None => "Compression output restored to the harness-requested maximum."
                            .to_owned(),
                    })
                    .unwrap_or_else(|| "No route selected.".to_owned());
                app.refresh_simulation().await;
            }
            KeyCode::Char('a') if app.tab() == Tab::Providers => app.start_provider_form(false),
            KeyCode::Char('e') if app.tab() == Tab::Providers => app.start_provider_form(true),
            KeyCode::Char('i') if app.tab() == Tab::Providers => {
                match app.editor.toggle_provider_jit(app.provider_index) {
                    Ok(()) => app.status = "Provider JIT loading toggled.".to_owned(),
                    Err(error) => app.status = format!("Could not update provider: {error:#}"),
                }
            }
            KeyCode::Char('o') if app.tab() == Tab::Providers => {
                match app.editor.toggle_provider_auto_load(app.provider_index) {
                    Ok(()) => app.status = "Provider explicit auto-load toggled.".to_owned(),
                    Err(error) => app.status = format!("Could not update provider: {error:#}"),
                }
            }
            KeyCode::Char('d') if app.tab() == Tab::Providers => {
                match app.editor.toggle_provider_disabled(app.provider_index) {
                    Ok(()) => app.status = "Provider enabled state toggled.".to_owned(),
                    Err(error) => app.status = format!("Could not update provider: {error:#}"),
                }
            }
            KeyCode::Char('a') if app.tab() == Tab::Clients => app.start_client_form(false),
            KeyCode::Char('e') if app.tab() == Tab::Clients => app.start_client_form(true),
            KeyCode::Char('d') if app.tab() == Tab::Clients => {
                match app.editor.toggle_client_disabled(app.client_index) {
                    Ok(()) => app.status = "Client enabled state toggled.".to_owned(),
                    Err(error) => app.status = format!("Could not update client: {error:#}"),
                }
            }
            KeyCode::Char('x') if app.tab() == Tab::Clients => {
                match app.editor.remove_client(app.client_index) {
                    Ok(client) => {
                        app.status = format!(
                            "Removed client '{}'; review and save to persist.",
                            client.id
                        );
                        app.clamp_selections();
                    }
                    Err(error) => app.status = format!("Could not remove client: {error:#}"),
                }
            }
            KeyCode::Char('a') if app.tab() == Tab::Recipes => {
                let target = app.discovered_model_ids().into_iter().next().or_else(|| {
                    app.editor
                        .config
                        .routes
                        .first()
                        .map(|route| route.qualified_id())
                });
                if let Some(target) = target {
                    let id = unique_recipe_id(&app.editor.config.recipes, "claude-code");
                    match app.editor.add_claude_recipe(id.clone(), target) {
                        Ok(()) => {
                            app.recipe_index = app.editor.config.recipes.len().saturating_sub(1);
                            app.recipe_slot_index = 0;
                            app.status = format!(
                                "Claude Code recipe '{id}' staged; assign each slot, then bind it to a client key."
                            );
                        }
                        Err(error) => app.status = format!("Could not add recipe: {error:#}"),
                    }
                } else {
                    app.status = "Discover at least one model before adding a recipe.".to_owned();
                }
            }
            KeyCode::Char('h') if app.tab() == Tab::Recipes => {
                let target = app.discovered_model_ids().into_iter().next().or_else(|| {
                    app.editor
                        .config
                        .routes
                        .first()
                        .map(|route| route.qualified_id())
                });
                if let Some(target) = target {
                    let id = unique_recipe_id(&app.editor.config.recipes, "hermes");
                    match app.editor.add_hermes_recipe(id.clone(), target) {
                        Ok(()) => {
                            app.recipe_index = app.editor.config.recipes.len().saturating_sub(1);
                            app.recipe_slot_index = 0;
                            app.status = format!(
                                "Hermes recipe '{id}' staged; assign primary, delegation, and compression, then authorize it as a selectable client recipe."
                            );
                        }
                        Err(error) => app.status = format!("Could not add recipe: {error:#}"),
                    }
                } else {
                    app.status = "Discover at least one model before adding a recipe.".to_owned();
                }
            }
            KeyCode::Char('e') if app.tab() == Tab::Recipes => app.start_recipe_form(),
            KeyCode::Enter | KeyCode::Char(' ') if app.tab() == Tab::Recipes => {
                app.start_recipe_target_picker();
            }
            KeyCode::Char('n') if app.tab() == Tab::Recipes => {
                let slots = app
                    .editor
                    .config
                    .recipes
                    .get(app.recipe_index)
                    .map_or(0, |recipe| recipe.slots.len());
                app.recipe_slot_index = move_index(app.recipe_slot_index, slots, 1);
            }
            KeyCode::Char('p') if app.tab() == Tab::Recipes => {
                let slots = app
                    .editor
                    .config
                    .recipes
                    .get(app.recipe_index)
                    .map_or(0, |recipe| recipe.slots.len());
                app.recipe_slot_index = move_index(app.recipe_slot_index, slots, -1);
            }
            KeyCode::Char('[') if app.tab() == Tab::Recipes => {
                let targets = app.recipe_targets();
                match app.editor.cycle_recipe_slot_target(
                    app.recipe_index,
                    app.recipe_slot_index,
                    &targets,
                    -1,
                ) {
                    Ok(()) => app.status = "Recipe slot target staged.".to_owned(),
                    Err(error) => app.status = format!("Could not update recipe: {error:#}"),
                }
            }
            KeyCode::Char(']') if app.tab() == Tab::Recipes => {
                let targets = app.recipe_targets();
                match app.editor.cycle_recipe_slot_target(
                    app.recipe_index,
                    app.recipe_slot_index,
                    &targets,
                    1,
                ) {
                    Ok(()) => app.status = "Recipe slot target staged.".to_owned(),
                    Err(error) => app.status = format!("Could not update recipe: {error:#}"),
                }
            }
            KeyCode::Char('l') if app.tab() == Tab::Recipes => {
                match app
                    .editor
                    .set_recipe_realms(app.recipe_index, vec!["local".to_owned()])
                {
                    Ok(()) => app.status = "Recipe constrained to local providers.".to_owned(),
                    Err(error) => app.status = format!("Could not update recipe: {error:#}"),
                }
            }
            KeyCode::Char('c') if app.tab() == Tab::Recipes => {
                match app
                    .editor
                    .set_recipe_realms(app.recipe_index, vec!["cloud".to_owned()])
                {
                    Ok(()) => app.status = "Recipe constrained to cloud providers.".to_owned(),
                    Err(error) => app.status = format!("Could not update recipe: {error:#}"),
                }
            }
            KeyCode::Char('b') if app.tab() == Tab::Recipes => {
                match app.editor.set_recipe_realms(
                    app.recipe_index,
                    vec!["local".to_owned(), "cloud".to_owned()],
                ) {
                    Ok(()) => app.status = "Recipe allows local and cloud providers.".to_owned(),
                    Err(error) => app.status = format!("Could not update recipe: {error:#}"),
                }
            }
            KeyCode::Char('i') if app.tab() == Tab::Recipes => {
                match app.editor.set_recipe_realms(app.recipe_index, vec![]) {
                    Ok(()) => app.status = "Recipe inherits route and global realms.".to_owned(),
                    Err(error) => app.status = format!("Could not update recipe: {error:#}"),
                }
            }
            KeyCode::Char('o') if app.tab() == Tab::Recipes => {
                match app
                    .editor
                    .cycle_recipe_slot_output_limit(app.recipe_index, app.recipe_slot_index)
                {
                    Ok(Some(tokens)) => {
                        app.status = format!(
                            "Recipe slot output maximum staged at {}.",
                            format_context_length(tokens)
                        )
                    }
                    Ok(None) => app.status = "Recipe slot inherits output maximum.".to_owned(),
                    Err(error) => app.status = format!("Could not update recipe: {error:#}"),
                }
            }
            KeyCode::Char('t') if app.tab() == Tab::Recipes => {
                match app
                    .editor
                    .cycle_recipe_slot_reasoning_effort(app.recipe_index, app.recipe_slot_index)
                {
                    Ok(effort) => {
                        app.status = format!(
                            "Recipe slot reasoning effort staged as {}.",
                            effort.as_str()
                        )
                    }
                    Err(error) => {
                        app.status = format!("Could not update recipe reasoning: {error:#}")
                    }
                }
            }
            KeyCode::Char('d') if app.tab() == Tab::Recipes => {
                match app.editor.toggle_recipe_disabled(app.recipe_index) {
                    Ok(()) => app.status = "Recipe enabled state toggled.".to_owned(),
                    Err(error) => app.status = format!("Could not update recipe: {error:#}"),
                }
            }
            KeyCode::Char('x') if app.tab() == Tab::Recipes => {
                match app.editor.remove_recipe(app.recipe_index) {
                    Ok(recipe) => {
                        app.status = format!("Removed recipe '{}'.", recipe.id);
                        app.clamp_selections();
                    }
                    Err(error) => app.status = format!("Could not remove recipe: {error:#}"),
                }
            }
            KeyCode::Char('a') if app.tab() == Tab::Aliases => app.start_alias_form(false),
            KeyCode::Char('e') if app.tab() == Tab::Aliases => app.start_alias_form(true),
            KeyCode::Enter | KeyCode::Char(' ') if app.tab() == Tab::Aliases => {
                app.start_alias_target_picker();
            }
            KeyCode::Char('[') if app.tab() == Tab::Aliases => {
                match app
                    .editor
                    .cycle_alias_context_length(app.alias_index, false)
                {
                    Ok(Some(tokens)) => {
                        app.status = format!(
                            "Alias context contract staged at {}.",
                            format_context_length(tokens)
                        )
                    }
                    Ok(None) => {
                        app.status =
                            "Alias context will be inherited from its exact target.".to_owned()
                    }
                    Err(error) => app.status = format!("Could not update alias: {error:#}"),
                }
            }
            KeyCode::Char(']') if app.tab() == Tab::Aliases => {
                match app.editor.cycle_alias_context_length(app.alias_index, true) {
                    Ok(Some(tokens)) => {
                        app.status = format!(
                            "Alias context contract staged at {}.",
                            format_context_length(tokens)
                        )
                    }
                    Ok(None) => {
                        app.status =
                            "Alias context will be inherited from its exact target.".to_owned()
                    }
                    Err(error) => app.status = format!("Could not update alias: {error:#}"),
                }
            }
            KeyCode::Char('d') if app.tab() == Tab::Aliases => {
                match app.editor.toggle_alias_disabled(app.alias_index) {
                    Ok(()) => app.status = "Alias enabled state toggled.".to_owned(),
                    Err(error) => app.status = format!("Could not update alias: {error:#}"),
                }
            }
            KeyCode::Char('x') if app.tab() == Tab::Aliases => {
                match app.editor.remove_alias(app.alias_index) {
                    Ok(alias) => {
                        app.status = format!("Removed alias '{}'.", alias.id);
                        app.clamp_selections();
                    }
                    Err(error) => app.status = format!("Could not remove alias: {error:#}"),
                }
            }
            KeyCode::Char(character @ '1'..='5') if app.tab() == Tab::Models => {
                app.apply_archetype(character as usize - '1' as usize);
                app.refresh_simulation().await;
            }
            KeyCode::Char('6') if app.tab() == Tab::Models => {
                app.toggle_selected_model_compression_role();
                app.refresh_simulation().await;
            }
            KeyCode::Char('g') if app.tab() == Tab::Models => {
                app.apply_suggested_archetype();
                app.refresh_simulation().await;
            }
            KeyCode::Char(' ') if app.tab() == Tab::Models => {
                app.toggle_selected_model_auto_routing();
                app.refresh_simulation().await;
            }
            KeyCode::Char('[') if app.tab() == Tab::Models => {
                app.cycle_selected_model_context_length(false);
                app.refresh_simulation().await;
            }
            KeyCode::Char(']') if app.tab() == Tab::Models => {
                app.cycle_selected_model_context_length(true);
                app.refresh_simulation().await;
            }
            KeyCode::Char('x') if app.tab() == Tab::Models => {
                if let Some(model) = app.selected_model() {
                    app.editor.remove_exact_profile(&model);
                    app.status = format!("Removed exact profile for {model}.");
                    app.refresh_simulation().await;
                }
            }
            KeyCode::Char('h') if app.tab() == Tab::Ranking => {
                app.strength_index = app.strength_index.saturating_sub(1)
            }
            KeyCode::Char('l') if app.tab() == Tab::Ranking => {
                app.strength_index = (app.strength_index + 1).min(STRENGTH_KEYS.len() - 1)
            }
            KeyCode::Char('-') if app.tab() == Tab::Ranking => {
                if let Some(model) = app.selected_ranking_model() {
                    app.ensure_exact_ranking_profile(&model);
                    app.editor
                        .adjust_strength(&model, STRENGTH_KEYS[app.strength_index], -0.05);
                    app.refresh_simulation().await;
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') if app.tab() == Tab::Ranking => {
                if let Some(model) = app.selected_ranking_model() {
                    app.ensure_exact_ranking_profile(&model);
                    app.editor
                        .adjust_strength(&model, STRENGTH_KEYS[app.strength_index], 0.05);
                    app.refresh_simulation().await;
                }
            }
            KeyCode::Char('t') if app.tab() == Tab::Ranking => {
                if let Some(model) = app.selected_ranking_model() {
                    app.ensure_exact_ranking_profile(&model);
                    app.editor.adjust_latency(&model, -1);
                }
            }
            KeyCode::Char('T') if app.tab() == Tab::Ranking => {
                if let Some(model) = app.selected_ranking_model() {
                    app.ensure_exact_ranking_profile(&model);
                    app.editor.adjust_latency(&model, 1);
                }
            }
            KeyCode::Char('c') if app.tab() == Tab::Ranking => {
                if let Some(model) = app.selected_ranking_model() {
                    app.ensure_exact_ranking_profile(&model);
                    app.editor.adjust_cost(&model, -1);
                }
            }
            KeyCode::Char('C') if app.tab() == Tab::Ranking => {
                if let Some(model) = app.selected_ranking_model() {
                    app.ensure_exact_ranking_profile(&model);
                    app.editor.adjust_cost(&model, 1);
                }
            }
            KeyCode::Char('[') if app.tab() == Tab::Simulator => {
                app.route_index = move_index(app.route_index, app.editor.config.routes.len(), -1);
                app.refresh_simulation().await;
            }
            KeyCode::Char(']') if app.tab() == Tab::Simulator => {
                app.route_index = move_index(app.route_index, app.editor.config.routes.len(), 1);
                app.refresh_simulation().await;
            }
            KeyCode::Char('s') if app.tab() == Tab::Review => match app.editor.save() {
                Ok(backup) => {
                    app.status = format!(
                        "Saved atomically. Backup: {}. Restart Infernaut to apply.",
                        backup.display()
                    );
                }
                Err(error) => app.status = format!("Save failed: {error:#}"),
            },
            KeyCode::PageDown if app.tab() == Tab::Review => {
                app.review_scroll = app.review_scroll.saturating_add(10)
            }
            KeyCode::PageUp if app.tab() == Tab::Review => {
                app.review_scroll = app.review_scroll.saturating_sub(10)
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.tab = Tab::Review as usize;
            }
            _ => {}
        }
    }
    Ok(())
}

enum FormAction {
    Continue,
    Cancel,
    Save,
}

fn handle_provider_form(form: &mut ProviderForm, key: KeyEvent) -> FormAction {
    match key.code {
        KeyCode::Esc => FormAction::Cancel,
        KeyCode::Enter => FormAction::Save,
        KeyCode::Tab | KeyCode::Down => {
            form.field = (form.field + 1) % ProviderForm::FIELD_NAMES.len();
            FormAction::Continue
        }
        KeyCode::BackTab | KeyCode::Up => {
            form.field = form
                .field
                .checked_sub(1)
                .unwrap_or(ProviderForm::FIELD_NAMES.len() - 1);
            FormAction::Continue
        }
        KeyCode::Left if form.field == 1 => {
            form.cycle_kind(-1);
            FormAction::Continue
        }
        KeyCode::Right if form.field == 1 => {
            form.cycle_kind(1);
            FormAction::Continue
        }
        code => {
            form.edit_text(code);
            FormAction::Continue
        }
    }
}

fn handle_client_form(form: &mut ClientForm, key: KeyEvent) -> FormAction {
    match key.code {
        KeyCode::Esc => FormAction::Cancel,
        KeyCode::Enter => FormAction::Save,
        KeyCode::Tab | KeyCode::Down => {
            form.field = (form.field + 1) % ClientForm::FIELD_NAMES.len();
            FormAction::Continue
        }
        KeyCode::BackTab | KeyCode::Up => {
            form.field = form
                .field
                .checked_sub(1)
                .unwrap_or(ClientForm::FIELD_NAMES.len() - 1);
            FormAction::Continue
        }
        KeyCode::Left if form.field == 3 => {
            form.cycle_recipe(-1);
            FormAction::Continue
        }
        KeyCode::Right | KeyCode::Char(' ') if form.field == 3 => {
            form.cycle_recipe(1);
            FormAction::Continue
        }
        KeyCode::Left if form.field == 4 => {
            form.cycle_selectable_recipe(-1);
            FormAction::Continue
        }
        KeyCode::Right if form.field == 4 => {
            form.cycle_selectable_recipe(1);
            FormAction::Continue
        }
        KeyCode::Char(' ') if form.field == 4 => {
            form.toggle_selectable_recipe();
            FormAction::Continue
        }
        code => {
            form.edit_text(code);
            FormAction::Continue
        }
    }
}

fn handle_recipe_form(form: &mut RecipeForm, key: KeyEvent) -> FormAction {
    match key.code {
        KeyCode::Esc => FormAction::Cancel,
        KeyCode::Enter => FormAction::Save,
        KeyCode::Tab | KeyCode::Down => {
            form.field = (form.field + 1) % RecipeForm::FIELD_NAMES.len();
            FormAction::Continue
        }
        KeyCode::BackTab | KeyCode::Up => {
            form.field = form
                .field
                .checked_sub(1)
                .unwrap_or(RecipeForm::FIELD_NAMES.len() - 1);
            FormAction::Continue
        }
        code => {
            form.edit_text(code);
            FormAction::Continue
        }
    }
}

fn handle_alias_form(form: &mut AliasForm, key: KeyEvent) -> FormAction {
    match key.code {
        KeyCode::Esc => FormAction::Cancel,
        KeyCode::Enter => FormAction::Save,
        KeyCode::Tab | KeyCode::Down => {
            form.field = (form.field + 1) % AliasForm::FIELD_NAMES.len();
            FormAction::Continue
        }
        KeyCode::BackTab | KeyCode::Up => {
            form.field = form
                .field
                .checked_sub(1)
                .unwrap_or(AliasForm::FIELD_NAMES.len() - 1);
            FormAction::Continue
        }
        code => {
            form.edit_text(code);
            FormAction::Continue
        }
    }
}

fn handle_recipe_target_picker(
    picker: &mut RecipeTargetPicker,
    key: KeyEvent,
) -> TargetPickerAction {
    match key.code {
        KeyCode::Esc => TargetPickerAction::Cancel,
        KeyCode::Enter => picker
            .filtered_targets()
            .get(picker.selected)
            .map(|target| TargetPickerAction::Select((*target).to_owned()))
            .unwrap_or(TargetPickerAction::Continue),
        KeyCode::Down => {
            picker.selected = move_index(picker.selected, picker.filtered_targets().len(), 1);
            TargetPickerAction::Continue
        }
        KeyCode::Up => {
            picker.selected = move_index(picker.selected, picker.filtered_targets().len(), -1);
            TargetPickerAction::Continue
        }
        KeyCode::Backspace => {
            picker.query.pop();
            picker.selected = 0;
            picker.clamp();
            TargetPickerAction::Continue
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            picker.query.push(character);
            picker.selected = 0;
            picker.clamp();
            TargetPickerAction::Continue
        }
        _ => TargetPickerAction::Continue,
    }
}

fn handle_alias_target_picker(picker: &mut AliasTargetPicker, key: KeyEvent) -> TargetPickerAction {
    match key.code {
        KeyCode::Esc => TargetPickerAction::Cancel,
        KeyCode::Enter => picker
            .filtered_targets()
            .get(picker.selected)
            .map(|target| TargetPickerAction::Select((*target).to_owned()))
            .unwrap_or(TargetPickerAction::Continue),
        KeyCode::Down => {
            picker.selected = move_index(picker.selected, picker.filtered_targets().len(), 1);
            TargetPickerAction::Continue
        }
        KeyCode::Up => {
            picker.selected = move_index(picker.selected, picker.filtered_targets().len(), -1);
            TargetPickerAction::Continue
        }
        KeyCode::Backspace => {
            picker.query.pop();
            picker.selected = 0;
            picker.clamp();
            TargetPickerAction::Continue
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            picker.query.push(character);
            picker.selected = 0;
            picker.clamp();
            TargetPickerAction::Continue
        }
        _ => TargetPickerAction::Continue,
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(frame.area());
    let titles = Tab::ALL
        .iter()
        .map(|tab| Line::from(tab.title()))
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(titles)
            .select(app.tab)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Infernaut Configure "),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        areas[0],
    );

    match app.tab() {
        Tab::Providers => draw_providers(frame, app, areas[1]),
        Tab::Clients => draw_clients(frame, app, areas[1]),
        Tab::Recipes => draw_recipes(frame, app, areas[1]),
        Tab::Aliases => draw_aliases(frame, app, areas[1]),
        Tab::Models => draw_models(frame, app, areas[1]),
        Tab::Ranking => draw_ranking(frame, app, areas[1]),
        Tab::Routes => draw_routes(frame, app, areas[1]),
        Tab::Privacy => draw_privacy(frame, app, areas[1]),
        Tab::Simulator => draw_simulator(frame, app, areas[1]),
        Tab::Review => draw_review(frame, app, areas[1]),
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if app.editor.is_dirty() {
                    " ● modified "
                } else {
                    " ✓ saved "
                },
                Style::default().fg(if app.editor.is_dirty() {
                    Color::Yellow
                } else {
                    Color::Green
                }),
            ),
            Span::raw(&app.status),
            Span::raw("   Tab/Shift-Tab screens · ↑↓ select · q quit"),
        ]))
        .block(Block::default().borders(Borders::ALL)),
        areas[2],
    );
    if let Some(form) = &app.provider_form {
        draw_provider_form(frame, form);
    }
    if let Some(form) = &app.client_form {
        draw_client_form(frame, form);
    }
    if let Some(form) = &app.recipe_form {
        draw_recipe_form(frame, form);
    }
    if let Some(picker) = &app.recipe_target_picker {
        draw_recipe_target_picker(frame, picker);
    }
    if let Some(form) = &app.alias_form {
        draw_alias_form(frame, form);
    }
    if let Some(picker) = &app.alias_target_picker {
        draw_alias_target_picker(frame, picker);
    }
}

fn draw_providers(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.editor.config.providers.iter().map(|provider| {
        let status = app
            .report
            .providers
            .iter()
            .find(|status| status.id == provider.id);
        Row::new(vec![
            Cell::from(provider.id.clone()),
            Cell::from(provider.alias.clone().unwrap_or_else(|| "—".to_owned())),
            Cell::from(provider_kind_name(provider.kind)),
            Cell::from(provider.realm.clone()),
            Cell::from(provider.priority.to_string()),
            Cell::from(if provider.disabled {
                "disabled".to_owned()
            } else if provider.is_audio_provider() {
                "service".to_owned()
            } else if provider.jit_load {
                "jit".to_owned()
            } else if provider.auto_load {
                "auto-load".to_owned()
            } else {
                "-".to_owned()
            }),
            Cell::from(match status {
                Some(status) if status.available && provider.is_audio_provider() => {
                    format!("static · {} services", status.model_count)
                }
                Some(status) if status.available => format!("✓ {} models", status.model_count),
                Some(status) => format!("✗ {}", status.error.as_deref().unwrap_or("unavailable")),
                None => "not tested".to_owned(),
            }),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(12),
            Constraint::Length(18),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new([
            "Provider",
            "Alias",
            "Kind",
            "Realm",
            "Priority",
            "Runtime",
            "Discovery",
        ])
        .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" a add · e edit · i JIT · o auto-load · d disable · r discover "),
    );
    let mut state = table_state(app.provider_index);
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_clients(frame: &mut Frame, app: &App, area: Rect) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(7), Constraint::Length(8)])
        .split(area);
    let rows = app.editor.config.clients.iter().map(|client| {
        let key_status = client_key_status(&app.editor.config, client);
        let status_color = match key_status {
            ClientKeyStatus::Set => Color::Green,
            ClientKeyStatus::Disabled => Color::DarkGray,
            ClientKeyStatus::Missing | ClientKeyStatus::Empty | ClientKeyStatus::Duplicate => {
                Color::Yellow
            }
        };
        Row::new(vec![
            Cell::from(client.id.clone()),
            Cell::from(client.display_name().to_owned()),
            Cell::from(client.api_key_env.clone()),
            Cell::from(client.recipe.clone().unwrap_or_else(|| "none".to_owned())),
            Cell::from(if client.recipes.is_empty() {
                "none".to_owned()
            } else {
                client.recipes.join(",")
            }),
            Cell::from(key_status.label()).style(Style::default().fg(status_color)),
            Cell::from("inference only"),
            Cell::from(if client.disabled {
                "disabled"
            } else {
                "enabled"
            }),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Length(22),
            Constraint::Percentage(35),
            Constraint::Length(18),
            Constraint::Length(24),
            Constraint::Length(13),
            Constraint::Length(16),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new([
            "Client ID",
            "Display title",
            "API key environment",
            "Recipe",
            "Selectable recipes",
            "Environment",
            "Access",
            "State",
        ])
        .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" a add · e edit · d disable · x remove "),
    );
    let mut state = table_state(app.client_index);
    frame.render_stateful_widget(table, sections[0], &mut state);
    frame.render_widget(
        Paragraph::new(
            "Named clients receive inference-only credentials and stable Mission Control identity.\n\
             TOML stores only an environment-variable name; key values are never displayed or saved here.\n\
             Each enabled client must resolve to a distinct non-empty key that differs from the administrative key.\n\
             Exclusive recipes restrict a key to one dispatch plan; selectable recipes augment the normal catalog and are chosen per session through the harness model picker.\n\
             Configure the environment value outside this TUI, then restart Infernaut after saving.",
        )
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL).title(" Client key boundary ")),
        sections[1],
    );
}

fn draw_recipes(frame: &mut Frame, app: &App, area: Rect) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Min(8),
            Constraint::Length(5),
        ])
        .split(area);
    let recipe_rows = app.editor.config.recipes.iter().map(|recipe| {
        let clients = app
            .editor
            .config
            .clients
            .iter()
            .filter(|client| client.authorizes_recipe(&recipe.id))
            .map(|client| client.id.as_str())
            .collect::<Vec<_>>()
            .join(",");
        Row::new(vec![
            Cell::from(recipe.id.clone()),
            Cell::from(recipe.display_name().to_owned()),
            Cell::from(format!("{:?}", recipe.kind)),
            Cell::from(format!("{:?}", recipe.protocol)),
            Cell::from(if recipe.allowed_realms.is_empty() {
                "inherit".to_owned()
            } else {
                recipe.allowed_realms.join(",")
            }),
            Cell::from(clients),
            Cell::from(if recipe.disabled {
                "disabled"
            } else {
                "enabled"
            }),
        ])
    });
    let recipes = Table::new(
        recipe_rows,
        [
            Constraint::Length(20),
            Constraint::Length(20),
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Min(16),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new([
            "Recipe ID",
            "Title",
            "Harness",
            "Protocol",
            "Realms",
            "Clients",
            "State",
        ])
        .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .block(Block::default().borders(Borders::ALL).title(
        " a Claude template · h Hermes template · e edit name · j/k recipe · l/c/b realms · i inherit · d disable · x remove ",
    ));
    let mut recipe_state = table_state(app.recipe_index);
    frame.render_stateful_widget(recipes, sections[0], &mut recipe_state);

    let slot_rows = app
        .editor
        .config
        .recipes
        .get(app.recipe_index)
        .into_iter()
        .flat_map(|recipe| recipe.slots.iter())
        .map(|slot| {
            Row::new(vec![
                Cell::from(slot.id.clone()),
                Cell::from(slot.display_name().to_owned()),
                Cell::from(format!("{:?}", slot.trigger)),
                Cell::from(slot.target.clone()),
                Cell::from(
                    slot.max_output_tokens
                        .map(format_context_length)
                        .unwrap_or_else(|| "inherit".to_owned()),
                ),
                Cell::from(slot.reasoning_effort.as_str()),
            ])
        });
    let slots =
        Table::new(
            slot_rows,
            [
                Constraint::Length(16),
                Constraint::Length(24),
                Constraint::Length(14),
                Constraint::Min(36),
                Constraint::Length(12),
                Constraint::Length(10),
            ],
        )
        .header(
            Row::new([
                "Slot",
                "Purpose",
                "Trigger",
                "Target model or route",
                "Output max",
                "Reasoning",
            ])
            .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL).title(
            " n/p slot · Enter choose target · [/] quick cycle · o output max · t thinking ",
        ));
    let mut slot_state = table_state(app.recipe_slot_index);
    frame.render_stateful_widget(slots, sections[1], &mut slot_state);
    let dispatch_contract = match app
        .editor
        .config
        .recipes
        .get(app.recipe_index)
        .map(|recipe| recipe.kind)
    {
        Some(RecipeKind::Hermes) => {
            "Primary is selected through Hermes /model. Delegation and compression are internal, activating only for Hermes' focused subagent contract or structured context-checkpoint prompt. Authorize one or more Hermes recipes as selectable on the named client; ordinary models and routes remain visible."
        }
        Some(RecipeKind::ClaudeCode) => {
            "Primary handles normal turns. Background maps Claude's Haiku alias; subagent maps CLAUDE_CODE_SUBAGENT_MODEL. Classifier and compression are selected internally only by their narrow Claude signatures. Bind this recipe exclusively on the Claude client."
        }
        _ => {
            "Model-triggered slots are selectable; classifier and compression triggers remain internal. Exclusive client binding hides the ordinary catalog, while selectable authorization augments it."
        }
    };
    frame.render_widget(
        Paragraph::new(dispatch_contract)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Dispatch contract "),
            ),
        sections[2],
    );
}

fn draw_aliases(frame: &mut Frame, app: &App, area: Rect) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(7)])
        .split(area);
    let rows = app.editor.config.aliases.iter().map(|alias| {
        let inherited_context = app
            .catalog
            .target_context_length(&app.editor.config, &alias.target);
        let contract = alias
            .context_length
            .map(format_context_length)
            .unwrap_or_else(|| "inherit".to_owned());
        let effective = alias
            .context_length
            .or(inherited_context)
            .map(format_context_length)
            .unwrap_or_else(|| "unknown".to_owned());
        let available = app
            .catalog
            .alias_target_available(&app.editor.config, alias);
        Row::new(vec![
            Cell::from(alias.qualified_id()),
            Cell::from(if alias.title.is_empty() {
                alias.id.clone()
            } else {
                alias.title.clone()
            }),
            Cell::from(alias.target.clone()),
            Cell::from(contract),
            Cell::from(effective),
            Cell::from(if alias.disabled {
                "disabled"
            } else if available {
                "ready"
            } else {
                "unavailable"
            }),
        ])
    });
    let table =
        Table::new(
            rows,
            [
                Constraint::Length(22),
                Constraint::Length(22),
                Constraint::Min(38),
                Constraint::Length(11),
                Constraint::Length(11),
                Constraint::Length(12),
            ],
        )
        .header(
            Row::new([
                "Alias",
                "Title",
                "Exact target",
                "Contract",
                "Advertised",
                "State",
            ])
            .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL).title(
            " a add · e edit · Enter choose exact target · [/] context · d disable · x remove ",
        ));
    let mut state = table_state(app.alias_index);
    frame.render_stateful_widget(table, sections[0], &mut state);
    frame.render_widget(
        Paragraph::new(
            "Aliases publish as alias:<id>. They are stable, deterministic pointers to a physical model, route, or authorized recipe primary: no additional classification, ranking, or fallback is performed. A missing, policy-denied, or undersized target fails closed. 'inherit' advertises the target's detected context; a fixed contract gates both the target and each request.",
        )
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Exact model pointers "),
        ),
        sections[1],
    );
}

fn draw_models(frame: &mut Frame, app: &App, area: Rect) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(10)])
        .split(area);
    let reconciled = reconcile_models(&app.editor.config, &app.catalog);
    let rows = reconciled.iter().map(|row| {
        let color = match row.state {
            ModelProfileState::Unclassified => Color::Yellow,
            ModelProfileState::Dormant => Color::DarkGray,
            ModelProfileState::Exact => Color::Green,
            ModelProfileState::Wildcard => Color::Cyan,
        };
        let suggestion = app
            .catalog
            .models
            .iter()
            .find(|model| model.id == row.model)
            .map(|model| suggested_roles_name(suggest_archetype(model)))
            .unwrap_or_else(|| "-".to_owned());
        let applied = row
            .matched_profile
            .as_ref()
            .and_then(|matched| {
                app.editor
                    .config
                    .routing
                    .models
                    .iter()
                    .find(|profile| profile.model == *matched)
            })
            .map(applied_roles_name)
            .unwrap_or_else(|| "-".to_owned());
        let context_override = row.matched_profile.as_ref().and_then(|matched| {
            app.editor
                .config
                .routing
                .models
                .iter()
                .find(|profile| profile.model == *matched)
                .and_then(|profile| profile.context_length)
        });
        let context = app
            .catalog
            .models
            .iter()
            .find(|model| model.id == row.model)
            .map(|model| model_context_display(model, context_override))
            .unwrap_or_else(|| {
                context_override
                    .map(|tokens| format!("{}*", format_context_length(tokens)))
                    .unwrap_or_else(|| "—".to_owned())
            });
        Row::new(vec![
            Cell::from(format!("{:?}", row.state)).style(Style::default().fg(color)),
            Cell::from(row.model.clone()),
            Cell::from(context),
            Cell::from(
                row.loaded
                    .map(|loaded| if loaded { "yes" } else { "no" })
                    .unwrap_or("?"),
            ),
            Cell::from(
                row.auto_routing
                    .map(|enabled| if enabled { "route" } else { "reserved" })
                    .unwrap_or("-"),
            ),
            Cell::from(applied),
            Cell::from(suggestion),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(13),
            Constraint::Percentage(35),
            Constraint::Length(13),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Percentage(18),
            Constraint::Percentage(22),
        ],
    )
    .header(
        Row::new([
            "State",
            "Model",
            "Context",
            "Loaded",
            "Routing",
            "Applied roles",
            "Suggested roles",
        ])
        .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .block(Block::default().borders(Borders::ALL).title(
        " Space route/reserve · [/] context · g suggest · 1 Fast · 2 Inspect · 3 Balanced · 4 Heavy+data · 5 Vision · 6 Compression · x remove ",
    ));
    let mut state = table_state(app.model_index);
    frame.render_stateful_widget(table, sections[0], &mut state);

    frame.render_widget(
        Paragraph::new(
            "Fast — low-latency chat and light work\n\
             Inspect — bounded code reading, review, and debugging\n\
             Balanced — mixed everyday work without a strong specialization\n\
             Heavy + data — architecture, substantial implementation, migrations, SQL, and analytics\n\
             Vision — additive image/screenshot/UI specialization; combine with any workload role\n\
             Compression — additive allowlist for detected Claude Code compaction; reserved models become eligible only then\n\
             Context — [ and ] cycle an operator override; * marks override, ? means provider detection was unavailable\n\
             * after an applied role means its strengths were subsequently customized",
        )
        .block(Block::default().borders(Borders::ALL).title(" Role reminder ")),
        sections[1],
    );
}

fn draw_ranking(frame: &mut Frame, app: &App, area: Rect) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(6)])
        .split(area);
    let rows = app.catalog.models.iter().map(|model| {
        let profile = app
            .editor
            .config
            .routing
            .models
            .iter()
            .find(|profile| profile.model == model.id);
        let mut cells = vec![Cell::from(model.id.clone())];
        for key in STRENGTH_KEYS {
            cells.push(Cell::from(format!(
                "{:.2}",
                profile
                    .and_then(|profile| profile.strengths.get(key))
                    .copied()
                    .unwrap_or(0.50)
            )));
        }
        cells.push(Cell::from(
            profile
                .map(|profile| profile.traits.latency_tier.to_string())
                .unwrap_or_else(|| "-".to_owned()),
        ));
        cells.push(Cell::from(
            profile
                .map(|profile| profile.traits.cost_tier.to_string())
                .unwrap_or_else(|| "-".to_owned()),
        ));
        Row::new(cells)
    });
    let mut header = vec![Cell::from("Model")];
    header.extend(STRENGTH_KEYS.iter().map(|key| {
        Cell::from(*key).style(if *key == STRENGTH_KEYS[app.strength_index] {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        })
    }));
    header.push(Cell::from("lat"));
    header.push(Cell::from("cost"));
    let mut widths = vec![Constraint::Length(36)];
    widths.extend((0..STRENGTH_KEYS.len()).map(|_| Constraint::Length(9)));
    widths.push(Constraint::Length(5));
    widths.push(Constraint::Length(5));
    let table = Table::new(rows, widths)
        .header(Row::new(header))
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" h/l workload · -/+ score · t/T latency -/+ · c/C cost -/+ "),
        );
    let mut state = table_state(app.ranking_model_index);
    frame.render_stateful_widget(table, sections[0], &mut state);

    let selected = app
        .selected_ranking_model()
        .unwrap_or_else(|| "No model".to_owned());
    frame.render_widget(
        Paragraph::new(format!(
            "Selected: {selected}\nWorkload: {}\nProfiles are stored by canonical ID; missing models remain dormant and newly discovered models remain unclassified.",
            STRENGTH_KEYS[app.strength_index]
        ))
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL).title(" Durable ranking ")),
        sections[1],
    );
}

fn draw_routes(frame: &mut Frame, app: &App, area: Rect) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(5)])
        .split(area);
    let selected_route = app.editor.config.routes.get(app.route_index);
    let selected_id = selected_route
        .map(|route| route.qualified_id())
        .unwrap_or_else(|| "-".to_owned());
    let selected_contract = selected_route
        .and_then(|route| route.context_length)
        .map(|tokens| format!("{} fixed", format_context_length(tokens)))
        .unwrap_or_else(|| "none (catalog-derived)".to_owned());
    let selected_pool = selected_route
        .map(|route| app.catalog.route_context_pool(&app.editor.config, route))
        .map(|pool| format!("{}/{}", pool.eligible, pool.total))
        .unwrap_or_else(|| "-".to_owned());
    let selected_estimate = selected_route
        .and_then(|route| route.token_estimate_bytes_per_token)
        .map(|ratio| format!("{ratio} bytes/token"))
        .unwrap_or_else(|| "3.0 bytes/token (default)".to_owned());
    let selected_compression_limit = selected_route
        .and_then(|route| route.compression_max_output_tokens)
        .map(format_context_length)
        .unwrap_or_else(|| "harness max".to_owned());
    frame.render_widget(
        Paragraph::new(format!(
            "Routing mode: {} (m)   Loaded preference: {} (w)\nSelected: {}   Contract: {}   Eligible: {}   Estimate: {}   Compression max: {}\nContracts gate an existing route; they do not create additional route IDs.\nShadow recommends without changing the actual model; Active applies rankings.",
            routing_mode_name(app.editor.config.routing.mode),
            loaded_model_preference_name(app.editor.config.routing.loaded_model_preference),
            selected_id,
            selected_contract,
            selected_pool,
            selected_estimate,
            selected_compression_limit,
        ))
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL).title(" Global routing ")),
        sections[0],
    );
    let rows = app.editor.config.routes.iter().map(|route| {
        let pool = app.catalog.route_context_pool(&app.editor.config, route);
        Row::new(vec![
            Cell::from(route.qualified_id()),
            Cell::from(
                route
                    .context_length
                    .map(format_context_length)
                    .unwrap_or_else(|| "none".to_owned()),
            ),
            Cell::from(
                route
                    .token_estimate_bytes_per_token
                    .map(|ratio| format!("{ratio:.2}"))
                    .unwrap_or_else(|| "3.00*".to_owned()),
            ),
            Cell::from(
                route
                    .compression_max_output_tokens
                    .map(format_context_length)
                    .unwrap_or_else(|| "-".to_owned()),
            ),
            Cell::from(format!("{}/{}", pool.eligible, pool.total)),
            Cell::from(format!("{:?}", route.strategy)),
            Cell::from(route.task.as_str()),
            Cell::from(route.effort.as_str()),
            Cell::from(route.allowed_realms.join(",")),
            Cell::from(if route.candidates.is_empty() {
                "all eligible".to_owned()
            } else {
                route.candidates.len().to_string()
            }),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(28),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Length(15),
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Length(14),
            Constraint::Min(12),
        ],
    )
    .header(Row::new([
        "Route",
        "Contract",
        "B/token",
        "Comp max",
        "Eligible",
        "Strategy",
        "Task",
        "Effort",
        "Realms",
        "Candidates",
    ]))
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .block(Block::default().borders(Borders::ALL).title(
        " ↑↓ route · [/] contract · i input estimate · o compression max · l local · c cloud · b both · t task · e effort · a strategy ",
    ));
    let mut state = table_state(app.route_index);
    frame.render_stateful_widget(table, sections[1], &mut state);
}

fn draw_privacy(frame: &mut Frame, app: &App, area: Rect) {
    let mode = app.editor.config.privacy_lock.mode;
    let local = app
        .editor
        .config
        .providers
        .iter()
        .filter(|provider| provider.realm == "local" && !provider.disabled)
        .count();
    let cloud = app
        .editor
        .config
        .providers
        .iter()
        .filter(|provider| provider.realm == "cloud" && !provider.disabled)
        .count();
    let explanation = match mode {
        PrivacyMode::Off => {
            "All configured provider realms may be used. Route constraints still apply."
        }
        PrivacyMode::Selective => {
            "Local and cloud may coexist. Private routes and local-only request policy remain fail-closed."
        }
        PrivacyMode::LocalOnly => {
            "Cloud providers are globally ineligible. No fallback to cloud occurs when local models are unavailable."
        }
    };
    frame.render_widget(
        Paragraph::new(format!(
            "Privacy lock: {}   (press p to cycle)\n\n{}\n\nConfigured providers: {local} local · {cloud} cloud\n\nSafety invariant\nA route with no eligible model returns an error. Infernaut never relaxes a privacy boundary to make a request succeed.",
            privacy_mode_name(mode),
            explanation,
        ))
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Privacy is explicit and fail-closed "),
        ),
        area,
    );
}

fn draw_simulator(frame: &mut Frame, app: &App, area: Rect) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(8)])
        .split(area);
    let route = app
        .editor
        .config
        .routes
        .get(app.route_index)
        .map(|route| route.qualified_id())
        .unwrap_or_else(|| "-".to_owned());
    let route_context = app
        .editor
        .config
        .routes
        .get(app.route_index)
        .and_then(|route| route.context_length)
        .map(|tokens| format!("{} fixed", format_context_length(tokens)))
        .unwrap_or_else(|| "none (catalog-derived)".to_owned());
    let audio_service = app
        .editor
        .config
        .routes
        .get(app.route_index)
        .and_then(|route| {
            [AudioService::Transcription, AudioService::Speech]
                .into_iter()
                .find(|service| {
                    app.catalog
                        .route_is_audio(&app.editor.config, route, *service)
                })
        });
    let (scenario_title, scenario_prompt, scenario_tools) = match audio_service {
        Some(AudioService::Transcription) => (
            "Audio transcription (endpoint-driven)",
            "Opaque multipart audio; only the model field participates in routing.",
            "not applicable",
        ),
        Some(AudioService::Speech) => (
            "Speech synthesis (endpoint-driven)",
            "OpenAI speech JSON; only the model field participates in routing.",
            "not applicable",
        ),
        None => (
            SCENARIOS[app.scenario_index].title,
            SCENARIOS[app.scenario_index].prompt,
            SCENARIOS[app.scenario_index].tools.label(),
        ),
    };
    frame.render_widget(
        Paragraph::new(format!(
            "Scenario: {}\nPrompt: {}\nTools: {}\nRoute: {}   ([/])   Route contract: {}   Mode: {} (m)   Loaded: {} (w)",
            scenario_title,
            scenario_prompt,
            scenario_tools,
            route,
            route_context,
            routing_mode_name(app.editor.config.routing.mode),
            loaded_model_preference_name(app.editor.config.routing.loaded_model_preference),
        ))
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" ↑↓ scenario · [/] route · no provider inference "),
        ),
        sections[0],
    );
    if let Some(resolved) = &app.simulation_service {
        let (endpoint, explanation) = match audio_service {
            Some(AudioService::Speech) => (
                "POST /v1/audio/speech or /v1/audio/speech/stream",
                "Infernaut rewrites only the model field and streams WAV bytes without passing binary data through text diagnostics or plugins.",
            ),
            _ => (
                "POST /v1/audio/transcriptions",
                "The multipart file remains opaque. Infernaut rewrites only the model field and never copies uploaded audio into plugins.",
            ),
        };
        frame.render_widget(
            Paragraph::new(format!(
                "Endpoint: {endpoint}\nClassification: none (explicit service route)\n\nWinner: {}\nProvider: {}\nUpstream model: {}\nRealm: {}\n\n{explanation} Authentication, privacy, metrics, and fail-closed route eligibility still apply.",
                resolved.qualified_id,
                resolved.provider,
                resolved.upstream_id,
                resolved.realm,
            ))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" Service routing explanation ")),
            sections[1],
        );
    } else if let Some(decision) = &app.simulation {
        let alternatives = decision
            .candidates
            .iter()
            .take(8)
            .map(|candidate| {
                format!(
                    "{:>7.2}  {:<50} {}",
                    candidate.score,
                    candidate.model,
                    candidate.reasons.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let rejected = decision
            .rejected
            .iter()
            .take(8)
            .map(|candidate| format!("{:<50} {}", candidate.model, candidate.reasons.join(", ")))
            .collect::<Vec<_>>()
            .join("\n");
        frame.render_widget(
            Paragraph::new(format!(
                "Classification: {} / {} / {}   confidence {:.2}   compression: {}\nIntent: {} B   scaffold: {} B   required context: ~{} tokens\nSignals: {}\n\nWinner: {}\nReasons: {}\n\nCandidates\n{}\n\nRejected\n{}",
                decision.classification.task.as_str(),
                decision.classification.operation.as_str(),
                decision.classification.effort.as_str(),
                decision.classification.confidence,
                if decision.classification.compression_request {
                    "yes"
                } else {
                    "no"
                },
                decision.classification.intent_bytes,
                decision.classification.scaffold_bytes,
                decision.classification.estimated_required_tokens,
                decision.classification.signals.join(", "),
                decision.selected_model,
                decision.selection_reasons.join(", "),
                alternatives,
                if rejected.is_empty() { "—" } else { &rejected }
            ))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" Routing explanation ")),
            sections[1],
        );
    } else {
        frame.render_widget(
            Paragraph::new(
                app.simulation_error
                    .as_deref()
                    .unwrap_or("No simulation result."),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Simulation error "),
            ),
            sections[1],
        );
    }
}

fn draw_review(frame: &mut Frame, app: &App, area: Rect) {
    let diff = app.editor.diff().join("\n");
    let issue_summary = if app.report.issues.is_empty() {
        "Doctor: no issues".to_owned()
    } else {
        format!(
            "Doctor: {} issue(s): {}",
            app.report.issues.len(),
            app.report
                .issues
                .iter()
                .map(|issue| format!("{}:{}", issue.severity, issue.code))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{issue_summary}\nConfig: {}\n\n{}\n",
            app.editor.path().display(),
            diff
        ))
        .scroll((app.review_scroll, 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" s validate + backup + atomic save · PgUp/PgDn scroll "),
        ),
        area,
    );
}

fn draw_provider_form(frame: &mut Frame, form: &ProviderForm) {
    let area = centered_rect(76, 82, frame.area());
    frame.render_widget(Clear, area);
    let rows = ProviderForm::FIELD_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| {
            Row::new(vec![Cell::from(*name), Cell::from(form.value(index))]).style(
                if index == form.field {
                    Style::default().bg(Color::DarkGray).fg(Color::Cyan)
                } else {
                    Style::default()
                },
            )
        });
    let title = if form.original_id.is_some() {
        " Edit provider "
    } else {
        " Add provider "
    };
    let table = Table::new(rows, [Constraint::Length(16), Constraint::Min(30)])
        .block(Block::default().borders(Borders::ALL).title(title));
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(4)])
        .split(area);
    frame.render_widget(table, sections[0]);
    frame.render_widget(
        Paragraph::new(format!(
            "Tab fields · ←/→ provider kind · Enter stage · Esc cancel\n{}",
            form.error.as_deref().unwrap_or(
                if form.provider.kind == ProviderKind::OpenaiAudio {
                    "Paired setup stages exclusive STT and TTS profiles plus explicit routes. Voice discovery follows the sole TTS route. No audio is uploaded during configuration."
                } else if form.provider.kind == ProviderKind::OpenaiTranscription {
                    "Legacy STT-only setup stages an exclusive Transcription profile and explicit route. No audio is uploaded during configuration."
                } else {
                    "Credentials are environment-variable names only. Staging performs no inference."
                }
            )
        ))
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL)),
        sections[1],
    );
}

fn draw_client_form(frame: &mut Frame, form: &ClientForm) {
    let area = centered_rect(64, 48, frame.area());
    frame.render_widget(Clear, area);
    let rows = ClientForm::FIELD_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| {
            Row::new(vec![Cell::from(*name), Cell::from(form.value(index))]).style(
                if index == form.field {
                    Style::default().bg(Color::DarkGray).fg(Color::Cyan)
                } else {
                    Style::default()
                },
            )
        });
    let title = if form.original_id.is_some() {
        " Edit named client "
    } else {
        " Add named client "
    };
    let table = Table::new(rows, [Constraint::Length(18), Constraint::Min(30)])
        .block(Block::default().borders(Borders::ALL).title(title));
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(5)])
        .split(area);
    frame.render_widget(table, sections[0]);
    frame.render_widget(
        Paragraph::new(format!(
            "Tab fields · ←/→ choose recipe · Space toggle selectable · Enter stage · Esc cancel\n{}\nExclusive recipe locks the catalog to one plan. Selectable recipes augment the normal catalog. The secret value remains outside TOML and is never shown.",
            form.error.as_deref().unwrap_or(
                "Use a dedicated environment variable for this inference-only client key."
            )
        ))
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL)),
        sections[1],
    );
}

fn draw_recipe_form(frame: &mut Frame, form: &RecipeForm) {
    let area = centered_rect(60, 34, frame.area());
    frame.render_widget(Clear, area);
    let rows = RecipeForm::FIELD_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| {
            Row::new(vec![Cell::from(*name), Cell::from(form.value(index))]).style(
                if index == form.field {
                    Style::default().bg(Color::DarkGray).fg(Color::Cyan)
                } else {
                    Style::default()
                },
            )
        });
    let table = Table::new(rows, [Constraint::Length(18), Constraint::Min(26)]).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Edit recipe "),
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(5)])
        .split(area);
    frame.render_widget(table, sections[0]);
    frame.render_widget(
        Paragraph::new(format!(
            "Tab fields · Enter stage · Esc cancel\n{}\nChanging the ID also updates exclusive and selectable client bindings.",
            form.error
                .as_deref()
                .unwrap_or("The display title is cosmetic; the ID is part of the published recipe model identifier.")
        ))
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL)),
        sections[1],
    );
}

fn draw_alias_form(frame: &mut Frame, form: &AliasForm) {
    let area = centered_rect(62, 36, frame.area());
    frame.render_widget(Clear, area);
    let rows = AliasForm::FIELD_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| {
            Row::new(vec![Cell::from(*name), Cell::from(form.value(index))]).style(
                if index == form.field {
                    Style::default().bg(Color::DarkGray).fg(Color::Cyan)
                } else {
                    Style::default()
                },
            )
        });
    let table = Table::new(rows, [Constraint::Length(18), Constraint::Min(28)]).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Edit exact model alias "),
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(6)])
        .split(area);
    frame.render_widget(table, sections[0]);
    frame.render_widget(
        Paragraph::new(format!(
            "Tab fields · Enter stage · Esc cancel\n{}\nThe public model ID is alias:<id>. Choose its one target from the Aliases screen after saving this form.",
            form.error
                .as_deref()
                .unwrap_or("Use a short, durable ID such as cron, batch, or nightly-review.")
        ))
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL)),
        sections[1],
    );
}

fn draw_recipe_target_picker(frame: &mut Frame, picker: &RecipeTargetPicker) {
    let area = centered_rect(84, 82, frame.area());
    frame.render_widget(Clear, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            "Filter: {}{}",
            picker.query,
            if picker.query.is_empty() {
                "type a provider or model name"
            } else {
                ""
            }
        ))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Find target "),
        ),
        sections[0],
    );
    let filtered = picker.filtered_targets();
    let rows = filtered
        .iter()
        .map(|target| Row::new([Cell::from((*target).to_owned())]));
    let table = Table::new(rows, [Constraint::Percentage(100)])
        .header(
            Row::new([format!("Model or route ({} matches)", filtered.len())])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL));
    let mut state = table_state(picker.selected);
    frame.render_stateful_widget(table, sections[1], &mut state);
    frame.render_widget(
        Paragraph::new("↑/↓ select · type to filter · Backspace edit · Enter assign · Esc cancel")
            .block(Block::default().borders(Borders::ALL)),
        sections[2],
    );
}

fn draw_alias_target_picker(frame: &mut Frame, picker: &AliasTargetPicker) {
    let area = centered_rect(84, 82, frame.area());
    frame.render_widget(Clear, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(4),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            "Filter: {}{}",
            picker.query,
            if picker.query.is_empty() {
                "type a provider, model, route, or recipe name"
            } else {
                ""
            }
        ))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Find exact target "),
        ),
        sections[0],
    );
    let filtered = picker.filtered_targets();
    let rows = filtered
        .iter()
        .map(|target| Row::new([Cell::from((*target).to_owned())]));
    let table = Table::new(rows, [Constraint::Percentage(100)])
        .header(
            Row::new([format!(
                "Physical model, route, or recipe primary ({} matches)",
                filtered.len()
            )])
            .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL));
    let mut state = table_state(picker.selected);
    frame.render_stateful_widget(table, sections[1], &mut state);
    frame.render_widget(
        Paragraph::new(
            "↑/↓ select · type to filter · Backspace edit · Enter assign · Esc cancel\nThe selected target is exclusive; Infernaut will not substitute another target.",
        )
        .block(Block::default().borders(Borders::ALL)),
        sections[2],
    );
}

fn model_context_display(
    model: &crate::catalog::DiscoveredModel,
    staged_override: Option<u64>,
) -> String {
    let catalog_override = model
        .provider_metadata
        .get("context_length_override")
        .and_then(serde_json::Value::as_u64);
    let detected = model
        .provider_metadata
        .get("detected_context_length")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            catalog_override
                .is_none()
                .then(|| model.context_length())
                .flatten()
        });
    if let Some(override_length) = staged_override {
        return match detected.filter(|detected| *detected != override_length) {
            Some(detected) => format!(
                "{}*/{}",
                format_context_length(override_length),
                format_context_length(detected)
            ),
            None => format!("{}*", format_context_length(override_length)),
        };
    }

    let effective = if catalog_override.is_some() {
        detected
    } else {
        model.context_length()
    };
    let supported = model
        .provider_metadata
        .get("maximum_context_length")
        .and_then(serde_json::Value::as_u64)
        .filter(|supported| Some(*supported) != effective);
    match (effective, supported) {
        (Some(effective), Some(supported)) => format!(
            "{}/{}",
            format_context_length(effective),
            format_context_length(supported)
        ),
        (Some(effective), None) => format_context_length(effective),
        (None, _) => "?".to_owned(),
    }
}

fn format_context_length(tokens: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * KIB;
    if tokens >= MIB && tokens.is_multiple_of(MIB) {
        format!("{}M", tokens / MIB)
    } else if tokens.is_multiple_of(KIB) {
        format!("{}K", tokens / KIB)
    } else if tokens >= 1000 {
        format!("{}K", tokens.div_ceil(1000))
    } else {
        tokens.to_string()
    }
}

fn staged_route_context_status(app: &App) -> String {
    app.editor
        .config
        .routes
        .get(app.route_index)
        .map(|route| {
            let contract = route
                .context_length
                .map(|tokens| format!("{} fixed", format_context_length(tokens)))
                .unwrap_or_else(|| "none (catalog-derived)".to_owned());
            format!(
                "{} contract staged as {contract}; use Review & Save to persist.",
                route.qualified_id()
            )
        })
        .unwrap_or_else(|| "No route selected.".to_owned())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("failed to enable terminal raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout)).context("failed to initialize terminal")
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn table_state(selected: usize) -> ratatui::widgets::TableState {
    let mut state = ratatui::widgets::TableState::default();
    state.select(Some(selected));
    state
}

fn clamp_index(index: usize, length: usize) -> usize {
    if length == 0 {
        0
    } else {
        index.min(length - 1)
    }
}

fn move_index(index: usize, length: usize, delta: isize) -> usize {
    if length == 0 {
        return 0;
    }
    (index as isize + delta).rem_euclid(length as isize) as usize
}

fn unique_provider_id(providers: &[ProviderConfig]) -> String {
    for suffix in 1.. {
        let candidate = if suffix == 1 {
            "new-provider".to_owned()
        } else {
            format!("new-provider-{suffix}")
        };
        if providers.iter().all(|provider| provider.id != candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn unique_client_id(clients: &[ClientConfig]) -> String {
    for suffix in 1.. {
        let candidate = if suffix == 1 {
            "new-client".to_owned()
        } else {
            format!("new-client-{suffix}")
        };
        if clients.iter().all(|client| client.id != candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn unique_recipe_id(recipes: &[RecipeConfig], base: &str) -> String {
    if !recipes.iter().any(|recipe| recipe.id == base) {
        return base.to_owned();
    }
    for suffix in 2.. {
        let candidate = format!("{base}-{suffix}");
        if !recipes.iter().any(|recipe| recipe.id == candidate) {
            return candidate;
        }
    }
    unreachable!("an unused recipe id always exists")
}

fn unique_alias_id(aliases: &[ModelAliasConfig]) -> String {
    let base = "cron";
    if !aliases.iter().any(|alias| alias.id == base) {
        return base.to_owned();
    }
    (2..)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !aliases.iter().any(|alias| alias.id == *candidate))
        .expect("an unused alias ID exists")
}

fn title_from_id(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn client_environment_name(id: &str) -> String {
    let normalized = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("INFERNAUT_{}_KEY", normalized.trim_matches('_'))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientKeyStatus {
    Set,
    Missing,
    Empty,
    Duplicate,
    Disabled,
}

impl ClientKeyStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Missing => "missing",
            Self::Empty => "empty",
            Self::Duplicate => "duplicate key",
            Self::Disabled => "not required",
        }
    }
}

fn client_key_status(config: &Config, client: &ClientConfig) -> ClientKeyStatus {
    if client.disabled {
        return ClientKeyStatus::Disabled;
    }
    let Ok(value) = std::env::var(&client.api_key_env) else {
        return ClientKeyStatus::Missing;
    };
    if value.is_empty() {
        return ClientKeyStatus::Empty;
    }
    let duplicates_admin = config
        .server
        .api_key_env
        .as_ref()
        .and_then(|environment| std::env::var(environment).ok())
        .is_some_and(|admin| admin == value);
    let duplicates_client = config.clients.iter().any(|other| {
        other.id != client.id
            && !other.disabled
            && std::env::var(&other.api_key_env).is_ok_and(|other_value| other_value == value)
    });
    if duplicates_admin || duplicates_client {
        ClientKeyStatus::Duplicate
    } else {
        ClientKeyStatus::Set
    }
}

fn nonempty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_defaults_are_human_readable_and_shell_friendly() {
        assert_eq!(title_from_id("claude-code"), "Claude Code");
        assert_eq!(
            client_environment_name("claude-code"),
            "INFERNAUT_CLAUDE_CODE_KEY"
        );
        assert_eq!(
            unique_client_id(&[
                ClientConfig {
                    id: "new-client".into(),
                    title: String::new(),
                    api_key_env: "FIRST".into(),
                    recipe: None,
                    recipes: vec![],
                    disabled: false,
                },
                ClientConfig {
                    id: "new-client-2".into(),
                    title: String::new(),
                    api_key_env: "SECOND".into(),
                    recipe: None,
                    recipes: vec![],
                    disabled: false,
                },
            ]),
            "new-client-3"
        );
    }

    #[test]
    fn editing_a_generated_client_id_keeps_generated_fields_in_sync() {
        let mut form = ClientForm {
            client: ClientConfig {
                id: "new-client".into(),
                title: "New Client".into(),
                api_key_env: "INFERNAUT_NEW_CLIENT_KEY".into(),
                recipe: None,
                recipes: vec![],
                disabled: false,
            },
            original_id: None,
            recipe_ids: vec![],
            selectable_recipe_index: 0,
            field: 0,
            error: None,
        };
        form.edit_text(KeyCode::Char('2'));
        assert_eq!(form.client.id, "new-client2");
        assert_eq!(form.client.title, "New Client2");
        assert_eq!(form.client.api_key_env, "INFERNAUT_NEW_CLIENT2_KEY");

        form.client.title = "My workstation".into();
        form.client.api_key_env = "CUSTOM_CLIENT_KEY".into();
        form.edit_text(KeyCode::Char('x'));
        assert_eq!(form.client.id, "new-client2x");
        assert_eq!(form.client.title, "My workstation");
        assert_eq!(form.client.api_key_env, "CUSTOM_CLIENT_KEY");
    }

    #[test]
    fn editing_recipe_id_keeps_generated_title_in_sync() {
        let mut form = RecipeForm {
            recipe: RecipeConfig {
                id: "hermes".into(),
                title: "Hermes".into(),
                kind: RecipeKind::Hermes,
                protocol: crate::config::RecipeProtocol::Openai,
                allowed_realms: vec![],
                slots: vec![],
                disabled: false,
            },
            original_id: "hermes".into(),
            field: 0,
            error: None,
        };

        form.edit_text(KeyCode::Char('-'));
        form.edit_text(KeyCode::Char('c'));
        assert_eq!(form.recipe.id, "hermes-c");
        assert_eq!(form.recipe.title, "Hermes C");

        form.recipe.title = "My coding fleet".into();
        form.edit_text(KeyCode::Char('2'));
        assert_eq!(form.recipe.id, "hermes-c2");
        assert_eq!(form.recipe.title, "My coding fleet");
    }

    #[test]
    fn selectable_recipe_toggles_are_additive_and_clear_exclusive_binding() {
        let mut form = ClientForm {
            client: ClientConfig {
                id: "hermes".into(),
                title: "Hermes".into(),
                api_key_env: "INFERNAUT_HERMES_KEY".into(),
                recipe: Some("claude-lab".into()),
                recipes: vec![],
                disabled: false,
            },
            original_id: Some("hermes".into()),
            recipe_ids: vec![
                "claude-lab".into(),
                "hermes-private".into(),
                "hermes-cloud".into(),
            ],
            selectable_recipe_index: 1,
            field: 4,
            error: None,
        };

        form.toggle_selectable_recipe();
        assert_eq!(form.client.recipe, None);
        assert_eq!(form.client.recipes, vec!["hermes-private"]);
        form.cycle_selectable_recipe(1);
        form.toggle_selectable_recipe();
        assert_eq!(form.client.recipes, vec!["hermes-cloud", "hermes-private"]);
        form.toggle_selectable_recipe();
        assert_eq!(form.client.recipes, vec!["hermes-private"]);

        form.cycle_recipe(1);
        assert!(form.client.recipe.is_some());
        assert!(form.client.recipes.is_empty());
    }

    #[test]
    fn recipe_target_picker_filters_and_selects_visible_target() {
        let mut picker = RecipeTargetPicker {
            recipe_index: 0,
            slot_index: 1,
            targets: vec![
                "agamotto/qwen-large".into(),
                "meridian/qwen-small".into(),
                "route/private/auto".into(),
            ],
            query: "meridian".into(),
            selected: 0,
        };
        assert_eq!(picker.filtered_targets(), vec!["meridian/qwen-small"]);
        assert!(matches!(
            handle_recipe_target_picker(
                &mut picker,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            TargetPickerAction::Select(target) if target == "meridian/qwen-small"
        ));
    }
}
