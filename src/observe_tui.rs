use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    io::{self, Stdout},
    net::{IpAddr, SocketAddr},
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line as TextLine, Span},
    widgets::{
        Block, Borders, Cell, Clear, Paragraph, Row, Table,
        canvas::{Canvas, Line, Rectangle},
    },
};
use tokio::sync::mpsc;

use crate::{
    config::Config,
    mission_audio::{MissionAudio, SoundCue},
    telemetry::{TelemetryEvent, TelemetryStage},
};

const RECENT_WINDOW: Duration = Duration::from_secs(90);
const MAX_JOURNEYS: usize = 512;
const DESTINATION_TRAJECTORY_X: f64 = 71.0;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Scope {
    All,
    Route(String),
    Provider(String),
    Client(String),
    Compression,
    Errors,
}

impl Scope {
    fn label(&self) -> String {
        match self {
            Self::All => "all traffic".to_owned(),
            Self::Route(route) => route.clone(),
            Self::Provider(provider) => format!("provider:{provider}"),
            Self::Client(client) => format!("client:{client}"),
            Self::Compression => "compression only".to_owned(),
            Self::Errors => "errors only".to_owned(),
        }
    }

    fn matches(&self, journey: &Journey) -> bool {
        match self {
            Self::All => true,
            Self::Route(route) => journey.route.as_ref() == Some(route),
            Self::Provider(provider) => journey.provider.as_ref() == Some(provider),
            Self::Client(client) => &journey.client == client,
            Self::Compression => journey.compression,
            Self::Errors => matches!(
                journey.stage,
                TelemetryStage::Failed | TelemetryStage::Disconnected
            ),
        }
    }
}

#[derive(Clone)]
struct Journey {
    request_id: String,
    endpoint: String,
    client: String,
    client_source: Option<String>,
    requested_model: String,
    route: Option<String>,
    provider: Option<String>,
    resolved_model: Option<String>,
    realm: Option<String>,
    task: Option<String>,
    operation: Option<String>,
    effort: Option<String>,
    compression: bool,
    streaming: bool,
    request_bytes: u64,
    estimated_tokens: Option<u64>,
    response_bytes: Option<u64>,
    status: Option<u16>,
    time_to_headers_ms: Option<u64>,
    time_to_first_byte_ms: Option<u64>,
    time_to_last_byte_ms: Option<u64>,
    activation_ms: Option<u64>,
    elapsed_ms: Option<u64>,
    detail: Option<String>,
    stage: TelemetryStage,
    accepted_at: Instant,
    updated_at: Instant,
    timestamp_unix_ms: u64,
}

impl Journey {
    fn from_event(event: &TelemetryEvent) -> Self {
        let age = Duration::from_millis(now_unix_ms().saturating_sub(event.timestamp_unix_ms))
            .min(RECENT_WINDOW + Duration::from_secs(1));
        let now = Instant::now();
        Self {
            request_id: event.request_id.clone(),
            endpoint: event.endpoint.clone(),
            client: event.client.label.clone(),
            client_source: event.client.source.clone(),
            requested_model: event.requested_model.clone(),
            route: event.route.clone(),
            provider: event.provider.clone(),
            resolved_model: event.resolved_model.clone(),
            realm: event.realm.clone(),
            task: event.task.clone(),
            operation: event.operation.clone(),
            effort: event.effort.clone(),
            compression: event.compression,
            streaming: event.streaming,
            request_bytes: event.request_bytes,
            estimated_tokens: event.estimated_tokens,
            response_bytes: event.response_bytes,
            status: event.status,
            time_to_headers_ms: event.time_to_headers_ms,
            time_to_first_byte_ms: event.time_to_first_byte_ms,
            time_to_last_byte_ms: event.time_to_last_byte_ms,
            activation_ms: event.activation_ms,
            elapsed_ms: event.elapsed_ms,
            detail: event.detail.clone(),
            stage: event.stage,
            accepted_at: now.checked_sub(age).unwrap_or(now),
            updated_at: now.checked_sub(age).unwrap_or(now),
            timestamp_unix_ms: event.timestamp_unix_ms,
        }
    }

    fn update(&mut self, event: &TelemetryEvent) {
        self.stage = event.stage;
        self.timestamp_unix_ms = event.timestamp_unix_ms;
        let age = Duration::from_millis(now_unix_ms().saturating_sub(event.timestamp_unix_ms))
            .min(RECENT_WINDOW + Duration::from_secs(1));
        let now = Instant::now();
        self.updated_at = now.checked_sub(age).unwrap_or(now);
        self.route = event.route.clone().or_else(|| self.route.clone());
        self.provider = event.provider.clone().or_else(|| self.provider.clone());
        self.resolved_model = event
            .resolved_model
            .clone()
            .or_else(|| self.resolved_model.clone());
        self.realm = event.realm.clone().or_else(|| self.realm.clone());
        self.task = event.task.clone().or_else(|| self.task.clone());
        self.operation = event.operation.clone().or_else(|| self.operation.clone());
        self.effort = event.effort.clone().or_else(|| self.effort.clone());
        self.compression |= event.compression;
        self.estimated_tokens = event.estimated_tokens.or(self.estimated_tokens);
        self.response_bytes = event.response_bytes.or(self.response_bytes);
        self.status = event.status.or(self.status);
        self.time_to_headers_ms = event.time_to_headers_ms.or(self.time_to_headers_ms);
        self.time_to_first_byte_ms = event.time_to_first_byte_ms.or(self.time_to_first_byte_ms);
        self.time_to_last_byte_ms = event.time_to_last_byte_ms.or(self.time_to_last_byte_ms);
        self.activation_ms = event.activation_ms.or(self.activation_ms);
        self.elapsed_ms = event.elapsed_ms.or(self.elapsed_ms);
        self.detail = event.detail.clone().or_else(|| self.detail.clone());
    }

