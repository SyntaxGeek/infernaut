# Guided configuration

Infernaut can be configured interactively without giving up the durability,
reviewability, or portability of its TOML file. The configurator is a terminal
UI over the same configuration types and validation rules used by the server;
it is not a second service and it does not maintain a separate database.

```bash
cargo run --release -- configure infernaut.toml
```

Provider discovery is read-only and performs no model inference. To edit a
configuration while its providers are unavailable, start offline and refresh
discovery later with `r`:

```bash
cargo run --release -- configure infernaut.toml --offline
```

The binary loads `.env` from the directory containing the selected
`infernaut.toml` before opening the TUI. This applies equally to provider
credentials, the administrative server key, and named client keys. Values
already exported by the parent process win over `.env`; set
`INFERNAUT_ENV_FILE=/path/to/credentials.env` to select another file. The
parser accepts ordinary `KEY=VALUE`, `export KEY=VALUE`, quoted values, empty
values, and comments, but deliberately performs no shell command execution or
variable expansion.

## Named inference clients

Optional top-level `[[clients]]` entries map distinct API-key environment
variables to stable, content-free observer identities:

```toml
[[clients]]
id = "claude-code"
title = "Claude Code"
api_key_env = "INFERNAUT_CLAUDE_CODE_KEY"
# recipe = "claude-lab" # optional key-bound harness dispatch
# recipes = ["hermes-private"] # optional additive, session-selectable plans
```

This works across laptops, desktops, and changing source addresses because
identity follows the presented key rather than an IP mapping. Values remain in
the environment and are never stored in TOML. Every enabled client key must be
non-empty and distinct from the administrative server key and every other
client key.

Named keys are intentionally limited to `/v1` catalog and inference endpoints.
Continue using `server.api_key_env` for the configurator's administrative
operations, the live observer, metrics, probes, and diagnostic catalogs. Named
clients can be authored directly in TOML or managed on the guided TUI's
**Clients** screen. A client may either be bound exclusively to one route
recipe or authorized to select one or more additive recipes; clients without
either retain the complete ordinary model and route catalog.

Use `Tab` and `Shift-Tab` to move through the following screens.

## Clients

The Clients screen manages stable inference-client identities without storing
credential values in TOML:

- `a`: add a named client
- `e`: edit the selected client ID, display title, and API-key environment name
- `d`: enable or disable the selected client
- `x`: stage removal of the selected client
- `Tab` / `Shift-Tab`: move between fields in the client form
- `Enter`: stage the client
- `Esc`: cancel

The client form's **Exclusive recipe** field cycles with left/right. An
exclusively bound key receives only that recipe's virtual slots from
`/v1/models` and cannot bypass the recipe by requesting a physical model
directly. The **Selectable recipes** field cycles candidates with left/right
and toggles each with Space. Selectable recipes augment the ordinary catalog
and are useful for choosing different Hermes plans per session. Choosing one
binding mode clears the other.

New clients receive editable `new-client` and
`INFERNAUT_NEW_CLIENT_KEY` defaults. The table reads the current process
environment—including the automatically loaded `.env`—only to report `set`,
`missing`, `empty`, `duplicate key`, or `not required`; it never displays or
writes the secret value. Different
environment-variable names that resolve to the same key are reported as
duplicates because Infernaut rejects that ambiguity at startup.

All edits remain staged until **Review & Save** performs normal validation,
backup, and atomic replacement. Removing a client from the routing
configuration does not delete or modify its operating-system environment
variable.

## Recipes

The Recipes screen builds opt-in harness dispatch plans. The Claude Code
template separates primary, background, subagent, auto-mode classifier, and
compression traffic. The Hermes template provides a session-selectable primary
plan plus narrow internal delegation, vision-analysis, and compression
dispatch. Every unbound client remains unchanged.

- `a`: add a Claude Code recipe template
- `h`: add a Hermes recipe template
- `e`: edit the selected recipe ID and display title; ID changes also update
  exclusive and selectable client bindings
- `j` / `k`: select a recipe
- `n` / `p`: select a slot within that recipe
- `Enter`: open a searchable discovered-model and route target picker
- `[` / `]`: quick-cycle the selected target
- `o`: cycle the selected slot's output maximum
- `t`: cycle the selected OpenAI-compatible slot's reasoning policy
- `l` / `c` / `b` / `i`: allow local, cloud, both, or inherit realm policy
- `d`: disable an unbound recipe
- `x`: remove an unbound recipe

