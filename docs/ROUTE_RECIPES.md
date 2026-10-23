# Route recipes

## Transport limits

Output max is a generation budget, not the model context window. The ChatGPT
Codex subscription adapter currently omits output caps because its upstream
transport rejects them: recipe caps are **not enforced generation ceilings on
that transport**. Reasoning effort is forwarded as Responses `reasoning.effort`;
supported values still depend on the selected upstream model.
Use a provider that honors output limits when a generation ceiling is required.

Adaptive session pins are isolated by named client key (or peer IP for shared
administrative keys). Clients may send `x-infernaut-session-id` to isolate
concurrent sessions further. Without that header, initial conversation text is
a heuristic identity; identical openings from one client can still collide.
Pins expire and are capped at 4,096 entries; eviction can require fresh routing.

Route recipes are an optional, key-bound dispatch layer for harnesses that use
different model aliases or recognizable request shapes for internal jobs. They
do not replace physical model IDs or adaptive routes. A client without a recipe
behaves exactly as it did before recipes were configured.

Recipes have two client-binding modes:

- `recipe = "..."` is exclusive. The key sees only that recipe's selectable
  slots and cannot request ordinary models or routes. This is the Claude Code
  contract.
- `recipes = ["...", "..."]` is additive. The key retains the ordinary
  catalog and gains one selectable entry per recipe. This lets Hermes select a
  routing plan per session with `/model` while keeping the same profile and
  memory.

The first built-in template targets Claude Code:

| Slot | Virtual model | Purpose |
| --- | --- | --- |
| Primary | `recipe:<id>:primary` | Interactive main-agent turns |
| Background | `recipe:<id>:background` | Claude's Haiku/background model alias |
| Subagent | `recipe:<id>:subagent` | All Claude Code subagents |
| Classifier | `recipe:<id>:classifier` | Internal target for recognized auto-mode safety classification |
| Compression | `recipe:<id>:compression` | Internal target for recognized Claude compaction requests |

Each slot can target one canonical `provider/model` ID or an existing
`route/...` ID. A recipe therefore composes with adaptive routing instead of
duplicating it.

## Guided setup

Run the configurator:

```bash
cargo run --release -- configure infernaut.toml
```

1. Open **Recipes** and press `a` to add a Claude Code template or `h` to add a
   Hermes template.
2. Use `j`/`k` to select a recipe. Press `e` to edit its durable ID and display
   title; renaming the ID updates every bound client reference in the staged
   configuration. Use `n`/`p` to select a slot. Press `Enter`
   to open the searchable model/route picker; type to filter, use up/down, and
   press `Enter` again to assign the target. `[`/`]` remain quick-cycle keys.
   Use `l`, `c`, `b`, or `i` for
   local, cloud, both, or inherited realm policy; `o` cycles a slot's output
   maximum. For OpenAI-compatible recipes, `t` cycles the slot reasoning
   policy through `inherit`, `none`, and the supported effort levels.
   `inherit` preserves the harness request; an explicit effort is enforced
   after internal slot dispatch.
3. Open **Clients** and retain a dedicated API-key environment variable. Use
   **Exclusive recipe** for Claude Code. For Hermes, move to **Selectable
   recipes**, choose a recipe with left/right, and press Space to toggle it;
   repeat to authorize multiple recipes on the same key.
4. Review and save. The doctor warns when a target is temporarily unavailable
   and when a background, classifier, subagent, or compressor has less reported context than the
   primary target.

Model targets are stored by canonical provider ID, not a display alias. They
remain configured if a provider is offline or removes a model, making catalog
churn visible instead of silently remapping a job. Point a slot to an adaptive
route when automatic membership changes are desirable.

## Manual TOML