    fn active(&self) -> bool {
        matches!(
            self.stage,
            TelemetryStage::Accepted
                | TelemetryStage::Classified
                | TelemetryStage::Loading
                | TelemetryStage::Forwarding
                | TelemetryStage::Headers
                | TelemetryStage::Streaming
        )
    }
}

enum Inbound {
    Status(String),
    Connected(String),
    Event(Box<TelemetryEvent>),
}

struct ObserverApp {
    scope: Scope,
    journeys: BTreeMap<String, Journey>,
    order: VecDeque<String>,
    seen: HashSet<(String, u64)>,
    connection: String,
    paused: bool,
    reduced_motion: bool,
    sound_enabled: bool,
    sound_available: bool,
    sound_backend: Option<&'static str>,
    picker: Option<ScopePicker>,
    selected: usize,
}

struct ScopePicker {
    options: Vec<Scope>,
    selected: usize,
}

impl ObserverApp {
    fn new(reduced_motion: bool) -> Self {
        Self {
            scope: Scope::All,
            journeys: BTreeMap::new(),
            order: VecDeque::new(),
            seen: HashSet::new(),
            connection: "connecting…".to_owned(),
            paused: false,
            reduced_motion,
            sound_enabled: false,
            sound_available: false,
            sound_backend: None,
            picker: None,
            selected: 0,
        }
    }

    fn ingest(&mut self, event: TelemetryEvent) -> Option<SoundCue> {
        if !self.seen.insert((event.request_id.clone(), event.sequence)) {
            return None;
        }
        let previous = self
            .journeys
            .get(&event.request_id)
            .map(|journey| (journey.stage, journey.compression));
        let cue = sound_cue_for_transition(previous, &event);
        if let Some(journey) = self.journeys.get_mut(&event.request_id) {
            journey.update(&event);
        } else {
            self.order.push_front(event.request_id.clone());
            self.journeys
                .insert(event.request_id.clone(), Journey::from_event(&event));
        }
        while self.order.len() > MAX_JOURNEYS {
            if let Some(id) = self.order.pop_back() {
                self.journeys.remove(&id);
                self.seen.retain(|(request_id, _)| request_id != &id);
            }
        }
        self.selected = self.selected.min(self.filtered().len().saturating_sub(1));
        cue.filter(|_| {
            self.journeys
                .get(&event.request_id)
                .is_some_and(|journey| self.scope.matches(journey))
        })
    }

    fn filtered(&self) -> Vec<&Journey> {
        self.order
            .iter()
            .filter_map(|id| self.journeys.get(id))
            .filter(|journey| self.scope.matches(journey))
            .collect()
    }

    fn visible(&self) -> Vec<&Journey> {
        self.filtered()
            .into_iter()
            .filter(|journey| journey.active() || journey.updated_at.elapsed() <= RECENT_WINDOW)
            .collect()
    }

    fn scope_options(&self) -> Vec<Scope> {
        let mut routes = BTreeSet::new();
        let mut providers = BTreeSet::new();
        let mut clients = BTreeSet::new();
        for journey in self.journeys.values() {
            if let Some(route) = &journey.route {
                routes.insert(route.clone());
            }
            if let Some(provider) = &journey.provider {
                providers.insert(provider.clone());
            }
            clients.insert(journey.client.clone());
        }
        let mut options = vec![Scope::All, Scope::Compression, Scope::Errors];
        options.extend(routes.into_iter().map(Scope::Route));
        options.extend(providers.into_iter().map(Scope::Provider));
        options.extend(clients.into_iter().map(Scope::Client));
        options
    }

    fn open_picker(&mut self) {
        let options = self.scope_options();
        let selected = options
            .iter()
            .position(|scope| scope == &self.scope)
            .unwrap_or(0);
        self.picker = Some(ScopePicker { options, selected });
    }
}

pub async fn run(
    config_path: impl AsRef<Path>,
    url_override: Option<String>,
    reduced_motion: bool,
    sound: bool,
) -> Result<()> {
    let config = Config::load(config_path.as_ref())?;
    let base_url = url_override.unwrap_or_else(|| default_observer_url(config.server.listen));
    let api_key = config
        .server
        .api_key_env
        .as_ref()
        .and_then(|name| std::env::var(name).ok())
        .filter(|value| !value.is_empty());
    let (sender, mut receiver) = mpsc::channel(512);
    let feed = tokio::spawn(event_feed(base_url, api_key, sender));
    let mut audio = MissionAudio::new();
    let mut app = ObserverApp::new(reduced_motion);
    app.sound_available = audio.available();
    app.sound_backend = audio.backend_label();
    app.sound_enabled = sound && app.sound_available;
    let mut terminal = setup_terminal()?;
    if app.sound_enabled {
        audio.play(SoundCue::PowerUp);
    }
    let result = event_loop(&mut terminal, &mut app, &mut receiver, &mut audio).await;
    feed.abort();
    restore_terminal(&mut terminal)?;
    result
}

