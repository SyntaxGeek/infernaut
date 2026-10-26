# Observer plugins, the reference sniffer, the sentinel, and the egress watcher

Infernaut observer plugins are optional, out-of-process programs that receive
typed copies of inference-boundary events. They are suitable for lightweight
security findings, telemetry, and experimentation without putting plugin work in
the provider response path. The first protocol is report-only: an observer cannot
modify, block, retry, redirect, or terminate inference.

The durable product boundary and future guard/transformer roadmap are recorded in
[Inspection and plugin architecture](INSPECTION_AND_PLUGINS.md). This page is the
operator and plugin-author guide for the implemented observer protocol.

## Build and enable the sniffer

Build both Infernaut executables:

```bash
cargo build --release --bins
cp sniffer.example.toml sniffer.toml
target/release/infernaut-sniffer --validate sniffer.toml
```

Validation parses and compiles every signature without creating the storage
directory. Review the example rules and capture levels before enabling them.
Then add the observer to `infernaut.toml`:

```toml
[[plugins]]
id = "sniffer"
mode = "observe"
command = "./target/release/infernaut-sniffer"
args = ["sniffer.toml"]
events = [
  "request.content",
  "response.delta",
  "response.completed",
  "provider.error",
  "diagnostic.matched",
]
queue_capacity = 1024
```

Paths are resolved from Infernaut's working directory. `command` is executed
directly, not through a shell. Restart Infernaut after changing plugin or rule
configuration. Startup logs contain `observer plugin started` and the sniffer
prints `report-only observer ready` to standard error.

Check transport health without reading content:

```bash
curl --silent http://127.0.0.1:4040/health | jq '.plugins'
```

Each plugin reports `running`, `delivered_events`, `dropped_events`, and
`write_failures`. `delivered_events` means the NDJSON record was written to the
child's standard input. A nonzero dropped count means the bounded queue filled or
the child stopped. It should be investigated because a missing delta or completion
can make a finding incomplete.

## Sniffer rules

The sniffer accepts compiled keyword and regular-expression rules:

```toml
[[rules]]
id = "private-key"
description = "Private key material visible at the inference boundary"
severity = "critical"
type = "keyword"
patterns = ["BEGIN PRIVATE KEY", "BEGIN OPENSSH PRIVATE KEY"]
case_insensitive = true
directions = ["request", "response"]
realms = ["cloud"]
capture = "payload"
max_findings_per_payload = 3

[[rules]]
id = "credential-assignment"
description = "Likely credential assignment"
severity = "high"
type = "regex"
pattern = '''\b(api[_-]?key|password|secret|token)\s*[:=]\s*\S+'''
case_insensitive = true
directions = ["request", "response"]
capture = "match"
```

`directions` and `realms` are optional; an empty list matches both directions or
all configured realms. Keyword patterns are compiled into a multi-pattern
automaton. Regex rules use Rust's bounded, non-backtracking regex engine, so PCRE
look-around and backreferences are intentionally unavailable. Invalid or overly
large expressions fail startup.

The sniffer examines the serialized provider request after routing and protocol
adaptation. Streaming response bytes are assembled up to `max_payload_kb` and
examined when `response.completed` arrives. This includes SSE framing and JSON
envelopes. Infernaut observes provider deltas, not guaranteed tokenizer tokens.

This bundled sniffer is a **wire-payload matcher**, not a semantic token scanner.
A keyword split across SSE events may not match, and UTF-8 split across network
chunks may be replaced during text conversion. Do not rely on it to detect all
occurrences of a secret or to block exfiltration. Use a protocol-aware external
plugin when reconstructed generated-text inspection is required.

## Findings and offending payload evidence

Every match appends one `infernaut.finding.v1` record to a JSONL file:

