use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use anyhow::{Context, Result, bail};
use flate2::{Compression, write::GzEncoder};
use infernaut::plugin_protocol::{PLUGIN_EVENT_SCHEMA_V1, PluginEvent, PluginEventKind};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

fn default_capture() -> Capture {
    Capture::Match
}

fn default_retain_days() -> u64 {
    14
}

fn default_max_total_mb() -> u64 {
    500
}

fn default_max_file_mb() -> u64 {
    25
}

fn default_max_payload_kb() -> usize {
    512
}

fn default_context_bytes() -> usize {
    80
}

fn default_max_findings() -> usize {
    10
}

fn default_true() -> bool {
    true
}

const MAX_INFLIGHT_REQUESTS: usize = 4096;
const MAX_INLINE_MATCH_BYTES: usize = 4096;
const MAX_INLINE_CONTEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnifferConfig {
    #[serde(default)]
    plugin: SnifferIdentity,
    storage: StorageConfig,
    #[serde(default)]
    rules: Vec<RuleConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnifferIdentity {
    #[serde(default = "default_plugin_id")]
    id: String,
    #[serde(default = "default_mode")]
    mode: String,
}

impl Default for SnifferIdentity {
    fn default() -> Self {
        Self {
            id: default_plugin_id(),
            mode: default_mode(),
        }
    }
}

fn default_plugin_id() -> String {
    "sniffer".into()
}

fn default_mode() -> String {
    "report".into()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageConfig {
    path: PathBuf,
    #[serde(default = "default_capture")]
    capture: Capture,
    #[serde(default = "default_retain_days")]
    retain_days: u64,
    #[serde(default = "default_max_total_mb")]
    max_total_mb: u64,
    #[serde(default = "default_max_file_mb")]
    max_file_mb: u64,
    #[serde(default = "default_max_payload_kb")]
    max_payload_kb: usize,
    #[serde(default = "default_context_bytes")]
    context_before: usize,
    #[serde(default = "default_context_bytes")]
    context_after: usize,
    #[serde(default = "default_true")]
    compress: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Capture {
    None,
    #[default]
    Match,
    Message,
    Payload,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Direction {
    Request,
    Response,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleConfig {
    id: String,
    #[serde(default)]
    description: String,
    #[serde(default = "default_severity")]
    severity: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    patterns: Vec<String>,
    pattern: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    directions: Vec<Direction>,
    #[serde(default)]
    realms: Vec<String>,
    capture: Option<Capture>,
    #[serde(default = "default_max_findings")]
    max_findings_per_payload: usize,
}

fn default_severity() -> String {
    "medium".into()
}

enum Matcher {
    Keywords {
        matcher: AhoCorasick,
        patterns: Vec<String>,
    },
    Regex(Regex),
}

struct CompiledRule {
    config: RuleConfig,
    matcher: Matcher,
}

impl CompiledRule {
    fn compile(config: RuleConfig) -> Result<Self> {
        if config.id.trim().is_empty() {
            bail!("rule id cannot be empty");
        }
        if config.max_findings_per_payload == 0 {
            bail!(
                "rule '{}' max_findings_per_payload must be positive",
                config.id
            );
        }
        let matcher = match config.kind.as_str() {
            "keyword" => {
                if config.patterns.is_empty() {
                    bail!("keyword rule '{}' requires patterns", config.id);
                }
                if config.patterns.len() > 10_000
                    || config
                        .patterns
                        .iter()
                        .fold(0usize, |total, pattern| total.saturating_add(pattern.len()))
                        > 1024 * 1024
                    || config.patterns.iter().any(String::is_empty)
                {
                    bail!(
                        "keyword rule '{}' must use non-empty patterns totaling at most 1 MiB and 10,000 entries",
                        config.id
                    );
                }
                let matcher = AhoCorasickBuilder::new()
                    .ascii_case_insensitive(config.case_insensitive)
                    .build(&config.patterns)
                    .with_context(|| format!("invalid keyword rule '{}'", config.id))?;
                Matcher::Keywords {
                    matcher,
                    patterns: config.patterns.clone(),
                }
            }
            "regex" => {
                let pattern = config
                    .pattern
                    .as_deref()
                    .context("regex rule requires pattern")?;
                if pattern.is_empty() || pattern.len() > 256 * 1024 {
                    bail!(
                        "regex rule '{}' must have a non-empty pattern no larger than 256 KiB",
                        config.id
                    );
                }
                Matcher::Regex(
                    RegexBuilder::new(pattern)
                        .case_insensitive(config.case_insensitive)
                        .size_limit(2 * 1024 * 1024)
                        .build()
                        .with_context(|| format!("invalid regex rule '{}'", config.id))?,
                )
            }
            other => bail!("rule '{}' has unsupported type '{other}'", config.id),
        };
        Ok(Self { config, matcher })
    }

    fn applies(&self, direction: Direction, realm: &str) -> bool {
        (self.config.directions.is_empty() || self.config.directions.contains(&direction))
            && (self.config.realms.is_empty()
                || self
                    .config
                    .realms
                    .iter()
                    .any(|candidate| candidate == realm))
    }

    fn matches(&self, content: &str) -> Vec<FoundMatch> {
        match &self.matcher {
            Matcher::Keywords { matcher, patterns } => matcher
                .find_iter(content)
                .take(self.config.max_findings_per_payload)
                .map(|matched| FoundMatch {
                    start: matched.start(),
                    end: matched.end(),
                    pattern: patterns[matched.pattern()].clone(),
                })
                .collect(),
            Matcher::Regex(regex) => regex
                .find_iter(content)
                .take(self.config.max_findings_per_payload)
                .map(|matched| FoundMatch {
                    start: matched.start(),
                    end: matched.end(),
                    pattern: regex.as_str().to_owned(),
                })
                .collect(),
        }
    }
}

struct FoundMatch {
    start: usize,
    end: usize,
    pattern: String,
}

#[derive(Default)]
struct RequestState {
    context: Option<EventContext>,
    response: String,
    response_truncated: bool,
    last_event_ms: u64,
}

#[derive(Clone)]
struct EventContext {
    request_id: String,
    timestamp_unix_ms: u64,
    endpoint: String,
    provider: String,
    model: String,
    realm: String,
}

impl From<&PluginEvent> for EventContext {
    fn from(event: &PluginEvent) -> Self {
        Self {
            request_id: event.request_id.clone(),
            timestamp_unix_ms: event.timestamp_unix_ms,
            endpoint: event.endpoint.clone(),
            provider: event.provider.clone(),
            model: event.model.clone(),
            realm: event.realm.clone(),
        }
    }
}

#[derive(Serialize)]
struct Finding<'a> {
    schema: &'static str,
    timestamp_unix_ms: u64,
    request_id: &'a str,
    provider: &'a str,
    model: &'a str,
    realm: &'a str,
    endpoint: &'a str,
    direction: Direction,
    rule: FindingRule<'a>,
    matched: FindingMatch,
    content_sha256: String,
    evidence: Evidence,
}

#[derive(Serialize)]
struct FindingRule<'a> {
    id: &'a str,
    description: &'a str,
    severity: &'a str,
    action: &'static str,
}

#[derive(Serialize)]
struct FindingMatch {
    offset: usize,
    length: usize,
    pattern: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

#[derive(Default, Serialize)]
struct Evidence {
    capture: Capture,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<usize>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    omitted_reason: Option<String>,
}

#[derive(Default)]
struct Stats {
    events_received: u64,
    payloads_inspected: u64,
    findings_written: u64,
    evidence_bytes: u64,
    write_failures: u64,
    truncated_payloads: u64,
    incomplete_evicted: u64,
}

struct FindingStore {
    config: StorageConfig,
    directory: PathBuf,
    findings_path: PathBuf,
    findings: BufWriter<File>,
    findings_bytes: u64,
    stats: Stats,
}

impl FindingStore {
    fn open(mut config: StorageConfig) -> Result<Self> {
        config.path = expand_home(&config.path);
        if config.max_file_mb == 0 || config.max_total_mb == 0 || config.max_payload_kb == 0 {
            bail!("storage size limits must be greater than zero");
        }
        ensure_private_directory(&config.path)?;
        cleanup_retention(&config.path, config.retain_days)?;
        enforce_storage_limit(&config.path, config.max_total_mb * 1024 * 1024, None)?;
        let (findings_path, findings, findings_bytes) = open_findings_file(&config.path)?;
        Ok(Self {
            directory: config.path.clone(),
            config,
            findings_path,
            findings,
            findings_bytes,
            stats: Stats::default(),
        })
    }

    fn rotate_if_needed(&mut self, incoming: usize) -> Result<()> {
        let limit = self.config.max_file_mb * 1024 * 1024;
        if self.findings_bytes.saturating_add(incoming as u64) <= limit {
            return Ok(());
        }
        self.findings.flush()?;
        let (path, writer, bytes) = open_findings_file(&self.directory)?;
        self.findings_path = path;
        self.findings = writer;
        self.findings_bytes = bytes;
        cleanup_retention(&self.directory, self.config.retain_days)?;
        enforce_storage_limit(
            &self.directory,
            self.config.max_total_mb * 1024 * 1024,
            Some(&self.findings_path),
        )?;
        Ok(())
    }

    fn write_finding(&mut self, finding: &Finding<'_>) -> Result<()> {
        let mut encoded = serde_json::to_vec(finding)?;
        encoded.push(b'\n');
        self.rotate_if_needed(encoded.len())?;
        self.findings.write_all(&encoded)?;
        self.findings.flush()?;
        self.findings_bytes += encoded.len() as u64;
        self.stats.findings_written += 1;
        enforce_storage_limit(
            &self.directory,
            self.config.max_total_mb * 1024 * 1024,
            Some(&self.findings_path),
        )?;
        Ok(())
    }

    fn evidence(&mut self, capture: Capture, content: &str, matched: &FoundMatch) -> Evidence {
        let max = self.config.max_payload_kb * 1024;
        match capture {
            Capture::None => Evidence {
                capture,
                ..Evidence::default()
            },
            Capture::Match => {
                let inline_limit = max.min(MAX_INLINE_CONTEXT_BYTES);
                let start = floor_char_boundary(
                    content,
                    matched
                        .start
                        .saturating_sub(self.config.context_before.min(inline_limit / 2)),
                );
                let end = ceil_char_boundary(
                    content,
                    matched
                        .end
                        .saturating_add(self.config.context_after)
                        .min(content.len()),
                );
                let (content, truncated) = bounded_text(content, start, end, inline_limit);
                Evidence {
                    capture,
                    content: Some(content),
                    truncated,
                    ..Evidence::default()
                }
            }
            Capture::Message | Capture::Payload => {
                let captured = &content.as_bytes()[..content.len().min(max)];
                let truncated = captured.len() < content.len();
                if truncated {
                    self.stats.truncated_payloads += 1;
                }
                if directory_size(&self.directory)
                    .unwrap_or(0)
                    .saturating_add(captured.len() as u64)
                    > self.config.max_total_mb * 1024 * 1024
                {
                    return Evidence {
                        capture,
                        truncated,
                        omitted_reason: Some("storage_limit_reached".into()),
                        ..Evidence::default()
                    };
                }
                match self.write_blob(captured) {
                    Ok((path, hash, bytes)) => Evidence {
                        capture,
                        path: Some(path),
                        sha256: Some(hash),
                        bytes: Some(bytes),
                        truncated,
                        ..Evidence::default()
                    },
                    Err(error) => {
                        self.stats.write_failures += 1;
                        Evidence {
                            capture,
                            truncated,
                            omitted_reason: Some(format!("write_failed: {error}")),
                            ..Evidence::default()
                        }
                    }
                }
            }
        }
    }

    fn write_blob(&mut self, content: &[u8]) -> Result<(String, String, usize)> {
        let hash = sha256_hex(content);
        let evidence_dir = self.directory.join("evidence");
        ensure_private_directory(&evidence_dir)?;
        let filename = if self.config.compress {
            format!("{hash}.blob.gz")
        } else {
            format!("{hash}.blob")
        };
        let path = evidence_dir.join(&filename);
        if !path.exists() {
            let file = private_create_new(&path)?;
            if self.config.compress {
                let mut encoder = GzEncoder::new(file, Compression::default());
                encoder.write_all(content)?;
                encoder.finish()?;
            } else {
                let mut file = file;
                file.write_all(content)?;
            }
            self.stats.evidence_bytes += content.len() as u64;
        } else if let Ok(file) = OpenOptions::new().write(true).open(&path) {
            let _ = file.set_modified(SystemTime::now());
        }
        Ok((format!("evidence/{filename}"), hash, content.len()))
    }
}

struct Sniffer {
    id: String,
    rules: Vec<CompiledRule>,
    store: FindingStore,
    requests: HashMap<String, RequestState>,
}

impl Sniffer {
    fn validate(config: &SnifferConfig) -> Result<()> {
        if config.plugin.id.trim().is_empty() {
            bail!("plugin id cannot be empty");
        }
        if config.plugin.mode != "report" {
            bail!("the reference sniffer currently supports only mode = 'report'");
        }
        if config.rules.is_empty() {
            bail!("at least one sniffer rule is required");
        }
        if config.storage.max_file_mb == 0
            || config.storage.max_total_mb == 0
            || config.storage.max_payload_kb == 0
        {
            bail!("storage size limits must be greater than zero");
        }
        if config.storage.path.as_os_str().is_empty() {
            bail!("storage.path cannot be empty");
        }
        if config.storage.max_file_mb > config.storage.max_total_mb {
            bail!("storage.max_file_mb cannot exceed storage.max_total_mb");
        }
        if config.storage.max_total_mb > u64::MAX / (1024 * 1024)
            || config.storage.max_file_mb > u64::MAX / (1024 * 1024)
            || config.storage.max_payload_kb > usize::MAX / 1024
        {
            bail!("storage size limits are too large");
        }
        if config.storage.max_payload_kb as u64 > config.storage.max_file_mb * 1024 {
            bail!("storage.max_payload_kb cannot exceed storage.max_file_mb in KiB");
        }
        let mut rule_ids = HashSet::new();
        for rule in &config.rules {
            if !rule_ids.insert(&rule.id) {
                bail!("duplicate rule id '{}'", rule.id);
            }
            if rule.max_findings_per_payload > 1000 {
                bail!(
                    "rule '{}' max_findings_per_payload cannot exceed 1000",
                    rule.id
                );
            }
            CompiledRule::compile(rule.clone())?;
        }
        Ok(())
    }

    fn new(config: SnifferConfig) -> Result<Self> {
        Self::validate(&config)?;
        if config.storage.capture == Capture::Payload
            || config
                .rules
                .iter()
                .any(|rule| rule.capture == Some(Capture::Payload))
        {
            eprintln!(
                "infernaut-sniffer warning: full payload capture is enabled; evidence may contain highly sensitive data"
            );
        }
        let rules = config
            .rules
            .into_iter()
            .map(CompiledRule::compile)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            id: config.plugin.id,
            rules,
            store: FindingStore::open(config.storage)?,
            requests: HashMap::new(),
        })
    }

    fn handle(&mut self, event: PluginEvent) {
        self.store.stats.events_received += 1;
        if event.schema != PLUGIN_EVENT_SCHEMA_V1 {
            eprintln!("{}: ignored unknown event schema {}", self.id, event.schema);
            return;
        }
        match event.event {
            PluginEventKind::RequestContent => {
                let context = EventContext::from(&event);
                if let Some(content) = event.content.as_deref() {
                    self.inspect(&context, Direction::Request, content, false);
                }
                self.reserve_request_state(&event.request_id);
                let state = self.requests.entry(event.request_id).or_default();
                state.context = Some(context);
                state.last_event_ms = event.timestamp_unix_ms;
            }
            PluginEventKind::ResponseDelta => {
                let limit = self.store.config.max_payload_kb * 1024;
                self.reserve_request_state(&event.request_id);
                let state = self.requests.entry(event.request_id.clone()).or_default();
                state.last_event_ms = event.timestamp_unix_ms;
                state
                    .context
                    .get_or_insert_with(|| EventContext::from(&event));
                if let Some(content) = event.content {
                    let remaining = limit.saturating_sub(state.response.len());
                    if content.len() > remaining {
                        state.response_truncated = true;
                    }
                    if remaining > 0 {
                        let end = floor_char_boundary(&content, content.len().min(remaining));
                        state.response.push_str(&content[..end]);
                    }
                }
            }
            PluginEventKind::ResponseCompleted => {
                if let Some(state) = self.requests.remove(&event.request_id) {
                    let context = state.context.unwrap_or_else(|| EventContext::from(&event));
                    self.inspect(
                        &context,
                        Direction::Response,
                        &state.response,
                        state.response_truncated,
                    );
                }
            }
            PluginEventKind::ProviderError => {
                let context = EventContext::from(&event);
                if let Some(state) = self.requests.remove(&event.request_id)
                    && !state.response.is_empty()
                {
                    self.inspect(
                        state.context.as_ref().unwrap_or(&context),
                        Direction::Response,
                        &state.response,
                        true,
                    );
                }
                if let Some(content) = event.content.as_deref() {
                    self.inspect(&context, Direction::Response, content, false);
                }
            }
            PluginEventKind::DiagnosticMatched => {
                let context = EventContext::from(&event);
                if let Some(content) = event.content.as_deref() {
                    self.inspect(&context, Direction::Response, content, false);
                }
            }
        }
    }

    fn reserve_request_state(&mut self, request_id: &str) {
        if self.requests.contains_key(request_id) || self.requests.len() < MAX_INFLIGHT_REQUESTS {
            return;
        }
        if let Some(oldest) = self
            .requests
            .iter()
            .min_by_key(|(_, state)| state.last_event_ms)
            .map(|(request_id, _)| request_id.clone())
        {
            self.requests.remove(&oldest);
            self.store.stats.incomplete_evicted += 1;
        }
    }

    fn inspect(
        &mut self,
        context: &EventContext,
        direction: Direction,
        content: &str,
        source_truncated: bool,
    ) {
        if content.is_empty() {
            return;
        }
        self.store.stats.payloads_inspected += 1;
        let content_hash = sha256_hex(content.as_bytes());
        for rule in &self.rules {
            if !rule.applies(direction, &context.realm) {
                continue;
            }
            for matched in rule.matches(content) {
                let capture = rule.config.capture.unwrap_or(self.store.config.capture);
                let mut evidence = self.store.evidence(capture, content, &matched);
                let (matched_content, matched_truncated) = if capture == Capture::None {
                    (None, false)
                } else {
                    let (value, truncated) =
                        bounded_text(content, matched.start, matched.end, MAX_INLINE_MATCH_BYTES);
                    (Some(value), truncated)
                };
                evidence.truncated |= source_truncated || matched_truncated;
                let finding = Finding {
                    schema: "infernaut.finding.v1",
                    timestamp_unix_ms: context.timestamp_unix_ms,
                    request_id: &context.request_id,
                    provider: &context.provider,
                    model: &context.model,
                    realm: &context.realm,
                    endpoint: &context.endpoint,
                    direction,
                    rule: FindingRule {
                        id: &rule.config.id,
                        description: &rule.config.description,
                        severity: &rule.config.severity,
                        action: "report",
                    },
                    matched: FindingMatch {
                        offset: matched.start,
                        length: matched.end - matched.start,
                        pattern: matched.pattern,
                        content: matched_content,
                    },
                    content_sha256: content_hash.clone(),
                    evidence,
                };
                if let Err(error) = self.store.write_finding(&finding) {
                    self.store.stats.write_failures += 1;
                    eprintln!("{}: failed to write finding: {error}", self.id);
                }
            }
        }
    }
}

fn main() -> Result<()> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let (validate_only, path) = match arguments.as_slice() {
        [path] => (false, PathBuf::from(path)),
        [flag, path] if flag == "--validate" => (true, PathBuf::from(path)),
        _ => bail!("usage: infernaut-sniffer [--validate] CONFIG"),
    };
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read sniffer config at {}", path.display()))?;
    let config: SnifferConfig = toml::from_str(&raw)
        .with_context(|| format!("failed to parse sniffer config at {}", path.display()))?;
    if validate_only {
        Sniffer::validate(&config)?;
        println!("sniffer configuration is valid");
        return Ok(());
    }
    let mut sniffer = Sniffer::new(config)?;
    eprintln!("{}: report-only observer ready", sniffer.id);
    for line in BufReader::new(io::stdin().lock()).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<PluginEvent>(&line) {
            Ok(event) => sniffer.handle(event),
            Err(error) => eprintln!("{}: invalid plugin event: {error}", sniffer.id),
        }
    }
    sniffer.store.findings.flush()?;
    eprintln!(
        "{}: stopped events={} inspected={} findings={} evidence_bytes={} write_failures={} truncated={} incomplete_evicted={}",
        sniffer.id,
        sniffer.store.stats.events_received,
        sniffer.store.stats.payloads_inspected,
        sniffer.store.stats.findings_written,
        sniffer.store.stats.evidence_bytes,
        sniffer.store.stats.write_failures,
        sniffer.store.stats.truncated_payloads,
        sniffer.store.stats.incomplete_evicted,
    );
    Ok(())
}