async fn event_feed(base_url: String, api_key: Option<String>, sender: mpsc::Sender<Inbound>) {
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .user_agent(format!("infernaut-observe/{}", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            let _ = sender
                .send(Inbound::Status(format!("client error: {error}")))
                .await;
            return;
        }
    };
    let url = format!("{}/infernaut/v1/events", base_url.trim_end_matches('/'));
    let mut retry = Duration::from_secs(1);
    loop {
        let _ = sender
            .send(Inbound::Status(format!("connecting to {base_url}")))
            .await;
        let mut request = client.get(&url);
        if let Some(api_key) = &api_key {
            request = request.bearer_auth(api_key);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                retry = Duration::from_secs(1);
                let _ = sender
                    .send(Inbound::Connected(format!("connected to {base_url}")))
                    .await;
                let mut stream = response.bytes_stream();
                let mut buffer = Vec::new();
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(chunk) => {
                            buffer.extend_from_slice(&chunk);
                            for payload in drain_sse_payloads(&mut buffer) {
                                if let Ok(event) =
                                    serde_json::from_slice::<TelemetryEvent>(&payload)
                                    && sender.send(Inbound::Event(Box::new(event))).await.is_err()
                                {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            let _ = sender
                                .send(Inbound::Status(format!("feed interrupted: {error}")))
                                .await;
                            break;
                        }
                    }
                }
            }
            Ok(response) => {
                let status = response.status();
                let message = if status.as_u16() == 401 {
                    "authentication rejected; check server.api_key_env".to_owned()
                } else {
                    format!("event feed returned HTTP {}", status.as_u16())
                };
                let _ = sender.send(Inbound::Status(message)).await;
            }
            Err(error) => {
                let _ = sender
                    .send(Inbound::Status(format!("connection failed: {error}")))
                    .await;
            }
        }
        tokio::time::sleep(retry).await;
        retry = (retry * 2).min(Duration::from_secs(10));
    }
}

fn drain_sse_payloads(buffer: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut payloads = Vec::new();
    while let Some(end) = crate::diagnostics::sse_event_end(buffer) {
        let block = buffer.drain(..end).collect::<Vec<_>>();
        let mut data = Vec::new();
        for line in block.split(|byte| *byte == b'\n') {
            if let Some(value) = line.strip_prefix(b"data: ") {
                data.extend_from_slice(value);
            }
        }
        if !data.is_empty() {
            payloads.push(data);
        }
    }
    payloads
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut ObserverApp,
    receiver: &mut mpsc::Receiver<Inbound>,
    audio: &mut MissionAudio,
) -> Result<()> {
    let mut sound_ready_at = Instant::now() + Duration::from_millis(750);
    loop {
        if !app.paused {
            while let Ok(inbound) = receiver.try_recv() {
                match inbound {
                    Inbound::Status(status) => app.connection = status,
                    Inbound::Connected(status) => {
                        app.connection = status;
                        sound_ready_at = Instant::now() + Duration::from_millis(750);
                    }
                    Inbound::Event(event) => {
                        let cue = app.ingest(*event);
                        if app.sound_enabled
                            && Instant::now() >= sound_ready_at
                            && let Some(cue) = cue
                        {
                            audio.play(cue);
                        }
                    }
                }
            }
        }
        terminal.draw(|frame| draw(frame, app))?;
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if let Some(picker) = app.picker.as_mut() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => app.picker = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    picker.selected = (picker.selected + 1) % picker.options.len().max(1);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    picker.selected = picker
                        .selected
                        .checked_sub(1)
                        .unwrap_or(picker.options.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(scope) = picker.options.get(picker.selected).cloned() {
                        app.scope = scope;
                        app.selected = 0;
                    }
                    app.picker = None;
                }
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Char(' ') => {
                app.paused = !app.paused;
                if !app.paused {
                    sound_ready_at = Instant::now() + Duration::from_millis(750);
                }
            }
            KeyCode::Char('s') | KeyCode::Char('/') => app.open_picker(),
            KeyCode::Char('a') => {
                app.scope = Scope::All;
                app.selected = 0;
            }
            KeyCode::Char('m') => app.reduced_motion = !app.reduced_motion,
            KeyCode::Char('8') if app.sound_available => {
                if app.sound_enabled {
                    audio.play(SoundCue::PowerDown);
                    app.sound_enabled = false;
                } else {
                    app.sound_enabled = true;
                    sound_ready_at = Instant::now() + Duration::from_millis(150);
                    audio.play(SoundCue::PowerUp);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.selected = (app.selected + 1).min(app.filtered().len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.selected = app.selected.saturating_sub(1);
            }
            _ => {}
        }
    }
    Ok(())
}

fn draw(frame: &mut Frame, app: &ObserverApp) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(14),
            Constraint::Length(10),
            Constraint::Length(2),
        ])
        .split(frame.area());
    draw_header(frame, areas[0], app);
    draw_graph(frame, areas[1], app);
    draw_timeline(frame, areas[2], app);
    frame.render_widget(
        Paragraph::new(format!(
            "q quit · s / scope · a all · space pause · j/k inspect · m motion ({}) · 8 audio ({})",
            if app.reduced_motion {
                "reduced"
            } else {
                "animated"
            },
            sound_state(app),
        ))
        .style(Style::default().fg(Color::DarkGray)),
        areas[3],
    );
    if let Some(picker) = &app.picker {
        draw_scope_picker(frame, picker);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &ObserverApp) {
    let visible = app.visible();
    let active = visible.iter().filter(|journey| journey.active()).count();
    let errors = visible
        .iter()
        .filter(|journey| {
            matches!(
                journey.stage,
                TelemetryStage::Failed | TelemetryStage::Disconnected
            )
        })
        .count();
    let state = if app.paused { "PAUSED" } else { "LIVE" };
    let title = TextLine::from(vec![
        Span::styled(
            " INFERNAUT MISSION CONTROL ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  Scope: {}  ·  {active} active  ·  {errors} recent errors  ·  {state}",
            app.scope.label()
        )),
    ]);
    frame.render_widget(
        Paragraph::new(title).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" telemetry link · {} ", app.connection)),
        ),
        area,
    );
}