```json
{
  "schema": "infernaut.finding.v1",
  "timestamp_unix_ms": 1784150000000,
  "request_id": "inf_...",
  "provider": "openai",
  "model": "openai/gpt-5",
  "realm": "cloud",
  "endpoint": "/v1/chat/completions",
  "direction": "request",
  "rule": {
    "id": "private-key",
    "description": "Private key material visible at the inference boundary",
    "severity": "critical",
    "action": "report"
  },
  "matched": {
    "offset": 120,
    "length": 17,
    "pattern": "BEGIN PRIVATE KEY",
    "content": "BEGIN PRIVATE KEY"
  },
  "content_sha256": "...",
  "evidence": {
    "capture": "payload",
    "path": "evidence/....blob.gz",
    "sha256": "...",
    "bytes": 1842,
    "truncated": false
  }
}
```

Capture is selected per rule, falling back to `storage.capture`:

- `none` records metadata, offsets, pattern, and the available-content hash.
- `match` stores the matched text plus bounded surrounding context inline.
- `message` stores the logical request or assembled response as a separate blob.
- `payload` stores the complete available request or assembled response as a
  separate blob. In this first transport, `message` and `payload` have the same
  available boundary; the separate names preserve the future structured contract.

`message` and `payload` evidence is content-addressed by SHA-256 and optionally
gzip-compressed. This is how a finding can include the offending payload without
making every JSONL line enormous. `max_payload_kb` is always enforced, and
`truncated` states when the complete available content was not retained. Full
payload capture produces a visible startup warning. Inline matched text is capped
at 4 KiB and inline match context at 64 KiB; the original offset and length remain
in the finding when a larger match is truncated.

Inspect findings and a referenced compressed evidence blob with standard tools:

```bash
jq -c . ~/.infernaut/sniffer/findings-*.jsonl
gzip -dc ~/.infernaut/sniffer/evidence/<sha256>.blob.gz
```

## Storage behavior

The `[storage]` section makes the sniffer a bounded logger rather than a reporting
database:

```toml
[storage]
path = "~/.infernaut/sniffer"
capture = "match"
retain_days = 14
max_total_mb = 500
max_file_mb = 25
max_payload_kb = 512
context_before = 80
context_after = 80
compress = true
```

Findings rotate at `max_file_mb`. Managed finding and evidence files older than
`retain_days` are removed at startup and rotation. When managed storage exceeds
`max_total_mb`, the oldest non-active files are removed. New large evidence is
omitted with `storage_limit_reached` if it would cross the limit; the finding
metadata is still written. On Unix, directories are mode `0700` and files are
mode `0600`.

Infernaut does not provide search, dashboards, alerts, archival, or network
upload. A downstream system can ingest the JSONL blob and referenced evidence as
needed. Retention deletion can leave an older finding whose evidence blob has
expired; consumers must treat evidence paths as retention-scoped references.

## Structural injection scanning with the sentinel

`infernaut-sentinel` is a second bundled observer. Where the sniffer matches
wire payloads, the sentinel parses the request body per endpoint and tags every
text segment with its provenance before scanning:

- Anthropic `/v1/messages`: `system` fields and blocks are system text, plain
  `text` blocks follow their message role, and `tool_result` blocks are
  untrusted regardless of the envelope role.
- OpenAI `/v1/chat/completions`: role `tool` and `function` messages are
  untrusted.
- OpenAI `/v1/responses`: `function_call_output` items are untrusted;
  `instructions` is system text.
- `/v1/embeddings`: inputs are treated as untrusted corpora.
- Bodies that fail to parse become one untrusted segment so nothing transiting
  the gateway goes unscanned.

Builtin detectors run only on untrusted segments: conservative
instruction-override and chat-template markers, invisible Unicode smuggling
(zero-width, bidi controls, tag characters), mixed-script homoglyph runs, long
hex/base64 blobs, and hidden HTML comments. User rules run on untrusted
segments always and on user/system/assistant segments only when the `[scan]`
toggles enable them.

Enable it next to (or instead of) the sniffer:

```bash
cp sentinel.example.toml sentinel.toml
target/release/infernaut-sentinel --validate sentinel.toml
```

