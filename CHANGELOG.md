# Changelog

All notable changes to this project will be documented in this file. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project intends to use [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Dependencies

- Upgrade Ratatui to 0.30.2 and Crossterm to 0.29; use patched LRU 0.18.4
  and remove unmaintained paste.
- Replace yanked chacha20 0.10.1 with 0.10.2 in the dependency lockfile.

### Added

- Unified OpenAI- and Anthropic-compatible model gateway.
- Local and cloud provider discovery with explicit privacy realms.
- Deterministic physical IDs, virtual routes, exact aliases, and harness
  recipes.
- Adaptive routing, context contracts, compression dispatch, and tool-loop
  pinning.
- Guided configuration TUI and read-only Infernaut Mission Control observer.
- Dedicated OpenAI-compatible transcription and speech synthesis lanes.
- Optional observer plugins and report-only keyword/regex sniffer.
- Content-free runtime metrics and provider-error diagnostics.

### Changed

- Restructured the README for public launch: grouped feature overview with
  screenshots of Mission Control and the configurator, and moved model-catalog
  and Hermes audio deep-dives into new `docs/MODEL_CATALOG.md` and
  `docs/HERMES_AUDIO.md` pages. No behavior changed.
- Replaced lab-internal hostnames in test fixtures and recipe examples with
  fictional provider names.

### Security

- Reject upstream redirects and bound request, buffered-response, and SSE buffers.
- Serialize OAuth store updates and refuse stale configuration-editor saves.
- Isolate routing pins by client, with optional session IDs and bounded caches.
- Fail closed on failed, incomplete, or truncated Codex event streams.

### Deployment

- Rust 1.89 minimum, Docker/Compose packaging, and container TUI instructions.
- Graceful SIGINT/SIGTERM draining and bounded provider-header/OAuth-refresh waits.
- Server and observer use the executable's non-shell dotenv loader.
- Codex reasoning effort is preserved; unsupported output caps are explicitly warned.

### Security (existing)

- Fail-closed privacy locks, realm constraints, route resolution, and client
  authentication.
- Local credentials, operator configuration, backups, and plugin evidence are
  excluded from version control and distribution packages.