fn draw_graph(frame: &mut Frame, area: Rect, app: &ObserverApp) {
    let visible = app.visible();
    let mut clients = visible
        .iter()
        .map(|journey| journey.client.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut destinations = visible
        .iter()
        .filter_map(|journey| {
            journey.provider.as_ref().map(|provider| {
                (
                    provider.clone(),
                    journey
                        .resolved_model
                        .as_deref()
                        .map(short_model)
                        .unwrap_or("?")
                        .to_owned(),
                )
            })
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    clients.truncate(8);
    destinations.truncate(8);
    let client_positions = vertical_positions(&clients, 72.0);
    let destination_positions = vertical_positions(&destinations, 72.0);
    let canvas_width = area.width.saturating_sub(2) as usize;
    let client_label_width = (canvas_width * 20 / 100).saturating_sub(2).max(4);
    let provider_label_width = (canvas_width * 15 / 100).saturating_sub(2).max(4);
    let model_label_width = (canvas_width * 22 / 100).saturating_sub(2).max(4);
    let now = Instant::now();
    let reduced_motion = app.reduced_motion;
    let canvas = Canvas::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" live inference trajectories "),
        )
        .x_bounds([0.0, 100.0])
        .y_bounds([0.0, 100.0])
        .paint(move |context| {
            for (x, y) in [
                (29.0, 90.0),
                (36.0, 72.0),
                (64.0, 88.0),
                (68.0, 24.0),
                (31.0, 12.0),
            ] {
                context.print(
                    x,
                    y,
                    Span::styled("·", Style::default().fg(Color::DarkGray)),
                );
            }
            context.print(
                4.0,
                96.0,
                Span::styled(
                    "CLIENT FLEET",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ),
            );
            context.print(
                76.0,
                96.0,
                Span::styled(
                    "PROVIDER SYSTEMS",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ),
            );
            context.draw(&Rectangle {
                x: 42.0,
                y: 39.0,
                width: 16.0,
                height: 22.0,
                color: Color::Cyan,
            });
            context.print(
                45.0,
                53.0,
                Span::styled(
                    "INFERNAUT",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            );
            context.print(
                44.0,
                47.0,
                Span::styled("NAVIGATION CORE", Style::default().fg(Color::White)),
            );
            for (client, y) in &client_positions {
                context.draw(&Rectangle {
                    x: 2.0,
                    y: *y - 4.0,
                    width: 22.0,
                    height: 8.0,
                    color: Color::DarkGray,
                });
                context.print(
                    4.0,
                    *y,
                    Span::styled(
                        truncate_label(client, client_label_width),
                        Style::default().fg(Color::White),
                    ),
                );
            }
            for ((provider, model), y) in &destination_positions {
                let state = destination_state(&visible, provider, model);
                let color = match state {
                    DestinationState::Serving => Color::Green,
                    DestinationState::Prefill => Color::Magenta,
                    DestinationState::Error => Color::Red,
                    DestinationState::Recent => Color::Yellow,
                    DestinationState::Activating => Color::Yellow,
                    DestinationState::Connecting => Color::Cyan,
                };
                context.draw(&Rectangle {
                    x: 74.0,
                    y: *y - 5.0,
                    width: 24.0,
                    height: 10.0,
                    color,
                });
                context.print(
                    76.0,
                    *y + 1.5,
                    Span::styled(
                        truncate_label(provider, provider_label_width),
                        Style::default().fg(color),
                    ),
                );
                context.print(
                    76.0,
                    *y - 2.0,
                    Span::styled(
                        truncate_label(model, model_label_width),
                        Style::default().fg(Color::Gray),
                    ),
                );
                context.print(
                    91.0,
                    *y + 1.5,
                    Span::styled(destination_state_label(state), Style::default().fg(color)),
                );
            }
            for journey in &visible {
                let Some(client_y) = client_positions.get(&journey.client) else {
                    continue;
                };
                context.draw(&Line {
                    x1: 24.0,
                    y1: *client_y,
                    x2: 42.0,
                    y2: 50.0,
                    color: edge_color(journey),
                });
                let destination_key = journey.provider.as_ref().map(|provider| {
                    (
                        provider.clone(),
                        journey
                            .resolved_model
                            .as_deref()
                            .map(short_model)
                            .unwrap_or("?")
                            .to_owned(),
                    )
                });
                if let Some(key) = destination_key
                    && let Some(destination_y) = destination_positions.get(&key)
                {
                    context.draw(&Line {
                        x1: 58.0,
                        y1: 50.0,
                        x2: 74.0,
                        y2: *destination_y,
                        color: edge_color(journey),
                    });
                    let (x, y) =
                        particle_position(journey, *client_y, *destination_y, now, reduced_motion);
                    context.print(
                        x,
                        y,
                        Span::styled(
                            particle_glyph(journey),
                            Style::default().fg(particle_color(journey)),
                        ),
                    );
                } else {
                    let (x, y) =
                        incoming_particle_position(journey, *client_y, now, reduced_motion);
                    context.print(
                        x,
                        y,
                        Span::styled(
                            particle_glyph(journey),
                            Style::default().fg(particle_color(journey)),
                        ),
                    );
                }
            }
            if visible.is_empty() {
                context.print(
                    35.0,
                    20.0,
                    Span::styled(
                        "Awaiting telemetry in this scope…",
                        Style::default().fg(Color::DarkGray),
                    ),
                );
            }
        });
    frame.render_widget(canvas, area);
}

fn draw_timeline(frame: &mut Frame, area: Rect, app: &ObserverApp) {
    let filtered = app.filtered();
    let rows = filtered.iter().take(7).enumerate().map(|(index, journey)| {
        let style = if index == app.selected {
            Style::default().bg(Color::DarkGray).fg(Color::White)
        } else {
            Style::default().fg(stage_color(journey.stage))
        };
        Row::new(vec![
            Cell::from(format_time(journey.timestamp_unix_ms)),
            Cell::from(journey.client.clone()),
            Cell::from(if journey.compression {
                "COMPACT".to_owned()
            } else {
                classification_label(journey)
            }),
            Cell::from(format_bytes(journey.request_bytes)),
            Cell::from(
                journey
                    .estimated_tokens
                    .map(compact_number)
                    .unwrap_or_else(|| "—".to_owned()),
            ),
            Cell::from(
                journey
                    .resolved_model
                    .as_deref()
                    .map(short_model)
                    .unwrap_or("resolving…")
                    .to_owned(),
            ),
            Cell::from(stage_label(journey.stage)),
            Cell::from(
                journey
                    .status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "—".to_owned()),
            ),
        ])
        .style(style)
    });
    let title = filtered
        .get(app.selected)
        .map(|journey| detail_title(journey))
        .unwrap_or_else(|| " recent requests ".to_owned());
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(9),
                Constraint::Length(16),
                Constraint::Length(15),
                Constraint::Length(11),
                Constraint::Length(10),
                Constraint::Min(20),
                Constraint::Length(12),
                Constraint::Length(5),
            ],
        )
        .header(
            Row::new([
                "Time",
                "Client",
                "Detection",
                "Wire in",
                "Budget",
                "Resolved model",
                "Stage",
                "HTTP",
            ])
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn draw_scope_picker(frame: &mut Frame, picker: &ScopePicker) {
    let area = centered_rect(58, 68, frame.area());
    frame.render_widget(Clear, area);
    let visible_rows = area.height.saturating_sub(3) as usize;
    let start = picker
        .selected
        .saturating_sub(visible_rows.saturating_sub(1));
    let rows = picker
        .options
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(index, scope)| {
            Row::new(vec![scope_kind(scope).to_owned(), scope.label()]).style(
                if index == picker.selected {
                    Style::default().bg(Color::DarkGray).fg(Color::Cyan)
                } else {
                    Style::default()
                },
            )
        });
    frame.render_widget(
        Table::new(rows, [Constraint::Length(14), Constraint::Min(24)])
            .header(
                Row::new(["Scope", "Observed value"]).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" read-only traffic scope · Enter select · Esc cancel "),
            ),
        area,
    );
}