```toml
[[plugins]]
id = "sentinel"
mode = "observe"
command = "./target/release/infernaut-sentinel"
args = ["sentinel.toml"]
events = ["request.content"]
```

The sentinel is request-side only: it ignores response events entirely. Pair it
with the sniffer for response-direction rules. Each match appends one
`infernaut.sentinel-finding.v1` record to `findings-*.jsonl` with the segment's
provenance, path, and detector id, plus the same content-addressed evidence
scheme as the sniffer with `capture = none | match | segment | payload`.

### Volume, deduplication, and disk bounds

The sentinel is built for chatty workloads without filling your disk:

- **Segment deduplication.** LLM APIs resend the full transcript on every call,
  so identical tool results arrive repeatedly. The sentinel hashes each segment
  and reports each distinct segment once per process; the exit stats include
  `dedup_skipped`. The seen-set is in memory (bounded at 100k hashes, ~7 MiB),
  so a plugin restart rescans the current history exactly once. Content that
  mutates between requests (new tool output, changed text) gets a fresh hash
  and is always reported.
- **Flood caps.** At most 200 findings per request and 100 per segment,
  regardless of how noisy the content is.
- **False-positive guards on encoded blobs.** Hex requires 64+ hex digits.
  Base64 that decodes to printable text is only reported when the decoded text
  carries an injection marker, so identifiers and slugs stay quiet; blobs with
  three or more slashes plus path-like characters are treated as filesystem
  paths, which share the base64 alphabet but decode to garbage.
- **Hard disk ceiling.** `max_total_mb` bounds all findings files and evidence
  blobs; oldest files are evicted on rotation, and the in-flight file is capped
  by `max_file_mb`, so worst-case usage is `max_total_mb + max_file_mb`.
  `retain_days` prunes anything older. When the quota is full, evidence is
  omitted (`omitted_reason: storage_limit_reached`) but findings are still
  written.

Findings are plain JSONL and evidence blobs are content-addressed files, so a
log agent can tail `findings-*.jsonl` (handling rotation by glob) and forward
to Grafana Loki, Elasticsearch, or any other sink. A natural reference
deployment runs Loki, Alloy, and Grafana next to the gateway: Alloy tails the
findings files and container logs, Loki stores 30 days, and a provisioned
dashboard covers findings by detector, severity, and provenance.

The sniffer boundary applies unchanged: the gateway sees the assembled
provider-boundary body, not where content originally came from. A harness that
pastes fetched web text into a plain user message is indistinguishable from the
operator's own input, and report-only findings cannot block a request.

## Egress monitoring with the egress watcher

The sentinel defends the request direction: untrusted content flowing toward
the model. `infernaut-egress` covers the opposite direction: what the MODEL
sends back. Motivated by the Qwen3.8 exfiltration findings (Cohere, October
2026): a tool-capable open-weight model that attempted an upload to a Beijing
cloud bucket while its reasoning trace claimed no tools would be called. A
reasoning trace is not a security boundary, so the watcher enforces nothing on
chain-of-thought and everything on the actual emitted output.

Subscribe the watcher to `response.delta` and `response.completed`:

```toml
[[plugins]]
id = "egress"
mode = "observe"
command = "/usr/local/bin/infernaut-egress"
args = ["egress.toml"]
events = ["response.delta", "response.completed"]
queue_capacity = 1024
```

The watcher buffers response chunks per request id, reassembles streamed
tool-call argument fragments (OpenAI `arguments`, Anthropic `partial_json`,
and friends), and scans the reassembled text plus every complete JSON string
in the body. Detection is report-only; findings land as
`infernaut.egress-finding.v1` JSONL in `storage.directory` with the same
rotation, retention, and total-quota semantics as the sentinel, minus the
evidence blobs (the matched text is inlined, capped at `max_match_chars`).

Detectors, with defaults from `egress.example.toml`:

