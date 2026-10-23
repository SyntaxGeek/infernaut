# Architecture

## Product boundary

Infernaut owns model discovery, provider authentication, protocol normalization, routing constraints, health, and inference telemetry. The calling harness owns conversation state, planning, tools, and the agent loop.

```text
Hermes / editor / client
          |
          | OpenAI-compatible API or Anthropic Messages
          v
     Infernaut
       |       |\
       |       +-- policy routes (`route/private`)
       |       +-- adaptive routes (`route/private/auto`)
       |       +-- optional observer process (report only)
       |       +-- read-only live topology event feed
       |
       +-- physical adapters (`provider/model`)
                    |
          local runtimes and cloud APIs
```

## Request ordering

Every request follows the same decision order:

1. Authenticate the local client.
2. Identify a physical model, deterministic route, or adaptive route.
3. Enforce the global privacy lock and route realm constraints.
4. For an enabled adaptive route, transiently classify task, operation, and
   effort, then rank only eligible catalog models.
5. Require the selected model to exist in the latest healthy catalog.
6. Translate the qualified ID to its exact upstream model ID.
7. Require native Anthropic Messages support when `/v1/messages` is used.
8. Nonblockingly publish subscribed request content to optional observers.
9. Proxy once, without implicit fallback.
10. Nonblockingly publish response lifecycle events to optional observers.
11. Record content-free routing and latency metadata.

The live route observer consumes a separate content-free lifecycle stream:
`accepted`, `classified`, `loading`, `forwarding`, `headers`, `streaming`,
`completed`, `failed`, and `disconnected`. `headers` means the provider
accepted the request; `streaming` begins only with the first upstream body
bytes. Infernaut publishes those records into a
bounded process-local ring and broadcast channel. Neither history retention nor
live delivery can block the inference path.

Constraints are never converted into ranking weights. A data-driven routing layer may eventually rank eligible models, but it cannot redefine eligibility.

Adaptive routing now provides that optional ranking layer. `shadow` mode logs a
recommendation while preserving the route's original deterministic selection;
`active` mode uses the recommendation. Manual strengths, model traits, and
explicit mapping rules influence ranking only after privacy, realm, provider,
route-candidate, and catalog-availability constraints. Fully qualified physical
IDs and ordinary routes bypass classification.

Classification runs in process and does not invoke another model. Semantic
classification uses user-authored input rather than static system/developer
instructions, assistant history, tool results, or tool schemas. The full
serialized request remains authoritative for context-capacity eligibility.
Request content is transient and is reduced to task, operation, effort, signal
names, confidence, byte counts, and a conservative token estimate for core
logs. A hash-derived conversation key retains only the prior model and
timestamp so OpenAI tool-result requests can remain pinned during a tool loop.
See [Adaptive routing](ADAPTIVE_ROUTING.md).

In selective privacy mode, local and cloud providers can coexist in one catalog. Realm-constrained routes remain the durable policy interface, and `x-infernaut-privacy: local_only` adds a per-request fail-closed constraint. The header is evaluated after deterministic model resolution but before provider preparation, loading, or network I/O. It cannot request a weaker policy than the global `local_only` emergency lock and is never forwarded upstream.

## Failure semantics

`route/private` fails with `routing_policy_violation/no_eligible_model` when no local model is available. Infernaut does not attempt a cloud request, provider probe, or retry. If an upstream stream fails after output begins, the stream ends; another provider is not substituted.

Provider diagnostics are an observational overlay, not routing policy. Infernaut matches
known signatures in ordinary response bodies and complete SSE events, retains the provider's
error fields, and attaches namespaced guidance for clients and logs. A diagnostic never
suppresses the original error, changes its HTTP status, retries a request, or broadens the
eligible provider realm.

Observer plugins are also an observational overlay. Each configured executable receives a
versioned NDJSON event stream through a bounded queue. Enqueue is nonblocking, so inference
does not inherit plugin latency; overload is represented as visible dropped-event counters.
The V1 process protocol has no decision or transformation channel. The executable is trusted
local code, not sandboxed code. The full authority split and enforcement roadmap are recorded
in [Inspection and plugin architecture](INSPECTION_AND_PLUGINS.md).

The interactive route observer is distinct from content observer plugins. Its
authenticated SSE feed never includes prompt, response, tool-schema, or token
content and exposes no mutation channel. It reports only request identity,
client/network labels, requested and resolved routing metadata, byte counts,
estimated token counts, lifecycle state, timing, status, and sanitized errors.
The observer renders only destinations proven by traffic; a successful vLLM
request is evidence that the model was serving at that moment, not a permanent
claim that it remains loaded. See [Live route observer](LIVE_OBSERVER.md).

## Catalog semantics

Physical model IDs have the form `provider/upstream-model-id`. The provider separator is the first slash only. Virtual IDs use the reserved `route/` namespace.

The internal canonical form always uses `/`. The optional `colon_qualified` publication form replaces slash delimiters with literal colons in `/v1/models` IDs for harnesses that visually discard slash prefixes. Resolution accepts either form. Catalog publication is deterministically alphabetized using the configured `provider_then_model` or `model_then_provider` order. Provider sorting uses the visible alias when configured, and routes participate under the `route` namespace. Publication order never changes internal routing priority.