#[derive(Clone, Copy)]
enum DestinationState {
    Serving,
    Prefill,
    Recent,
    Error,
    Activating,
    Connecting,
}

fn destination_state(journeys: &[&Journey], provider: &str, model: &str) -> DestinationState {
    let mut state = DestinationState::Recent;
    for journey in journeys.iter().filter(|journey| {
        journey.provider.as_deref() == Some(provider)
            && journey.resolved_model.as_deref().map(short_model) == Some(model)
    }) {
        if journey.stage == TelemetryStage::Failed {
            state = DestinationState::Error;
        } else if journey.stage == TelemetryStage::Streaming
            && journey
                .status
                .is_some_and(|status| (200..400).contains(&status))
        {
            return DestinationState::Serving;
        } else if journey.stage == TelemetryStage::Headers {
            state = DestinationState::Prefill;
        } else if journey.stage == TelemetryStage::Loading {
            state = DestinationState::Activating;
        } else if journey.stage == TelemetryStage::Forwarding && journey.status.is_none() {
            state = DestinationState::Connecting;
        }
    }
    state
}

fn destination_state_label(state: DestinationState) -> &'static str {
    match state {
        DestinationState::Serving => "SERVING",
        DestinationState::Prefill => "PREFILL",
        DestinationState::Recent => "RECENT",
        DestinationState::Error => "ERROR",
        DestinationState::Activating => "LOADING",
        DestinationState::Connecting => "AWAIT",
    }
}

