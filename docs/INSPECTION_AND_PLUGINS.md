# Inspection and plugin architecture

## Status and intent

This document preserves the agreed shape of Infernaut's optional inspection
and plugin work. It is both a product boundary and an implementation roadmap.
The first delivered slice is a report-only observer protocol plus the
`infernaut-sniffer` reference plugin. Enforcement is deliberately deferred
until the observer path has operational evidence and a stable contract.

Infernaut remains a local-first LLM gateway. It is not an agent, SIEM, DLP
platform, long-term evidence store, tool sandbox, or host-monitoring product.
Plugins extend the inference boundary without redefining the core.

## Observable boundary

Infernaut can observe only traffic that passes through its provider endpoint:

- requests sent by a harness to a model;
- model response deltas and completed responses;
- proposed tool calls encoded in model responses;
- tool results included in later model requests;
- routing, provider, model, realm, lifecycle, and diagnostic metadata.

Infernaut cannot observe whether a harness executed a tool, what a command did,
filesystem or process activity, host memory, or network connections made outside
Infernaut. Findings are partial and fallible. They may contain false positives
and false negatives and are not a guarantee that an agent or environment is safe.

Provider streams expose content deltas, not necessarily tokenizer tokens. The
protocol therefore names streaming events `response.delta`. Re-tokenization is a
plugin concern when a model-specific tokenizer is available.

## Plugin classes

The long-term protocol distinguishes three authorities:

1. **Observer** — receives immutable events for reporting, telemetry, or
   asynchronous analysis. It is not in the inference decision path.
2. **Guard** — makes a bounded synchronous `allow`, `warn`, `block`, or circuit
   decision. Its timeout and failure behavior must be explicit.
3. **Transformer** — intentionally rewrites a request or response for features
   such as compression or normalization. Transform permission is never implied by
   observation permission.

The first implementation supports observers only. Configuration that requests
guard or transformer authority must fail validation until those contracts exist.

## Event model

Events use an append-friendly, versioned NDJSON envelope:

```json
{
  "schema": "infernaut.plugin-event.v1",
  "event": "request.content",
  "request_id": "inf_...",
  "sequence": 0,
  "timestamp_unix_ms": 1784150000000,
  "endpoint": "/v1/chat/completions",
  "provider": "lmstudio",
  "model": "lmstudio/qwen3.6-35b-a3b",
  "realm": "local",
  "content_type": "application/json",
  "content": "{...}"
}
```

The initial observer events are:

- `request.content` — a serialized representation of the upstream request body
  after model resolution and protocol adaptation, without HTTP authorization
  headers;
- `response.delta` — raw upstream response bytes decoded lossily as UTF-8;
- `response.completed` — marks the end of the response and includes no content;
- `provider.error` — transport errors visible at the gateway boundary;
- `diagnostic.matched` — Infernaut's provider-error diagnostic metadata.

Future versions may add structured `tool.call.proposed`, `tool.result`,
`model.loaded`, and usage events. Additions are versioned and do not silently
change existing event meaning.

## Process boundary and trust

The portable observer transport is an out-of-process executable using NDJSON on
standard input. Infernaut owns a bounded asynchronous queue per plugin. A slow
or failed observer cannot delay inference. Queue saturation increments a visible
dropped-event counter; loss is never silent.

The initial process transport provides crash isolation, not a security sandbox.
Configured executables are trusted local code with the invoking user's OS
authority. Infernaut never sends provider authorization headers, OAuth tokens,
API keys, or credential-store contents through the event protocol. Future WASM
components may provide capability-based filesystem and network isolation for
untrusted plugins; native dynamic libraries are intentionally excluded.

## Content and privacy

Content access is opt-in per plugin and disabled when no plugin is configured.
An observer declaration lists the events it receives. The global `local_only`
privacy lock also forbids any future cloud-realm content exporter; the reference
sniffer has no network behavior and writes only to local storage.

Raw content is sensitive. Operators must treat finding and evidence files as
security data. Infernaut's normal metrics remain content-free.

## Reference sniffer

`infernaut-sniffer` is a free, local, report-only reference observer. It is
intended for home labs, small organizations, air-gapped environments, tinkerers,
and plugin authors. It supports:

- case-sensitive or case-insensitive keyword rules;
- regular expressions implemented with Rust's non-backtracking regex engine;
- request/response and local/cloud realm filters;
- per-rule severity and capture policy;
- JSONL findings;
- optional content-addressed evidence blobs;
- file permissions, retention, rotation, and storage limits;
- sniffer shutdown counters for received, inspected, matched, truncated, evicted,
  and failed work, plus Infernaut health counters for transport drops.

It does not claim to detect semantic, encrypted, encoded, or covert exfiltration.
Keywords use compiled multi-pattern matching where practical; regexes compile at
startup and cannot use arbitrary backtracking PCRE features.

## Storage contract

The sniffer behaves like a web server logger, not a reporting platform:

- append-only `findings-*.jsonl` files;
- optional content-addressed evidence blobs;
- age-based cleanup and a total managed-storage cap;
- user-only filesystem permissions on Unix;
- no index, search engine, dashboard, alert manager, or long-term archive;
- no network upload unless a separate future exporter is explicitly authorized.

Capture levels are:

- `none` — finding metadata and content hash only;
- `match` — matched text and bounded context;
- `message` — the logical request or assembled response handled by the plugin;
- `payload` — the complete available request or assembled response, subject to a
  configured byte cap.

Large evidence is stored separately and referenced by SHA-256. Findings record
content truncation, while Infernaut health records dropped observer events.
Full-payload capture is opt-in and produces a startup warning.

## Circuit breaker roadmap

The future inference circuit breaker is distinct from observation. It can stop
LLM traffic but cannot terminate an agent or undo a tool side effect. Proposed
decisions are `observe`, `warn`, `block_request`, `abort_stream`, `trip`, and
`quarantine`, scoped to a request, client, session, model, provider, realm, or all
inference.

Important semantics:

- pre-request inspection can prevent content from reaching a provider;
- a complete buffered tool call can be blocked before the harness receives it;
- streamed bytes already released cannot be recalled;
- global or cloud-realm trip authority requires a separate high-risk permission;
- a plugin cannot grant itself broader authority than its manifest;
- latches are auditable, persist across restart by default, and require an
  authenticated reset unless an explicit TTL is configured;
- deterministic high-confidence rules are preferred for enforcement;
- shadow/report mode precedes blocking deployment.

Streaming enforcement modes will be explicit: immediate observation, bounded
windowing, tool-call-only buffering, or complete response buffering. Their latency
and prevention guarantees must be documented separately.

## Delivery phases

1. **Report-only foundation** — typed events, bounded process observers,
   reference sniffer, local JSONL/evidence storage, health counters, fixtures, and
   operator/plugin-author documentation.
2. **Standard exporters** — OpenTelemetry/OpenInference observer and stable
   redaction controls, without making Infernaut a dashboard.
3. **Guard contract** — bounded decisions, shadow mode, explicit failure policy,
   structured tool-call buffering, and scoped circuit breakers.
4. **Sandboxed extensions** — capability-limited WASM components where the
   portability and dependency tradeoff is justified.
5. **Transformers** — separately authorized prompt/token compression and other
   rewrites with complete auditability.

This sequence preserves Infernaut's identity: routing and policy remain the
core; inspection, reporting, enforcement, and optimization remain optional and
explicitly authorized.
