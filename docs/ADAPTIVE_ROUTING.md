# Adaptive routing

Adaptive routing is an opt-in policy layer over Infernaut's existing physical
model and fail-closed route resolution. It classifies an OpenAI-compatible
request before provider preparation, then recommends or selects an eligible
physical model. It does not run an agent, alter prompts, execute tools, or call
another model to make the decision.

## Safe rollout modes

```toml
[routing]
mode = "shadow" # off | shadow | active
loaded_model_preference = "balanced" # quality_first | balanced | warm_first
session_ttl_seconds = 3600
```

- `off` preserves the original first-available route behavior.
- `shadow` classifies and scores adaptive routes, writes an `INFO` decision
  record, and still uses the route's original deterministic selection.
- `active` sends the request to the selected physical model.

Only routes with `strategy = "adaptive"` participate. Fully qualified physical
model IDs and existing `first_available` routes are never reclassified.

`loaded_model_preference` makes JIT/load churn an explicit routing tradeoff:

- `quality_first` gives a loaded model only a `0.001` tie-breaker. A meaningful
  quality difference always wins.
- `balanced` (default) adds `3` points for light work and `1` point otherwise.
- `warm_first` preserves the original behavior: `8` points for light work and
  `3` points otherwise.

The routing explanation records the selected policy and exact bonus. This
preference never makes an otherwise ineligible model eligible.

## Virtual route grammar

Route IDs may contain slash-delimited segments. A recommended convention is:

```text
route/{realm}/{task}[/{effort}]
```

For example:

```toml
[[routes]]
id = "private/auto"
title = "Private Auto"
description = "Automatically selects a private model and fails closed."
allowed_realms = ["local", "private"]
strategy = "adaptive"
task = "auto"
effort = "auto"
context_length = 131072
on_no_eligible_model = "deny"

[[routes]]
id = "cloud/code"
title = "Cloud Code"
allowed_realms = ["cloud"]
strategy = "adaptive"
task = "code"
effort = "auto"
on_no_eligible_model = "deny"
```

`task` may be `auto`, `code`, `data`, or `chat`. `effort` may be `auto`,
`light`, `standard`, or `heavy`. Fixing a route's task still allows operation
and effort classification. An explicit effort route is a manual override.

## Context contracts

`context_length` is the stable context window promised by a virtual route.
Infernaut publishes that value through both OpenAI- and Anthropic-shaped
model discovery and admits only physical models whose detected effective
context is at least that large. A model with a larger native window remains
eligible, but the route continues to advertise and enforce the configured
contract. This makes model changes safe in either direction during a growing
harness session.

```toml
[[routes]]
id = "private/auto-128k"
allowed_realms = ["local"]
strategy = "adaptive"
context_length = 131072
on_no_eligible_model = "deny"
```

Models below the contract are rejected as
`context_below_route_contract`; models with unknown context are rejected as
`context_unknown`. If the serialized request plus requested output reservation
exceeds the contract, Infernaut returns
`route_context_length_exceeded` before contacting a provider. If no model
satisfies the contract, the route fails closed.

One physical model may participate in multiple route tiers. For example, a
256K model can safely serve both 128K and 256K contracts. The contract is a
minimum eligibility requirement for physical models and the fixed advertised
ceiling for the virtual route; physical models do not need identical native
windows.

For compatibility, an adaptive route without `context_length` derives its
published window from the smallest eligible model, but only when every
eligible model has known context metadata. Otherwise the route omits the
window rather than guessing. This uncontracted mode is valid and remains
silent in `config doctor`; explicit contracts are recommended for stateful
agent harnesses whose sessions may move between physical models.

Claude Code currently budgets gateway models at either 200K or 1M rather than
consuming arbitrary context values from gateway discovery. Use a 200,000-token
contract with `CLAUDE_CODE_DISABLE_1M_CONTEXT=1` for Claude Code routes backed
by 256K-or-larger models. Other OpenAI- and Anthropic-compatible clients can
consume the route's exact published value normally.

### Route-scoped token estimation