fn particle_position(
    journey: &Journey,
    client_y: f64,
    destination_y: f64,
    now: Instant,
    reduced_motion: bool,
) -> (f64, f64) {
    if reduced_motion {
        return match journey.stage {
            TelemetryStage::Accepted => (37.0, midpoint(client_y, 50.0, 0.5)),
            TelemetryStage::Classified | TelemetryStage::Loading => (50.0, 50.0),
            TelemetryStage::Forwarding | TelemetryStage::Headers | TelemetryStage::Streaming => {
                (DESTINATION_TRAJECTORY_X, destination_y)
            }
            _ => (50.0, 50.0),
        };
    }
    if !journey.active() {
        let age = now.duration_since(journey.updated_at).as_secs_f64();
        if journey.stage == TelemetryStage::Failed {
            return (DESTINATION_TRAJECTORY_X, destination_y);
        }
        if journey.stage == TelemetryStage::Disconnected {
            return (24.0, client_y);
        }
        if age < 0.7 {
            let progress = age / 0.7;
            return (
                midpoint(DESTINATION_TRAJECTORY_X, 50.0, progress),
                midpoint(destination_y, 50.0, progress),
            );
        }
        let progress = ((age - 0.7) / 0.7).min(1.0);
        return (
            midpoint(50.0, 24.0, progress),
            midpoint(50.0, client_y, progress),
        );
    }
    let phase_age = now.duration_since(journey.updated_at).as_secs_f64();
    match journey.stage {
        TelemetryStage::Accepted => {
            let progress = (phase_age / 0.8).min(1.0);
            (
                midpoint(24.0, 50.0, progress),
                midpoint(client_y, 50.0, progress),
            )
        }
        TelemetryStage::Classified | TelemetryStage::Loading => (50.0, 50.0),
        TelemetryStage::Forwarding => {
            let progress = (phase_age / 0.75).min(1.0);
            (
                midpoint(50.0, DESTINATION_TRAJECTORY_X, progress),
                midpoint(50.0, destination_y, progress),
            )
        }
        TelemetryStage::Headers => (DESTINATION_TRAJECTORY_X, destination_y),
        TelemetryStage::Streaming => {
            let progress = (phase_age % 1.4) / 1.4;
            if progress < 0.5 {
                (
                    midpoint(DESTINATION_TRAJECTORY_X, 50.0, progress * 2.0),
                    midpoint(destination_y, 50.0, progress * 2.0),
                )
            } else {
                (
                    midpoint(50.0, 24.0, (progress - 0.5) * 2.0),
                    midpoint(50.0, client_y, (progress - 0.5) * 2.0),
                )
            }
        }
        _ => (50.0, 50.0),
    }
}

fn incoming_particle_position(
    journey: &Journey,
    client_y: f64,
    now: Instant,
    reduced_motion: bool,
) -> (f64, f64) {
    let progress = if reduced_motion {
        0.7
    } else {
        (now.duration_since(journey.accepted_at).as_secs_f64() / 0.8).min(1.0)
    };
    (
        midpoint(24.0, 50.0, progress),
        midpoint(client_y, 50.0, progress),
    )
}

fn particle_glyph(journey: &Journey) -> &'static str {
    if journey.stage == TelemetryStage::Failed {
        "✹"
    } else if journey.stage == TelemetryStage::Disconnected {
        "×"
    } else if journey.compression {
        "◈"
    } else if journey.stage == TelemetryStage::Streaming {
        "✦"
    } else if journey.stage == TelemetryStage::Completed {
        "●"
    } else if journey
        .response_bytes
        .filter(|_| !journey.active())
        .unwrap_or(journey.request_bytes)
        >= 512 * 1024
    {
        "◆"
    } else if journey
        .response_bytes
        .filter(|_| !journey.active())
        .unwrap_or(journey.request_bytes)
        >= 64 * 1024
    {
        "◇"
    } else {
        "›"
    }
}