Targets use durable canonical provider IDs. Temporarily missing targets remain
configured and are reported by the doctor. Existing adaptive routes can be
selected as targets when changing provider membership should remain automatic.
The primary slot in a new Hermes recipe inherits the reasoning effort sent by
the harness. Delegation, vision, and compression default to `none`, since
focused auxiliary work generally should not inherit an interactive session's
thinking budget. Existing recipes retain `inherit` until changed. When a target
reports granular reasoning options, the primary recipe model republishes them;
an explicit slot policy remains authoritative when a harness falls back to a
different effort for an unfamiliar virtual model.
See [Route recipes](ROUTE_RECIPES.md) for runtime behavior, Hermes setup, and
Claude Code launcher settings.

## Aliases

The Aliases screen creates stable exact model pointers for cron jobs,
background automation, and harness settings that should not be edited whenever
infrastructure changes:

- `a`: add an alias with an editable ID and display title
- `e`: edit the selected alias
- `Enter` or `Space`: choose one physical model, non-audio route, or selectable
  recipe slot target
- `[` / `]`: cycle inherited, 32K, 64K, 128K, 200K, 256K, and 1M context
- `d`: enable or disable the selected alias
- `x`: remove it

An alias is published as `alias:<id>` under colon-qualified IDs. It performs no
classification, scoring, or fallback. The configured target is either selected
or the request fails closed. The optional context value is an admission and
target-capacity contract; it does not change provider runtime settings. Recipe
targets remain visible and usable only to client keys authorized for that
recipe, and only selectable model-triggered slots may be aliased. See
[Exact model aliases](MODEL_ALIASES.md) for TOML, privacy, and failure semantics.

## Providers

The provider wizard edits the provider ID, kind, title, base URL, realm, public
alias, API-key environment variable name, and priority. It never asks for or
stores the secret itself. `r` reruns read-only model discovery and reports
provider connectivity alongside the number of models found. The provider table
shows the public alias in its own column; `—` means the provider ID remains the
published namespace.

- `a`: add a provider
- `e`: edit the selected provider
- `i`: toggle JIT loading (and turn explicit auto-load off)
- `o`: toggle explicit auto-load (and turn JIT loading off)
- `d`: enable or disable the selected provider
- `r`: refresh discovery
- `Tab` / `Shift-Tab`: move between fields in the provider form
- `Left` / `Right`: choose the provider kind
- `Enter`: stage the provider
- `Esc`: cancel

LM Studio, llama.cpp, and vLLM presets provide their usual local URLs. Cloud
provider presets retain Infernaut's existing API-key and OAuth behavior.

### Static OpenAI Audio providers

Audio runtimes commonly expose fixed STT or TTS endpoints without implementing
`GET /v1/models`. Add a combined service from the Providers screen by choosing
`openai_audio`. The guided form has two independent lanes:

- `STT model ID`, `STT upstream`, and `STT route ID`, defaulting to
  `private/transcription`.
- `TTS model ID`, `TTS upstream`, and `TTS route ID`, defaulting to
  `private/speech`.

For a single-model audio server, use a stable STT identity such as
`nemotron-3.5-asr` and `tts-1` for TTS. Upstream IDs may be the same values even
when the service ignores them. Pressing Enter atomically stages the provider,
both static models, exclusive non-adaptive `transcription` and `speech`
profiles, and two local first-available routes. No discovery, voice lookup, or
audio request occurs in the form. Review the generated TOML on Review & Save.
The Simulator identifies both as endpoint-driven routes and shows the exact
provider/model resolution without task classification.

Audio service models deliberately do not appear in `/v1/models`, cannot be
selected by chat routes, and cannot share a route with LLMs or with the other
audio service type. STT is addressed through `POST /v1/audio/transcriptions`;
TTS uses `POST /v1/audio/speech` or `/v1/audio/speech/stream`.
`GET /v1/audio/voices` follows the uniquely configured TTS route. The normal
global privacy lock and per-request `x-infernaut-privacy: local_only`
constraint apply to all four endpoints. The legacy `openai_transcription`
provider kind remains supported for an STT-only configuration.

## Models and provider catalog changes

Every discovered model is reconciled against routing profiles using its stable
canonical `provider/model` ID and one of four states:

- `Exact`: an exact canonical model profile exists.
- `Wildcard`: a deliberate wildcard profile currently manages the model.
- `Unclassified`: the provider added a model that has no routing profile.
- `Dormant`: a profile was retained even though the provider no longer reports
  that model.

This is intentionally non-destructive. A temporary provider outage or removed
cloud model does not erase tuning. If the same canonical model returns, its
profile becomes active again. A new model does not silently inherit a quality
claim unless it matches a wildcard the operator explicitly created.

The running server compares every refreshed catalog with the previous snapshot.
It logs added and removed canonical IDs and warns when newly added models are
unclassified. Lists are capped in the log record to avoid a provider-wide
catalog replacement flooding the terminal; counts always describe the complete
change. Run `config doctor` or open the configurator to inspect the full set.