```toml
[[clients]]
id = "claude-code"
title = "Claude Code"
api_key_env = "INFERNAUT_CLAUDE_CODE_KEY"
recipe = "claude-lab"

[[recipes]]
id = "claude-lab"
title = "Claude Code lab"
kind = "claude_code"
protocol = "anthropic"
allowed_realms = ["local"]

[[recipes.slots]]
id = "primary"
title = "Primary"
target = "agamotto/qwen3.6-27b-nvfp4"

[[recipes.slots]]
id = "background"
title = "Background / Haiku"
target = "agamotto/qwen3.6-35b-a3b-mtp"

[[recipes.slots]]
id = "subagent"
title = "Subagent"
target = "agamotto/qwen3.5-122b-a10b"

[[recipes.slots]]
id = "classifier"
title = "Classifier"
target = "agamotto/qwen2.5-7b-instruct"
trigger = "classifier"
max_output_tokens = 1024

[[recipes.slots]]
id = "compression"
title = "Compression"
target = "agamotto/qwen3.6-27b-nvfp4"
trigger = "compression"
max_output_tokens = 16384
```

`allowed_realms` is an additional fail-closed boundary. It intersects an
existing route's realm policy and can never weaken the global privacy lock.
Omit it to inherit global and route policy.

Only the recipe's virtual slots are returned by `GET /v1/models` when the
exclusive `recipe` binding is used. Physical models, ordinary routes, and
other recipes are rejected before provider contact. An additive `recipes`
binding instead appends only each recipe's model-triggered slots to the normal
catalog; internal classifier, delegation, vision, and compression slots stay
hidden. Administrative keys and clients without recipes retain the ordinary
combined catalog.

## Claude Code launcher

```bash
INFERNAUT_CLAUDE_RECIPE="claude-lab" \
./scripts/run-claude-infernaut.sh
```

The launcher retains its conservative 200K gateway default. To allow extended
context for recipe slots whose resolved model or route contract genuinely reports at
least 1,000,000 input tokens, opt in explicitly:

```bash
INFERNAUT_CLAUDE_RECIPE="claude-lab" \
INFERNAUT_CLAUDE_ENABLE_1M_CONTEXT=1 \
./scripts/run-claude-infernaut.sh
```

Infernaut publishes a capable slot with Claude's `[1m]` variant suffix. The launcher
reads the authenticated recipe catalog and selects that suffix independently for the
primary, background, and subagent aliases. A slot below the threshold, with an unknown
context, or backed by a smaller route contract remains unsuffixed and therefore uses
Claude's standard 200K budget. Infernaut also rejects a manually forged `[1m]` request
when the slot target does not meet the threshold.

The doctor warns when a reported background, classifier, subagent, or compression context
is below the primary target. Internal classifier and compression dispatch still receive
their own direct context admission check; the operator remains responsible for ensuring
the compression target can accept a full-history compaction request.

The launcher sets the main Opus/Sonnet model to `primary`, the default
Haiku/background model to `background`, and `CLAUDE_CODE_SUBAGENT_MODEL` to
`subagent`. Claude Code does not currently expose environment variables for a
separate auto-mode classifier or compaction model. Inside a `claude_code`
recipe, Infernaut recognizes Claude's narrow auto-mode classifier wire
signature and redirects it to `classifier`; it similarly redirects Claude's
compound compaction instruction to `compression`. Ordinary permission
discussions, summaries, background calls, and subagent calls are not
redirected.

Classifier detection is fail-closed: it requires the `<transcript>` envelope
and Claude's exact block/severity response instruction. If the signature
changes in a future Claude Code release, the request remains on its explicitly
requested slot instead of being guessed into the classifier. Direct recipe
targets also receive a pre-provider context check, so an oversized request is
rejected locally rather than sent to a small classifier model.

This split is explicit:

- The client key selects the recipe.
- Claude's model alias selects primary, background, or subagent.
- The narrow classifier and compaction signatures select their internal slots.
- Existing route classification runs only when the selected slot targets an
  adaptive route.

Server logs include `recipe request dispatched` with the named client, recipe,
requested slot, effective slot, target, classifier state, and compression
state. This lets an operator verify classifier traffic even if every slot
initially targets the same physical model.

When an older Claude recipe is loaded, Infernaut stages a durable migration:
the existing `classifier` target remains the true classifier, a new
`background` slot inherits the subagent target (falling back to primary), and
the classifier output cap defaults to 1024 tokens. Open **Review & Save** in
the configurator to persist that migration.