fn particle_color(journey: &Journey) -> Color {
    if matches!(
        journey.stage,
        TelemetryStage::Failed | TelemetryStage::Disconnected
    ) {
        Color::Red
    } else if journey.compression {
        Color::Magenta
    } else if journey.realm.as_deref() == Some("cloud") {
        Color::Blue
    } else if journey.stage == TelemetryStage::Loading {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn edge_color(journey: &Journey) -> Color {
    if matches!(
        journey.stage,
        TelemetryStage::Failed | TelemetryStage::Disconnected
    ) {
        Color::Red
    } else if journey.active() {
        Color::Cyan
    } else {
        Color::DarkGray
    }
}

fn stage_color(stage: TelemetryStage) -> Color {
    match stage {
        TelemetryStage::Failed | TelemetryStage::Disconnected => Color::Red,
        TelemetryStage::Loading => Color::Yellow,
        TelemetryStage::Completed => Color::Green,
        TelemetryStage::Headers => Color::Magenta,
        TelemetryStage::Streaming => Color::Cyan,
        _ => Color::Gray,
    }
}

fn stage_label(stage: TelemetryStage) -> String {
    format!("{stage:?}").to_ascii_lowercase()
}

fn sound_cue_for_transition(
    previous: Option<(TelemetryStage, bool)>,
    event: &TelemetryEvent,
) -> Option<SoundCue> {
    if event.compression && !previous.is_some_and(|(_, compression)| compression) {
        return Some(SoundCue::Compression);
    }
    if previous.is_some_and(|(stage, _)| stage == event.stage) {
        return None;
    }
    match event.stage {
        TelemetryStage::Accepted => Some(SoundCue::Ingress),
        TelemetryStage::Classified => Some(SoundCue::Routed),
        TelemetryStage::Loading => Some(SoundCue::Loading),
        TelemetryStage::Streaming => Some(SoundCue::Streaming),
        TelemetryStage::Completed => Some(SoundCue::Completed),
        TelemetryStage::Failed => Some(SoundCue::Failed),
        TelemetryStage::Disconnected => Some(SoundCue::Disconnected),
        TelemetryStage::Forwarding | TelemetryStage::Headers => None,
    }
}

fn sound_state(app: &ObserverApp) -> String {
    if !app.sound_available {
        "unavailable".to_owned()
    } else if app.sound_enabled {
        format!("on/{}", app.sound_backend.unwrap_or("system"))
    } else {
        "off".to_owned()
    }
}

fn classification_label(journey: &Journey) -> String {
    match (&journey.task, &journey.effort) {
        (Some(task), Some(effort)) => format!("{task}/{effort}"),
        _ if journey.route.is_some() => "routing".to_owned(),
        _ => "direct".to_owned(),
    }
}

fn detail_title(journey: &Journey) -> String {
    let mut details = vec![
        journey.request_id.clone(),
        format!(
            "{} → {}",
            journey.requested_model,
            journey.resolved_model.as_deref().unwrap_or("resolving")
        ),
        format!(
            "{} {}",
            journey.endpoint,
            if journey.streaming {
                "stream"
            } else {
                "buffered"
            }
        ),
    ];
    if let Some(operation) = &journey.operation {
        details.push(operation.clone());
    }
    if let Some(source) = &journey.client_source {
        details.push(format!("client via {}", source.replace('_', " ")));
    }
    if let Some(tokens) = journey.estimated_tokens {
        details.push(format!("~{} token budget", compact_number(tokens)));
    }
    if let Some(activation) = journey.activation_ms {
        details.push(format!("{activation}ms activation"));
    }
    if let Some(headers) = journey.time_to_headers_ms {
        details.push(format!("{headers}ms headers"));
    }
    if let Some(first_byte) = journey.time_to_first_byte_ms {
        details.push(format!("{first_byte}ms first byte"));
    }
    if let Some(last_byte) = journey.time_to_last_byte_ms
        && matches!(
            journey.stage,
            TelemetryStage::Completed | TelemetryStage::Failed | TelemetryStage::Disconnected
        )
    {
        details.push(format!("{last_byte}ms last byte"));
    }
    if let Some(elapsed) = journey.elapsed_ms {
        details.push(format!("{elapsed}ms total"));
    }
    if let Some(bytes) = journey.response_bytes {
        details.push(format!("{} out", format_bytes(bytes)));
    }
    if let Some(detail) = &journey.detail {
        details.push(detail.clone());
    }
    format!(" {} ", details.join(" · "))
}

fn scope_kind(scope: &Scope) -> &'static str {
    match scope {
        Scope::All => "all",
        Scope::Route(_) => "route",
        Scope::Provider(_) => "provider",
        Scope::Client(_) => "client",
        Scope::Compression => "detection",
        Scope::Errors => "status",
    }
}

fn vertical_positions<T: Ord + Clone>(items: &[T], span: f64) -> BTreeMap<T, f64> {
    let mut positions = BTreeMap::new();
    let count = items.len();
    for (index, item) in items.iter().enumerate() {
        let y = if count <= 1 {
            50.0
        } else {
            let upper = 50.0 + span / 2.0;
            upper - (span * index as f64 / (count - 1) as f64)
        };
        positions.insert(item.clone(), y);
    }
    positions
}

fn truncate_label(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_none() {
        return prefix;
    }
    let mut truncated = prefix
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn short_model(model: &str) -> &str {
    model
        .split_once('/')
        .map(|(_, model)| model)
        .unwrap_or(model)
}

fn midpoint(start: f64, end: f64, progress: f64) -> f64 {
    start + (end - start) * progress.clamp(0.0, 1.0)
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

fn compact_number(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn format_time(timestamp_unix_ms: u64) -> String {
    let seconds = (timestamp_unix_ms / 1000) % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn default_observer_url(listen: SocketAddr) -> String {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_owned(),
        IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".to_owned(),
        IpAddr::V6(ip) => format!("[{ip}]"),
        IpAddr::V4(ip) => ip.to_string(),
    };
    format!("http://{ip}:{}", listen.port())
}

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
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

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::ClientIdentity;
    use ratatui::backend::TestBackend;

    #[test]
    fn parses_fragmented_sse_frames() {
        let mut buffer = b"id: 1\ndata: {\"sequence\":1}\n".to_vec();
        assert!(drain_sse_payloads(&mut buffer).is_empty());
        buffer.extend_from_slice(b"\n");
        let payloads = drain_sse_payloads(&mut buffer);
        assert_eq!(payloads, vec![br#"{"sequence":1}"#.to_vec()]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn unspecified_listeners_connect_locally() {
        assert_eq!(
            default_observer_url("0.0.0.0:4040".parse().unwrap()),
            "http://127.0.0.1:4040"
        );
        assert_eq!(
            default_observer_url("[::]:4040".parse().unwrap()),
            "http://[::1]:4040"
        );
    }

    #[test]
    fn rendered_graph_contains_only_observed_path_nodes() {
        let mut app = ObserverApp::new(false);
        let _ = app.ingest(TelemetryEvent {
            sequence: 1,
            timestamp_unix_ms: now_unix_ms(),
            request_id: "inf_test".into(),
            stage: TelemetryStage::Streaming,
            endpoint: "/v1/messages".into(),
            client: ClientIdentity {
                label: "Claude Code".into(),
                source: Some("client_key".into()),
                peer: None,
                user_agent: None,
            },
            requested_model: "route:private:auto".into(),
            route: Some("route:private:auto".into()),
            provider: Some("agamotto".into()),
            resolved_model: Some("agamotto/nova-1.0-35b".into()),
            realm: Some("local".into()),
            task: Some("chat".into()),
            operation: Some("converse".into()),
            effort: Some("standard".into()),
            compression: true,
            streaming: true,
            request_bytes: 699_723,
            estimated_tokens: Some(232_000),
            response_bytes: None,
            status: Some(200),
            time_to_headers_ms: Some(125),
            time_to_first_byte_ms: Some(180),
            time_to_last_byte_ms: Some(180),
            activation_ms: None,
            elapsed_ms: None,
            detail: None,
        });
        let backend = TestBackend::new(150, 42);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("INFERNAUT"));
        assert!(rendered.contains("MISSION CONTROL"));
        assert!(rendered.contains("CLIENT FLEET"));
        assert!(rendered.contains("PROVIDER SYSTEMS"));
        assert!(rendered.contains("Claude Code"));
        assert!(rendered.contains("agamotto"));
        assert!(rendered.contains("nova-1.0-35b"));
        assert!(rendered.contains("COMPACT"));
        assert!(rendered.contains("Wire in"));
        assert!(rendered.contains("Budget"));
        assert!(!rendered.contains("unused-model"));
        assert!(
            app.scope_options()
                .contains(&Scope::Route("route:private:auto".into()))
        );
    }

    #[test]
    fn mission_symbols_preserve_precise_request_state() {
        let event = TelemetryEvent {
            sequence: 1,
            timestamp_unix_ms: now_unix_ms(),
            request_id: "inf_symbol".into(),
            stage: TelemetryStage::Forwarding,
            endpoint: "/v1/messages".into(),
            client: ClientIdentity {
                label: "Claude Code".into(),
                source: None,
                peer: None,
                user_agent: None,
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
            request_bytes: 1024,
            estimated_tokens: Some(1024),
            response_bytes: None,
            status: None,
            time_to_headers_ms: None,
            time_to_first_byte_ms: None,
            time_to_last_byte_ms: None,
            activation_ms: None,
            elapsed_ms: None,
            detail: None,
        };
        let mut journey = Journey::from_event(&event);
        assert_eq!(particle_glyph(&journey), "›");
        journey.request_bytes = 700 * 1024;
        assert_eq!(particle_glyph(&journey), "◆");
        journey.compression = true;
        assert_eq!(particle_glyph(&journey), "◈");
        journey.compression = false;
        journey.stage = TelemetryStage::Streaming;
        assert_eq!(particle_glyph(&journey), "✦");
        journey.stage = TelemetryStage::Completed;
        assert_eq!(particle_glyph(&journey), "●");
        journey.stage = TelemetryStage::Failed;
        assert_eq!(particle_glyph(&journey), "✹");
        assert_eq!(
            particle_position(&journey, 40.0, 60.0, Instant::now(), false),
            (DESTINATION_TRAJECTORY_X, 60.0)
        );
        journey.stage = TelemetryStage::Disconnected;
        assert_eq!(particle_glyph(&journey), "×");
        assert_eq!(
            particle_position(&journey, 40.0, 60.0, Instant::now(), false),
            (24.0, 40.0)
        );
    }

    #[test]
    fn reduced_motion_can_be_selected_at_startup() {
        assert!(ObserverApp::new(true).reduced_motion);
        assert!(!ObserverApp::new(false).reduced_motion);
    }

    #[test]
    fn lifecycle_sound_cues_are_distinct_and_compression_wins_classification() {
        let mut event = TelemetryEvent {
            sequence: 1,
            timestamp_unix_ms: now_unix_ms(),
            request_id: "inf_audio".into(),
            stage: TelemetryStage::Accepted,
            endpoint: "/v1/messages".into(),
            client: ClientIdentity {
                label: "Claude Code".into(),
                source: None,
                peer: None,
                user_agent: None,
            },
            requested_model: "route:private:auto".into(),
            route: None,
            provider: None,
            resolved_model: None,
            realm: None,
            task: None,
            operation: None,
            effort: None,
            compression: false,
            streaming: true,
            request_bytes: 1024,
            estimated_tokens: None,
            response_bytes: None,
            status: None,
            time_to_headers_ms: None,
            time_to_first_byte_ms: None,
            time_to_last_byte_ms: None,
            activation_ms: None,
            elapsed_ms: None,
            detail: None,
        };
        assert_eq!(
            sound_cue_for_transition(None, &event),
            Some(SoundCue::Ingress)
        );
        event.stage = TelemetryStage::Classified;
        assert_eq!(
            sound_cue_for_transition(Some((TelemetryStage::Accepted, false)), &event),
            Some(SoundCue::Routed)
        );
        event.compression = true;
        assert_eq!(
            sound_cue_for_transition(Some((TelemetryStage::Accepted, false)), &event),
            Some(SoundCue::Compression)
        );
        event.stage = TelemetryStage::Failed;
        assert_eq!(
            sound_cue_for_transition(Some((TelemetryStage::Streaming, true)), &event),
            Some(SoundCue::Failed)
        );
        assert_eq!(
            sound_cue_for_transition(Some((TelemetryStage::Failed, true)), &event),
            None
        );
    }

    #[test]
    fn graph_labels_are_bounded_and_nodes_leave_a_heading_band() {
        assert_eq!(truncate_label("agamotto", 14), "agamotto");
        assert_eq!(
            truncate_label("provider-name-that-is-too-long", 14),
            "provider-name…"
        );

        let positions = vertical_positions(&["a", "b", "c"], 72.0);
        assert_eq!(positions["a"], 86.0);
        assert_eq!(positions["b"], 50.0);
        assert_eq!(positions["c"], 14.0);
    }
}