The Models screen uses columns to keep discovery state, the canonical model ID,
detected context, load state, adaptive-routing eligibility, applied roles, and
suggested roles separate. Context is displayed compactly (`68K`, `128K`,
`256K`, or `1M`); `?` means discovery did not provide an honest limit. When a
loaded runtime configuration is lower than the model's supported maximum, the
column shows effective/supported values such as `68K/256K`. The `Applied roles`
column describes the current in-memory
configuration; the global `saved` or `modified` indicator at the bottom tells
you whether that state has been written to disk. A trailing `*` means the role
preset was subsequently customized in the Ranking screen.

The configurator presents a conservative, name-and-capability-based role
suggestion for each discovered model. Suggestions do not change configuration
until accepted.

- `g`: accept the suggestion for the selected model
- `Space`: reserve the selected model for direct physical-ID use, or make it
  eligible for adaptive routing again
- `[` / `]`: cycle the selected model's context override through automatic,
  32K, 64K, 128K, 200K, 256K, and 1M
- `1`: fast generalist
- `2`: small code inspector
- `3`: balanced
- `4`: heavy development + data
- `5`: toggle the additive vision role
- `6`: toggle the additive Compression role
- `x`: remove an exact profile

A `*` in the Context column marks an operator override. For example,
`256K*/128K` means Infernaut will treat the model as 256K even though the
provider reported 128K. `256K*` means the provider did not report a usable
value. The override is an admission and publication correction; it does not
change the context actually allocated by the upstream runtime. Clearing the
override returns authority to provider discovery.

Fast, Inspect, Balanced, and Heavy are mutually exclusive workload presets.
Vision and Compression are additive, so a front-end or multimodal coding model
can be `Heavy + Vision`, while a lightweight screenshot helper can be
`Fast + Vision`. The Heavy preset combines architecture, substantial
implementation, migration, debugging, SQL, and analytical strengths; it
replaces the former separate heavy-code and data presets.

Roles are saved as explicit profile metadata:

```toml
[[routing.models]]
model = "vllm/multimodal-developer"
context_length = 262144
roles = ["heavy", "vision"]
```

The workload role generates the initial strengths and traits. Fine-grained
changes in the Ranking screen remain authoritative and are shown with `*`;
roles never overwrite later manual tuning unless you deliberately apply
another preset. On an image-bearing request, a configured Vision role receives
a routing preference after normal privacy, context, and provider-capability
checks. A manual Vision role does not fabricate protocol support: a provider
that explicitly reports vision as unsupported is still rejected.

Provider-reported protocol capabilities and operator-defined quality remain
separate. Discovery can establish facts such as vision support or context size;
it cannot honestly establish that a model is a strong architect.

### Reserving compression and utility models

A model can remain published and directly addressable while being excluded
from adaptive routing:

```toml
[[routing.models]]
model = "lmstudio/compression-model"
auto_routing = false
roles = ["fast", "compression"]
```

This is useful for a permanently loaded Hermes compression model, embedding or
summarization utility model, or any model reserved for a dedicated workflow.
The model remains in `/v1/models`, and a request naming its physical
`provider/model` ID continues normally. Ordinary adaptive requests remove it
before scoring, context checks, loaded-model bonuses, and explicit preference
rules; the routing explanation records
`auto_routing_disabled(profile=lmstudio/compression-model)`.

When Hermes is configured with a direct compression-model override, that
override must name the physical model ID rather than a `route/...` virtual
model. A Hermes recipe removes that limitation: its hidden compression slot
may target either a physical model or an adaptive route. The reservation may
also be placed on a deliberate wildcard profile to reserve an entire model
family.

The additive `compression` role provides a narrow exception for detected
Claude Code compaction and recipe-confirmed Hermes compression. When an
adaptive route receives one of those requests and at least one enabled
compression-role model exists inside that route's realm and candidate pool:

- only compression-role models remain eligible;
- a compression-role model with `auto_routing = false` becomes eligible for
  that request only;
- privacy, route context, actual model context, provider availability, and
  protocol capability gates still apply;
- the compaction selection is not retained as the route's tool-loop session
  pin.

Generic requests to summarize text do not activate this path. If no applicable
model carries the role, routing remains backward compatible and uses the normal
pool. The included **Claude Code compaction** simulator scenario exercises that
detector without contacting a provider; Hermes detection is scoped to an
active `kind = "hermes"` recipe.

## Ranking

The ranking matrix exposes the workload scores and operational preferences used
by adaptive routing. Selecting an unprofiled model and changing a value first
creates a balanced exact profile.