Model eligibility reserves the requested maximum output and estimates input
from the complete serialized payload. The default of three bytes per token is
deliberately conservative. A route may explicitly select a less conservative
ratio when tokenizer evidence from its intended workload supports it:

```toml
[[routes]]
id = "private/auto"
allowed_realms = ["local"]
strategy = "adaptive"
token_estimate_bytes_per_token = 3.5
compression_max_output_tokens = 16384
on_no_eligible_model = "deny"
```

The accepted range is 1.0 through 8.0. A larger value admits more input bytes
per estimated token; it does not change the model's detected context,
advertised route contract, request body, or output reservation. Leaving the
setting absent preserves the fail-safe 3.0 default.

`compression_max_output_tokens` is used only when Infernaut recognizes a
Claude Code compaction request. It caps both the output tokens reserved during
context admission and a larger `max_tokens` value forwarded to the provider,
keeping the upstream request inside the same verified model ceiling. It does
not alter ordinary inference requests. Leaving it absent reserves and forwards
the complete harness-requested maximum.

Realms are configuration labels, not guesses:

- `local` normally identifies a provider on the host or LAN.
- `private` can identify an explicitly trusted remote or private-cloud
  transport.
- `cloud` identifies an ordinary external service.

Use `allowed_realms` to define the actual boundary. The global `local_only`
privacy lock and the per-request `x-infernaut-privacy: local_only` control
remain stronger than an adaptive decision. No routing score can broaden an
eligible realm.

## Classification

The built-in classifier examines transient request content in process and emits
content-free results:

- task: `code`, `data`, or `chat`
- operation: `inspect`, `review`, `debug`, `modify`, `implement`, `architect`,
  `migrate`, `analyze`, `summarize`, or `converse`
- effort: `light`, `standard`, or `heavy`
- matched signal names and a confidence value
- byte counts for current user intent and non-user scaffold, plus a conservative
  required-context estimate

Task and operation classification use the latest non-tool-result user input.
Effort may also consider prior user-authored text and the number of user turns.
System and developer prompts, assistant history, tool results, and tool schemas
are deliberately excluded from semantic intent. This prevents a large static
agent harness from turning `howdy` into a heavy coding request merely because
the harness describes repositories and attaches dozens of developer tools.

An active tool loop has one deliberately narrow structural exception. After
the latest real user-authored turn, Infernaut examines a bounded recent
window for assistant-issued requests to file-mutation tools such as
`Write`, `Edit`, `MultiEdit`, and `apply_patch`. Code-file counts, mutation-call
counts, and approximate mutation bytes can raise weak `chat/converse` evidence
to `code/modify` or `code/implement`, and substantial multi-file or large-write
work raises effort to `heavy`. Read/search tools, old writes preceding a newer
user turn, the mere presence of tool definitions, and non-code file writes do
not trigger this override.

Only structural counters become routing signals (`recent_code_mutations`,
`recent_code_files`, and `recent_mutation_bytes`). File paths and mutation
contents are neither retained nor emitted in decision logs. The actual request
body still passes through normally to its selected provider.

The full serialized request still participates in the independent
context-capacity check. Excluding harness scaffold from intent therefore does
not permit a small-context model to receive a request it cannot fit. File
extensions, code fences, stack traces, SQL/data terms, architecture language,
cross-codebase scope, testing, authentication, concurrency, user-authored
request size, and user-turn count are examples of semantic inputs. Tool
availability alone does not imply heavy coding because agent harnesses often
attach the same tool catalog to every request.

Classification does not persist request text. Core metrics remain content-free.
An observer plugin may separately receive content only when explicitly
configured.

## Manual model strengths

Model profiles are matched against canonical `provider/model` IDs. `*` wildcards
are accepted, although exact identities provide the most predictable result.
The guided configurator reconciles discovered and configured models without
discarding state: removed or unavailable models become dormant, returning
canonical IDs reactivate their profiles, and newly discovered models remain
unclassified until the operator accepts a suggested role preset or assigns one
manually. See [Guided configuration](CONFIGURATION.md).