| Detector | Severity | Fires on |
|---|---|---|
| `egress.metadata-endpoint` | high | `169.254.169.254`, `100.100.100.200`, `fd00:ec2::254`, `metadata.google.internal` |
| `egress.denylisted-host` | high | any host matching `domains.denylist` (wildcard `*.suffix` supported) |
| `egress.namespace-escape` | high | `unshare -U...`, `nsenter` |
| `egress.privilege-trick` | medium | `--propagation private`, `mount ... tmpfs` |
| `egress.upload-command` | medium | `curl`/`wget` with `-X PUT/POST`, `--upload-file`, `-T`, `--data-binary`, `-d @`, `-F` |
| `egress.cloud-storage` | medium | URL host on common cloud storage suffixes (`*.aliyuncs.com`, `*.amazonaws.com`, ...) |
| `egress.download-to-disk` | info | `curl`/`wget` writing a fetched URL to disk (the Qwen3.8 trigger shape) |
| `egress.external-domain` | info | any URL host not on `domains.allowlist` |

`egress.external-domain` is the "who is my model calling" feed: point
`domains.allowlist` at the API destinations your agents legitimately use and
everything else shows up per host, capped at `max_domain_findings_per_response`
per response. Metadata and denylisted hosts are reported only by their stronger
detector, never duplicated as external domains.

Boundaries worth naming: the watcher sees only traffic that transits Infernaut
and cannot see the outbound network call an agent makes from its own host; it
is detection and forensics, not prevention. Deny egress at the network layer,
block unprivileged user namespaces in agent containers, and treat the findings
as the audit trail. Buffering is bounded by `max_buffer_bytes` per response and
`max_tracked_requests` in-flight responses; the oldest buffer is dropped and
counted under pressure.

## Performance and failure semantics

Each observer owns a bounded asynchronous queue. Inference performs a nonblocking
enqueue and never waits for signature matching, disk I/O, or a slow child process.
The tradeoff is explicit loss under pressure. Infernaut logs queue drops at the
first occurrence and power-of-two counts, and `/health` exposes the totals.

The reference sniffer bounds assembled content with `max_payload_kb` and bounds
incomplete request state to 4,096 requests. If lifecycle events are lost, the
oldest incomplete state is eventually evicted and counted in the shutdown log.
Plugin crashes do not stop Infernaut. Conversely, report-only findings cannot
stop a request that appears malicious.

## Protocol for plugin authors

Infernaut starts each configured process once and writes one UTF-8 JSON object
per line to its standard input. There is no shell, handshake, response channel, or
restart loop in V1. Plugins should log to standard error, process input
incrementally, flush durable output deliberately, and exit when standard input
closes.

The envelope schema is `infernaut.plugin-event.v1`:

| Field | Meaning |
|---|---|
| `schema` | Exact protocol schema name |
| `event` | Subscribed event kind |
| `request_id` | Correlates one inference lifecycle |
| `sequence` | Process-wide monotonic event sequence |
| `timestamp_unix_ms` | Event creation time |
| `endpoint` | Infernaut inference endpoint |
| `provider` | Stable provider ID |
| `model` | Canonical provider-qualified model ID |
| `realm` | Resolved `local` or `cloud` realm |
| `content_type` | Content representation when present |
| `content` | Optional sensitive content |
| `diagnostic_code` | Optional Infernaut diagnostic ID |

Events available in V1 are `request.content`, `response.delta`,
`response.completed`, `provider.error`, and `diagnostic.matched`. Authorization
headers, OAuth tokens, API keys used for provider authentication, and the auth
store are never inserted into events. Credentials that a client includes inside a
prompt or tool result are content and are therefore visible to a subscribed
observer.

Observer executables are trusted local code running with Infernaut's operating
system privileges. The process boundary provides crash isolation, not a sandbox.
An observer could access the network or filesystem independently; review its code
and OS confinement. The bundled sniffer itself performs no network operations.

Most importantly, this boundary sees only inference I/O through Infernaut. It
cannot prove whether an agent executed a tool, inspect its host, observe a direct
provider connection, or guarantee detection of encoded, encrypted, semantic, or
covert exfiltration.