- `h` / `l`: choose a workload column
- `-` / `+`: decrease or increase its strength by 0.05
- `t` / `T`: decrease or increase the latency tier
- `c` / `C`: decrease or increase the cost tier

Strength values stay between `0.0` and `1.0`; latency and cost tiers stay
between `0` and `4`.

## Routes and privacy

The Routes screen edits the selected route's major routing constraints:

- `m`: cycle routing through `off`, `shadow`, and `active`
- `w`: cycle loaded-model preference through quality-first, balanced, and
  warm-first
- `l`: allow local providers only
- `c`: allow cloud providers only
- `b`: allow both local and cloud providers
- `t`: cycle fixed task
- `e`: cycle fixed effort
- `a`: toggle `first_available` and `adaptive`
- `[` / `]`: cycle the selected route context contract through none, 32K, 64K,
  128K, 200K, 256K, and 1M
- `i`: cycle the selected route's input estimator through the conservative
  default and explicit 3.0, 3.25, 3.5, 3.75, and 4.0 bytes per token
- `o`: cycle the maximum output for detected compression requests through the
  harness maximum, 4K, 8K, 16K, 24K, and 32K

The contract modifies the selected existing route; it does not create
additional context-specific route IDs. The `Contract` column is the stable
window advertised by the virtual route. The
`Eligible` column shows how many current candidate models satisfy it. A larger
physical model may serve a smaller route contract; a smaller or unknown-context
model may not. `none` retains catalog-derived compatibility behavior and is a
valid intentional setting, so Doctor does not warn merely because a route is
uncontracted. Explicit contracts are recommended for routes used by stateful
agent harnesses.

`B/token` controls the tokenizer-independent admission estimate for the
selected route. `3.00*` is the default. Higher values estimate fewer input
tokens and should be selected only when provider tokenizer evidence supports
them. By default Infernaut reserves the request's complete `max_tokens` value
after estimating input. `Comp max` is an explicit exception for recognized
Claude Code compaction requests. It limits both admission's output reservation
and a larger `max_tokens` value forwarded to the provider. Ordinary inference
is unchanged.

The Privacy screen uses `p` to cycle the global privacy lock. Privacy remains
fail-closed: an empty eligible set produces an error rather than silently
broadening a route or falling back to cloud.

## Simulator

The simulator runs the real in-process classifier, eligibility filters, model
profiles, rules, and scorer against representative prompts. It makes no request
to a provider and performs no model inference.

- `Up` / `Down`: choose a scenario
- `[` / `]`: choose a virtual route
- `m`: compare routing modes
- `w`: compare loaded-model preference policies

It displays task, operation, effort, confidence, the route context contract,
matched signals, the selected model, candidate scores, rejection reasons, and
tool-loop state. Context exclusions are shown as
`context_below_route_contract` or `context_unknown`. The included
tool scenarios distinguish an object-shaped optional `auto`
choice from an explicitly required Anthropic `any` choice. This makes tool
capability exclusions inspectable without executing a tool or contacting a
model. For a custom prompt or an automation-friendly result, use:

```bash
cargo run --release -- routing explain route:private:auto \
  --prompt "Architect and implement a new authentication subsystem" \
  --config infernaut.toml
```

The result is JSON and uses current provider discovery, but still performs no
inference.

## Review, validation, and save

The final screen shows the staged TOML diff and current doctor findings. Press
`s` to validate and save. Saving:

1. renders and parses the complete generated TOML;
2. runs the server's semantic configuration validation;
3. creates a timestamped sibling backup such as
   `infernaut.toml.bak-1785123456`;
4. writes and syncs a temporary sibling file;
5. atomically replaces the configured file.

The editor preserves comments and formatting in untouched TOML sections. A
section changed through a structured editor may be normalized. Restart the
Infernaut process after saving; live configuration reload is not implied.

The same checks are available without the TUI:

```bash
cargo run --release -- config validate infernaut.toml
cargo run --release -- config doctor infernaut.toml
cargo run --release -- config doctor infernaut.toml --json
```

`doctor` checks configuration, provider discovery, credential environment
variable presence, model reconciliation, missing adaptive-route contracts, and
whether current model pools can satisfy configured context contracts. Its JSON
form is suitable for scripts, CI, or an external monitoring system.

## Recommended operating loop

1. Run `config validate` before every start.
2. Open `configure` after adding a provider or when `doctor` reports catalog
   drift.
3. Classify new models deliberately; leave uncertain models unclassified.
4. Keep removed models dormant until it is clear they will not return.
5. Exercise representative prompts in the simulator while routing is in
   `shadow`.
6. Review passive routing logs and adjust strengths.
7. Enable `active` only after the choices are predictable.