```toml
[[routing.models]]
model = "lmstudio/small-code-moe"
context_length = 131072

[routing.models.strengths]
"code.inspect" = 0.90
"code.review" = 0.65
"code.implement" = 0.45
"chat" = 0.90
"default" = 0.50

[routing.models.traits]
architecture = "moe"
latency_tier = 1
cost_tier = 0
preferred_effort = ["light"]

[[routing.models]]
model = "chatgpt-codex/gpt-coding-model"

[routing.models.strengths]
"code.inspect" = 0.85
"code.review" = 0.95
"code.implement" = 1.00
"code.architect" = 1.00
"data.analyze" = 0.85

[routing.models.traits]
architecture = "dense"
latency_tier = 3
cost_tier = 4
preferred_effort = ["standard", "heavy"]
```

`routing.models.context_length` is an optional operator correction for a
physical model whose provider omits or misreports its window. It controls
catalog publication, route-contract eligibility, simulation, and request
admission. It does not allocate or enlarge upstream KV cache, so it must not be
set above the runtime's true configured limit. The Models screen cycles the
same value with `[` and `]`; automatic provider discovery resumes when the
override is cleared.

Strength values range from `0.0` through `1.0`. Lookup proceeds from the most
specific key to the least specific:

1. `{task}.{operation}`, such as `code.inspect`
2. `{task}`, such as `code`
3. `{operation}`, such as `inspect`
4. `default`
5. the built-in neutral value `0.50`

Traits are routing preferences rather than discovered facts. A low latency or
cost tier is favored for light work, and `preferred_effort` adds an explicit
affinity. Already-loaded models receive a small operational bonus, especially
for light requests. Provider catalog order resolves otherwise equal scores.

The guided configurator can also persist one workload role (`fast`, `inspect`,
`balanced`, or `heavy`) and additive `vision` and `compression` roles. These
roles record the origin of generated strengths so the Models screen can
distinguish applied presets from suggestions and later custom tuning. `heavy`
combines substantial development and data-analysis strengths. For requests
containing images, the additive Vision role contributes a ranking preference;
provider-reported vision support remains an eligibility constraint and cannot
be overridden by a manual role.

Set `auto_routing = false` on an exact or wildcard model profile to remove
matching models from adaptive candidate sets while preserving `/v1/models`
publication and direct physical-ID inference. This is intended for dedicated
compression and utility models that must remain addressable but should never
receive ordinary automatically routed traffic. The exclusion is applied before
scores, loaded-model preference, context checks, and explicit mapping rules.

### Claude Code compaction specialization

Claude Code does not expose a dedicated compaction-model setting. Infernaut
can specialize its compaction request inside an adaptive virtual route by
combining an explicit operator role with a narrow request signature:

```toml
[[routing.models]]
model = "agamotto/long-context-compressor"
auto_routing = false
roles = ["fast", "compression"]
```

Detection requires Claude Code's compound text-only, no-tools, structured
summary instructions; the word `summarize` alone is never sufficient. When the
signature is present and the route has an applicable compression-role model,
untagged models are rejected as `compression_role_required`. A tagged reserved
model is admitted with selection reasons `compression_role` and
`reserved_compression_model`.

The compression role changes model eligibility only. It does not weaken realm,
privacy, provider, route-contract, model-context, or protocol gates. This is
important because the compressor must accept the nearly full conversation
being compacted. Compression decisions are logged with
`compression_request=true` and `claude_code_compaction` in the signals.

Before scoring, provider-reported facts may remove an incompatible candidate:

- an image request rejects a model explicitly reported as vision-unsupported
- a required tool choice or active tool-result loop rejects a model explicitly
  reported as tools-unsupported
- structured output rejects a model explicitly reported as unsupported
- a known context limit rejects a request whose conservative size estimate
  exceeds that limit
- a context-contracted route excludes models below the contract or with an
  unknown context limit

`unknown` capability evidence remains eligible. Infernaut does not turn an
absence of evidence into an unsupported claim.

