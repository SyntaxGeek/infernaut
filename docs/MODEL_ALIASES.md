# Exact model aliases

Exact model aliases are stable, operator-owned pointers for harness features
that need a durable model ID. A cron scheduler, background job, or integration
can remain pinned to `alias:cron` while the physical provider/model mapping is
changed centrally in Infernaut.

Aliases deliberately do less than routes:

- one alias has exactly one target;
- no prompt classification, scoring, load preference, or fallback is applied;
- a missing, disabled, privacy-denied, or undersized target fails closed;
- the original alias ID and resolved physical model remain visible in logs,
  metrics, response headers, and Mission Control;
- changing an alias requires saving the configuration and restarting the
  gateway, but does not require changing or restarting a quiet harness.

## Configuration

```toml
[[aliases]]
id = "cron"
title = "Cron automation"
target = "agamotto/google/gemma-4-26b-a4b-qat"
context_length = 262144 # optional
```

With `server.model_id_format = "colon_qualified"`, this is published through
`GET /v1/models` as `alias:cron`. Slash publication uses `alias/cron` instead.
The target always uses the canonical configured identity, not a provider's
display alias.

`target` may be either:

- one physical `provider/model` ID; or
- one existing non-audio `route/...` ID; or
- one selectable, model-triggered `recipe/<recipe>/<slot>` ID.

Pointing at a route is useful when the stable harness-facing name should survive
changes to a route's membership. It does not add a second classifier or fallback
layer: the selected route retains its own declared strategy and policies.
Aliases cannot point to other aliases, which prevents chains and cycles.
Internal classifier, delegation, vision, and compression slots cannot be
selected as alias targets. Point to the recipe's primary/selectable slot and
the recipe will retain its own narrow internal dispatch behavior.

Recipe aliases preserve authorization. They appear only in catalogs returned
to named client keys that are exclusively bound to or explicitly authorized
for the target recipe. An unauthorized client receives
`recipe_scope_denied`; the alias cannot bypass a recipe's key boundary.

`context_length` is an optional public contract. When omitted, the alias
advertises the exact target's detected context. When set, the target must report
at least that capacity, route candidates below it are excluded, and a request
whose estimated input plus reserved output exceeds it is rejected before
provider contact. The setting does not reconfigure the upstream runtime.

`disabled = true` retains the durable mapping in TOML while removing it from
the catalog and resolution namespace.

## Guided configuration

Open the configurator and select **Aliases**:

```bash
cargo run --release -- configure infernaut.toml
```

- `a`: add an alias
- `e`: edit its ID and display title
- `Enter` or `Space`: choose its one exact physical, route, or recipe target
  with a searchable picker
- `[` / `]`: cycle the context contract through inherited and fixed tiers
- `d`: enable or disable it
- `x`: remove it

The table distinguishes the configured contract from the context actually
advertised. `config doctor` reports unavailable targets, unknown target context,
and contracts that exceed a target's reported capacity.

## Privacy and failure behavior

Aliases never weaken Infernaut's existing policy boundaries. A global
`local_only` lock or request-level `x-infernaut-privacy: local_only` control
still denies a cloud target. If the exact target disappears, the request returns
an error with `fallback_attempted = false`; another provider or model is never
selected merely to make the request succeed.

For workload-based selection use an adaptive `route:...`. For harness-function
dispatch use a `recipe:...`. Use `alias:...` when the intent is “this stable
name means that exact model, route, or authorized recipe entry until I
deliberately change it.”