An optional provider `alias` changes only the namespace published to clients.
Canonical identity, provider lookup, policy enforcement, telemetry, and upstream
translation remain keyed by the provider `id`. Alias uniqueness is validated at
startup to keep every published model ID unambiguous.

Capability metadata begins as `unknown/unverified`. Future probes may add evidence, but an adapter accepting a field is not sufficient evidence that a model reliably supports the capability.

Manual conformance probes can add versioned evidence for chat, SSE streaming, tools, and structured output. A dry-run plan is side-effect free. Actual runs require an explicit API or CLI action, reject unloaded models by default, use bounded output and operation timeouts, and cache results by model/runtime fingerprint. Probe evidence is not routing authority in V1.

Provider-specific discovery may supply richer declared evidence. The LM Studio adapter reads `/api/v1/models` to include downloaded LLMs and their native metadata while retaining `/v1` for OpenAI-compatible inference. Loaded models resolve to their transient instance IDs. Unloaded models may be delegated to LM Studio JIT or explicitly loaded through `/api/v1/models/load`; these strategies are mutually exclusive because explicit loads may not participate in JIT auto-eviction. Embedding models are excluded from the chat-oriented unified catalog for now.

Endpoint-only audio services form separate STT and TTS lanes. A provider with
`kind = "openai_audio"` declares static model identities and exactly one service
capability per identity because the upstream need not implement model
discovery. The legacy `openai_transcription` kind models only STT. Audio
identities and routes are excluded from the chat catalog and adaptive routing
engine, and a route cannot mix STT, TTS, or ordinary inference models.

`POST /v1/audio/transcriptions` resolves only STT, rewrites only the multipart
model field, and forwards the binary upload unchanged. `POST /v1/audio/speech`
and `/v1/audio/speech/stream` resolve only TTS and rewrite only the JSON model
field; returned audio bytes are forwarded without SSE annotation or text
plugin inspection. `GET /v1/audio/voices` follows the sole configured TTS route
and fails closed when selection would be ambiguous. These paths perform no
prompt classification, model activation, fallback, or content transformation.
Ordinary request metadata, privacy enforcement, response-byte telemetry, and
error propagation remain in force.

Cloud API-key credentials are resolved from named environment variables at request time and are never returned in the catalog or logs. Subscription OAuth credentials are isolated in a private JSON store, refreshed under a process-wide lock, and selected only after realm/privacy policy resolution. OpenAI API access uses `/v1/models`; ChatGPT subscription access uses the Codex device flow, Codex model catalog, and Responses transport. xAI API-key and OAuth providers both use `/v1/language-models`, preserving canonical IDs, aliases, modalities, version, and provider fingerprint while excluding image and video generation catalogs. All adapters retain the provider-qualified public model ID at the Infernaut boundary.

The ChatGPT Codex adapter is a wire-protocol adapter, not an agent runtime. It converts Chat Completions messages, tool calls, and streaming deltas to and from Codex Responses objects while leaving conversation state, tool execution, retries, and prompting with the calling harness. Native `/v1/responses` requests bypass this conversion.

Ollama Cloud uses the service's OpenAI-compatible `/v1` surface with a provider-scoped `OLLAMA_API_KEY`. It remains a cloud-realm provider even when the same model family also exists in a local Ollama runtime; realm follows the configured transport, not the model name.

The llama.cpp adapter targets multi-model router mode. It discovers stable model IDs and runtime state through `/models`, retains `/v1` for inference, and delegates unloaded or sleeping models to router autoload. Explicit loading through `/models/load` remains available as an alternative configuration. Infernaut does not attempt to reproduce llama.cpp's `--models-max` or idle-sleep policy; those remain runtime constraints.

Anthropic Messages is a second native boundary, not an OpenAI translation
adapter. `POST /v1/messages` preserves Anthropic request content blocks, tool
blocks, response objects, and SSE event types while replacing only the public
qualified model ID with the selected upstream model ID. LM Studio, llama.cpp,
and vLLM declare native support by provider kind. Generic OpenAI-compatible
providers must set `anthropic_compat = true`; providers without that
declaration fail before activation or upstream traffic. Client `x-api-key`
authentication is consumed locally, while protocol headers such as
`anthropic-version` and `anthropic-beta` pass through.

`POST /v1/messages/count_tokens` follows the same model and privacy resolution.
vLLM and llama.cpp receive a native upstream token-count request. For providers
that expose Messages without token counting, Infernaut returns a conservative
local estimate based on the complete serialized request. This fallback does not
activate a model, perform upstream traffic, or enter inference metrics.

## Resource arbitration

Inference always has priority over capability discovery. Infernaut counts active inference responses through the end of their response streams. A probe waits for zero active inference plus the configured idle grace period. If inference begins while a probe HTTP operation is pending, that operation is cancelled and the job becomes `preempted`; no partial capability evidence is published. This keeps model loading and probe work out of harness timeout paths.

## Analytics semantics

Core analytics record request count, upstream success status, resolved model, provider,
and time to response headers without persisting request content. Adaptive routing may
transiently inspect a request and logs only its content-free classification and decision
evidence. An explicitly configured observer may receive content under its own storage
policy; this does not place raw content in core metrics. Passive reliability can become
routing evidence later, but response status alone is not treated as model quality.