Supplying tool definitions alone does not require tool support. A model is
rejected for `tools_unsupported` only during an active tool-result loop or when
the request explicitly forces tool use. OpenAI `"required"` and forced
functions, Anthropic `any` and named tools, and Responses
`allowed_tools/required` are forcing choices. String or object-shaped `auto`
and `none` choices remain optional. Rejection explanations include the source,
for example `tools_unsupported(required_by=tool_choice:any)`.

## Explicit mapping rules

Rules take precedence over strength scores. Every supplied condition must
match, and rules are evaluated in configuration order.

```toml
[[routing.rules]]
route = "private/auto"
task = "code"
operation = "inspect"
effort = "light"
prefer = [
  "lmstudio/small-code-moe",
  "llamacpp/compact-coder",
]

[[routing.rules]]
route = "cloud/auto"
task = "code"
operation = "architect"
effort = "heavy"
prefer = [
  "chatgpt-codex/gpt-coding-model",
]
```

An unavailable or realm-ineligible preference is skipped. If none of a matched
rule's preferred models are eligible, normal strength scoring continues. This
does not introduce provider retries after a request has started.

## Tool-loop stability

Infernaut stores only a hash-derived conversation key, the selected model,
the content-free task/operation/effort classification, and a timestamp. When a
subsequent request ends in an OpenAI `tool` message or Anthropic `tool_result`
block, the prior eligible selection is pinned so a tool loop cannot oscillate
between providers. Entries expire after `routing.session_ttl_seconds`.

A pin is stable but not absolute. If concrete code-file mutations after the
latest user turn raise the active work beyond the stored classification,
Infernaut permits one re-score and records `tool_loop_escalation`. The newly
selected model is then pinned for subsequent calls. This allows a session that
began as a light inspection or conversation to graduate to a heavy coding
model once it starts implementing, without allowing routine tool calls to
bounce among providers or downgrade a heavy model.

The OpenAI API does not guarantee a conversation identifier. Infernaut uses
the request's `user` value when present and otherwise hashes stable initial
system/developer/user text. Tool-result-only user messages are excluded from
this identity so the key remains stable when the first tool response is added.
The original content is not retained.

## Decision logs

Every adaptive decision in `shadow` or `active` mode produces one `INFO` record:

```text
adaptive route decision routing_mode=Shadow route="private/auto"
task="code" operation="inspect" effort="light" confidence=0.93
intent_bytes=62 scaffold_bytes=84125 estimated_required_tokens=99103
signals=["code_signals=3","operation=inspect","bounded_read_or_chat"]
recommended_model="lmstudio/small-code-moe"
actual_model="lmstudio/loaded-default"
selection_reasons=["strength:code.inspect=0.90","preferred_effort"]
alternatives=["lmstudio/small-code-moe=102.00", ...]
rejected=["lmstudio/text-only=vision_unsupported"]
tool_loop_pinned=false
```

In shadow mode, `recommended_model` and `actual_model` may differ by design.
This provides a passive corpus for tuning mappings without storing prompts.
`intent_bytes` measures the latest user-authored text. `scaffold_bytes` is the
serialized request size minus all extracted user-authored text and is intended
as a diagnostic approximation, not token accounting. The required-token value
uses the same conservative three-bytes-per-token input estimate and requested
output reservation as model context eligibility.

The same decision engine is available without inference through the TUI
simulator or as JSON:

```bash
infernaut routing explain route:private:auto \
  --prompt "Read these two files and explain the validation behavior" \
  --config infernaut.toml
```

## Automated evidence

Provider discovery can establish protocol capabilities such as tools, vision,
and context size; explicitly unsupported requirements are enforced before
scoring. Explicit probes retain richer evidence for streaming, structured
output, and tools. Neither source establishes subjective coding or
architectural quality. Manual strengths are therefore preserved separately
from discovered capability evidence.

Future benchmark packs may add measured task-quality evidence, but automated
evidence must retain its source and must never silently overwrite a manual
strength or privacy constraint.
