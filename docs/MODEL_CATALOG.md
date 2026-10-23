# Model catalog and IDs

Infernaut publishes one deterministic catalog that OpenAI- and
Anthropic-compatible clients consume. This page collects the ID, ordering, and
per-provider discovery details that previously lived in the README.

An upstream model named `qwen/qwen3-14b` discovered from LM Studio is published as:

```text
lmstudio/qwen/qwen3-14b
```

Only the first slash separates the provider from the upstream model ID, so
upstream IDs may retain their own namespaces.

## Public ID format

`server.model_id_format` controls only the public IDs returned by `/v1/models`:

- `slash` publishes canonical IDs such as `lmstudio/qwen/qwen3-14b`.
- `colon_qualified` publishes `lmstudio:qwen:qwen3-14b` using literal colons. This keeps the provider visible in Hermes Telegram and Discord pickers, which display only the final slash-delimited component, without exposing URL-encoded punctuation such as `%3A`.

Infernaut accepts both the published ID and its canonical slash ID for inference. Diagnostic catalogs and resolved-model response headers retain the canonical ID.

## Catalog ordering

`server.model_order` controls the deterministic alphabetical order returned by `/v1/models`:

- `provider_then_model` sorts by visible provider alias/ID, then stable provider model ID.
- `model_then_provider` sorts by stable provider model ID, then visible provider alias/ID.

Routes participate under the `route` namespace, and exact pointers participate under the `alias` namespace. Catalog ordering does not change routing priority.

## Provider aliases

Each provider may define a short, unique provider `alias`. It replaces only that provider's namespace in published model IDs; configuration lookup, routing, privacy checks, metrics, and upstream requests continue using `id`. For example, with `model_id_format = "colon_qualified"`, `id = "chatgpt-codex"` and `alias = "cgpt"`, the picker publishes `cgpt:gpt-5.5` while still accepting `chatgpt-codex/gpt-5.5`. Provider aliases may contain letters, numbers, `-`, or `_`; `route`, `recipe`, and `alias` are reserved namespaces, and provider aliases may not collide with another provider ID or alias. Exact model pointers use the separate top-level `[[aliases]]` feature documented in [Exact model aliases](MODEL_ALIASES.md).

## Stable physical model IDs

Physical model IDs are derived from the stable provider catalog identity, not from a
transient loaded-instance alias. They therefore remain unchanged when a model is loaded,
evicted, or reloaded. Infernaut tracks `provider_model_id`, `load_id`, and
`inference_id` separately in each model's `x-infernaut` metadata. Previously published
loaded-instance IDs remain accepted as compatibility inputs while that alias is discoverable.

## Provider discovery metadata

### LM Studio

`kind = "lm_studio"` uses LM Studio's native `/api/v1/models` management endpoint for
discovery, so downloaded but currently unloaded LLMs appear in Infernaut's catalog.
Inference still uses the configured OpenAI-compatible `/v1` base URL.

For LM Studio, `provider_metadata.max_context_length` reports the effective context of the
loaded instance when one exists, while `maximum_context_length` retains the model's supported
upper bound. `configured_context_length` and `configured_parallel` expose the active load
configuration without conflating it with model capability.

LM Studio activation has three modes:

- `jit_load = true`: pass an unloaded model's native key to the inference endpoint, allowing LM Studio's JIT limits and auto-eviction policy to operate.
- `auto_load = true`: explicitly call LM Studio's native load endpoint and target the returned instance ID. Explicit loads may not participate in JIT auto-eviction.
- Both false: return `model_not_loaded` rather than changing LM Studio state.

`jit_load` and `auto_load` are mutually exclusive. An optional `load_context_length` applies to explicit loading; when omitted, LM Studio chooses its default. Other providers default to `kind = "openai_compatible"` and discover models from `{base_url}/models`.

### vLLM

For vLLM, Infernaut discovers IDs from `/v1/models` and enriches them from
`/server_info?config_format=json`. The live `model_config.max_model_len` becomes
`provider_metadata.max_context_length` and `maximum_context_length`; Infernaut does not
guess from the model family. If server info is unavailable and the model list does not carry a
context field, the limit remains unknown rather than silently becoming 128K or 1M.

### Ollama Cloud

Ollama Cloud discovery retains its OpenAI-compatible model list and enriches each model from
Ollama's native `/api/show` metadata. If any provider still omits or misreports a context
window, set `context_length` on its `[[routing.models]]` profile or use `[` and `]` on the
configurator's Models screen. This operator value is authoritative for publication and routing
admission but does not reconfigure the upstream runtime.

## Catalog contract

The public model catalog is a backward-compatible OpenAI/Anthropic superset. Physical models
publish Anthropic `type`, `display_name`, `max_input_tokens`, conservative capability fields,
and `max_tokens` only when the provider reports an output ceiling, alongside the OpenAI fields
and `x-infernaut` metadata. A route with `context_length` publishes that stable contract and
admits only models whose detected effective context meets it. An uncontracted adaptive route
conservatively publishes the smallest eligible window only when every limit is known;
deterministic routes publish the selected model's window. The same model record is available
from `GET /v1/models/{model_id}`.

Adaptive Anthropic routes can also specialize Claude Code compaction without
changing Claude Code itself. An additive `compression` model role forms an
explicit allowlist only when Infernaut recognizes Claude Code's compound
compaction prompt; a tagged `auto_routing = false` utility model remains
reserved for every ordinary request. Privacy, context, capability, and provider
gates continue to apply. Routes may also set
`compression_max_output_tokens` to replace Claude Code's broad output allowance
with an explicit compaction limit for both admission and provider forwarding;
ordinary requests are unaffected.

Claude Code gateway discovery currently uses model IDs and display names for
its picker but does not consume arbitrary context lengths from `/v1/models`.
Use a 200,000-token route contract with
`CLAUDE_CODE_DISABLE_1M_CONTEXT=1` when routing Claude Code through local
256K-or-larger models. See
[Anthropic compatibility](ANTHROPIC_COMPATIBILITY.md).

## Include and exclude filters

`include_models` and `exclude_models` accept exact IDs or `*` globs and are applied before
catalog publication. This is especially useful for OpenAI's account-wide model catalog, which
can include embeddings, audio, image, and legacy models that should not appear in a chat
picker. Filters are always evaluated against the upstream model ID, before the `provider/`
prefix is added.
