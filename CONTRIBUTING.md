# Contributing

Infernaut is an operator-focused personal project. Focused bug reports,
compatibility findings, documentation improvements, and small, well-scoped
changes are welcome. Large feature proposals should begin with a discussion so
the gateway remains a router rather than becoming an agent harness.

## Development setup

Rust 1.89 or newer is required. The end-to-end suite additionally uses Bash,
Python 3, curl, jq, and standard Unix process tools.

```bash
cp .env.example .env
cp infernaut.example.toml infernaut.toml
cargo build --all-targets
```

Never commit real provider credentials, OAuth tokens, operator configuration,
model inventories, prompts, responses, plugin captures, or internal network
addresses. Use the example configuration and synthetic fixtures for tests.

## Before submitting a change

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features --locked
bash tests/e2e.sh
cargo package --locked
```

Changes that affect routing or privacy behavior should include tests for both
the intended path and its fail-closed behavior. Provider adapters should
preserve upstream errors unless a documented compatibility translation is
required.

Maintainer release steps are documented in
[docs/RELEASING.md](docs/RELEASING.md).

## Compatibility reports

When reporting a provider or harness issue, include:

- Infernaut version or commit
- provider and harness versions
- endpoint and sanitized request-shape log
- requested and resolved model identifiers
- HTTP status and sanitized upstream error

Do not attach live configuration or credential files. Redact prompts, response
content, client keys, bearer tokens, cookies, internal addresses, usernames,
and filesystem paths.