fn sha256_hex(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

fn expand_home(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return std::env::var_os("HOME").map_or_else(|| path.to_owned(), PathBuf::from);
    }
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_owned()
}

fn now_epoch_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn open_findings_file(directory: &Path) -> Result<(PathBuf, BufWriter<File>, u64)> {
    let timestamp = now_epoch_nanos();
    for suffix in 0..1000_u16 {
        let path = directory.join(format!("findings-{timestamp}-{suffix}.jsonl"));
        match private_create_new(&path) {
            Ok(file) => return Ok((path, BufWriter::new(file), 0)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("unable to allocate a unique findings file name")
}

fn cleanup_retention(directory: &Path, retain_days: u64) -> Result<()> {
    if retain_days == 0 {
        return Ok(());
    }
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(
            retain_days.saturating_mul(24 * 60 * 60),
        ))
        .unwrap_or(UNIX_EPOCH);
    for path in managed_files(directory)? {
        let metadata = fs::metadata(&path)?;
        if metadata.modified().unwrap_or(SystemTime::now()) < cutoff {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn enforce_storage_limit(directory: &Path, limit: u64, exclude: Option<&Path>) -> Result<()> {
    let mut files = managed_files(directory)?
        .into_iter()
        .filter_map(|path| {
            let metadata = fs::metadata(&path).ok()?;
            Some((
                metadata.modified().unwrap_or(UNIX_EPOCH),
                metadata.len(),
                path,
            ))
        })
        .collect::<Vec<_>>();
    let mut total = files.iter().map(|(_, bytes, _)| bytes).sum::<u64>();
    if total <= limit {
        return Ok(());
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, bytes, path) in files {
        if exclude.is_some_and(|excluded| excluded == path) {
            continue;
        }
        fs::remove_file(path)?;
        total = total.saturating_sub(bytes);
        if total <= limit {
            break;
        }
    }
    Ok(())
}

fn managed_files(directory: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && entry.file_name() == "evidence" {
            for evidence in fs::read_dir(path)? {
                let evidence = evidence?;
                if evidence.path().is_file() {
                    files.push(evidence.path());
                }
            }
        } else if path.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("findings-") && name.ends_with(".jsonl"))
        {
            files.push(path);
        }
    }
    Ok(files)
}

fn directory_size(directory: &Path) -> io::Result<u64> {
    let mut total = 0;
    let mut pending = vec![directory.to_owned()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                total += metadata.len();
            }
        }
    }
    Ok(total)
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn bounded_text(text: &str, start: usize, end: usize, limit: usize) -> (String, bool) {
    let start = floor_char_boundary(text, start);
    let end = ceil_char_boundary(text, end.max(start));
    if end.saturating_sub(start) <= limit {
        return (text[start..end].to_owned(), false);
    }
    let bounded_end = floor_char_boundary(text, start.saturating_add(limit).min(end));
    (text[start..bounded_end].to_owned(), true)
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn ensure_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)
}

#[cfg(unix)]
fn private_create_new(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn private_create_new(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    #[test]
    fn keyword_matcher_is_case_insensitive_when_configured() {
        let rule = CompiledRule::compile(RuleConfig {
            id: "private-key".into(),
            description: String::new(),
            severity: "critical".into(),
            kind: "keyword".into(),
            patterns: vec!["BEGIN PRIVATE KEY".into()],
            pattern: None,
            case_insensitive: true,
            directions: vec![],
            realms: vec![],
            capture: None,
            max_findings_per_payload: 10,
        })
        .unwrap();
        let matches = rule.matches("begin private key");
        assert_eq!(matches.len(), 1);
        assert_eq!((matches[0].start, matches[0].end), (0, 17));
    }

    #[test]
    fn regex_rule_can_be_realm_and_direction_scoped() {
        let rule = CompiledRule::compile(RuleConfig {
            id: "internal".into(),
            description: String::new(),
            severity: "high".into(),
            kind: "regex".into(),
            patterns: vec![],
            pattern: Some(r"[a-z]+\.internal".into()),
            case_insensitive: false,
            directions: vec![Direction::Request],
            realms: vec!["cloud".into()],
            capture: None,
            max_findings_per_payload: 10,
        })
        .unwrap();
        assert!(rule.applies(Direction::Request, "cloud"));
        assert!(!rule.applies(Direction::Response, "cloud"));
        assert!(!rule.applies(Direction::Request, "local"));
        assert_eq!(rule.matches("host.internal").len(), 1);
    }

    #[test]
    fn payload_capture_writes_a_finding_and_content_addressed_evidence() {
        let directory =
            std::env::temp_dir().join(format!("infernaut-sniffer-test-{}", now_epoch_nanos()));
        let config = SnifferConfig {
            plugin: SnifferIdentity::default(),
            storage: StorageConfig {
                path: directory.clone(),
                capture: Capture::Match,
                retain_days: 1,
                max_total_mb: 10,
                max_file_mb: 1,
                max_payload_kb: 64,
                context_before: 8,
                context_after: 8,
                compress: true,
            },
            rules: vec![RuleConfig {
                id: "private-key".into(),
                description: "test signature".into(),
                severity: "critical".into(),
                kind: "keyword".into(),
                patterns: vec!["BEGIN PRIVATE KEY".into()],
                pattern: None,
                case_insensitive: false,
                directions: vec![Direction::Request],
                realms: vec!["cloud".into()],
                capture: Some(Capture::Payload),
                max_findings_per_payload: 1,
            }],
        };
        let mut sniffer = Sniffer::new(config).unwrap();
        let content = r#"{"messages":[{"content":"BEGIN PRIVATE KEY secret"}]}"#;
        sniffer.handle(PluginEvent {
            schema: PLUGIN_EVENT_SCHEMA_V1.into(),
            event: PluginEventKind::RequestContent,
            request_id: "inf_test".into(),
            sequence: 1,
            timestamp_unix_ms: 42,
            endpoint: "/v1/chat/completions".into(),
            provider: "cloud".into(),
            model: "cloud/model".into(),
            realm: "cloud".into(),
            content_type: Some("application/json".into()),
            content: Some(content.into()),
            diagnostic_code: None,
        });
        sniffer.store.findings.flush().unwrap();

        let finding_path = managed_files(&directory)
            .unwrap()
            .into_iter()
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .unwrap();
        let finding: serde_json::Value =
            serde_json::from_str(fs::read_to_string(finding_path).unwrap().trim()).unwrap();
        assert_eq!(finding["rule"]["id"], "private-key");
        assert_eq!(finding["evidence"]["capture"], "payload");
        let evidence_path = directory.join(finding["evidence"]["path"].as_str().unwrap());
        let mut decoded = String::new();
        GzDecoder::new(File::open(evidence_path).unwrap())
            .read_to_string(&mut decoded)
            .unwrap();
        assert_eq!(decoded, content);

        drop(sniffer);
        fs::remove_dir_all(directory).unwrap();
    }
}