The Claude controls represented by this template are documented in
[Claude Code model configuration](https://code.claude.com/docs/en/model-config),
[Claude Code subagents](https://code.claude.com/docs/en/sub-agents), and
[Claude Code permission modes](https://code.claude.com/docs/en/permission-modes).

## Other harnesses

### Hermes

Hermes recipes expose one selectable virtual model plus hidden delegation,
vision-analysis, and compression slots. A single client key can authorize
several recipes:

```toml
[[clients]]
id = "hermes"
title = "Hermes Telegram"
api_key_env = "INFERNAUT_HERMES_KEY"
recipes = ["hermes-private", "hermes-cloud"]

[[recipes]]
id = "hermes-private"
title = "Hermes private"
kind = "hermes"
protocol = "openai"
allowed_realms = ["local"]

[[recipes.slots]]
id = "primary"
title = "Private auto"
target = "route/private/auto"

[[recipes.slots]]
id = "delegation"
title = "Delegated worker"
target = "agamotto/qwen3.5-122b-a10b"
trigger = "delegation"
reasoning_effort = "none"

[[recipes.slots]]
id = "vision"
title = "Vision analysis"
target = "agamotto/vision-model"
trigger = "vision"
reasoning_effort = "none"

[[recipes.slots]]
id = "compression"
title = "Private compression"
target = "agamotto/long-context-compressor"
trigger = "compression"
max_output_tokens = 12000
reasoning_effort = "none"
```

With `model_id_format = "colon_qualified"`, Hermes sees
`recipe:hermes-private:primary` in `/model`; it does not see the internal
delegation, vision, or compression slots. Selecting that entry keeps the recipe
identity in the session's requested model. Normal turns go to `primary`. A
child created by Hermes `delegate_task`
goes to `delegation`, while narrowly recognized structured context-checkpoint
and incremental-merge prompts go to `compression`. Hermes' auxiliary
`vision_analyze` provider call goes to `vision`. An ordinary user request such
as “summarize this report,” and an ordinary main-agent turn that happens to
contain an image, remains on `primary`.

For per-session recipe selection, leave Hermes' `delegation.provider`,
`delegation.model`, and `delegation.base_url` overrides unset so children
inherit the active main custom endpoint and recipe model. An explicit Hermes
delegation override takes precedence and may bypass Infernaut or request a
physical model directly.

Configure Hermes compression to reuse its active main model so the auxiliary
request retains the selected recipe ID:

```yaml
auxiliary:
  compression:
    provider: main
    model: ""
```

Configure vision the same way; otherwise a Hermes-specific vision provider or
model override can bypass the recipe before Infernaut ever sees the request:

```yaml
auxiliary:
  vision:
    provider: main
    model: ""
```

If Hermes pins `auxiliary.compression` or `auxiliary.vision` to a physical model
or another endpoint, that call bypasses the recipe by design. Also review any
Hermes auxiliary fallback chain when the recipe's realm boundary is intended
to be a strict privacy boundary.

The detectors are intentionally fail-closed and recipe-scoped. Delegation
requires several markers from Hermes' focused child system contract; the raw
Hermes child session ID is not exposed in a standard provider request.
Compression follows the structured batch, iterative, and micro-compaction
prompt contracts. Vision detection requires Hermes' compact auxiliary shape:
one text-plus-data-image user message, no system prompt or tools, non-streaming,
and either the 2,000-token output cap used by `vision_analyze` or its absence
when Hermes' Chat Completions compatibility layer omits output caps for custom
providers. If Hermes changes a wire signature, the request stays on `primary`
and logs as an ordinary recipe dispatch instead of being guessed.

Older Hermes recipes are migrated in memory by adding missing `delegation` and
`vision` slots that initially inherit the primary target. Open **Review &
Save** to inspect their targets and persist the migration.

OpenCode, Pi, and other flexible clients do not need recipes merely to use
named keys or existing routes. Leave both client recipe fields unset unless a
stable harness-specific dispatch contract is implemented.
