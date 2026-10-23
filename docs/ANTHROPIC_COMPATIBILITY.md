# Anthropic Messages compatibility

Infernaut exposes `POST /v1/messages` for clients that speak the Anthropic
Messages API. The endpoint is a native pass-through: Infernaut does not
convert Anthropic requests to OpenAI Chat Completions or Responses.

It also exposes `POST /v1/messages/count_tokens` for Anthropic gateway clients
such as Claude Code. vLLM and llama.cpp requests use their native token-counting
endpoints. LM Studio currently lacks that endpoint, so Infernaut returns a
documented conservative estimate without loading a model or sending prompt
content upstream.

## Provider configuration

LM Studio and llama.cpp are known native providers and enable Messages
automatically:

```toml
[[providers]]
id = "lmstudio"
kind = "lm_studio"
base_url = "http://127.0.0.1:1234/v1"
realm = "local"
jit_load = true

[[providers]]
id = "llamacpp"
kind = "llama_cpp"
base_url = "http://127.0.0.1:8080/v1"
realm = "local"
jit_load = true
```

vLLM has a first-class provider kind and enables Messages automatically:

```toml
[[providers]]
id = "vllm"
kind = "vllm"
title = "vLLM"
base_url = "http://127.0.0.1:8000/v1"
realm = "local"
priority = 30
```

Ollama Cloud also exposes native Anthropic Messages and enables it
automatically:

```toml
[[providers]]
id = "ollama-cloud"
kind = "ollama_cloud"
realm = "cloud"
api_key_env = "OLLAMA_API_KEY"
```

Other generic OpenAI-compatible providers are conservative by default because
an OpenAI `/v1/models` endpoint does not prove that `/v1/messages` exists. Such
providers may set `anthropic_compat = true` only when the same server natively
implements Anthropic Messages.

`anthropic_compat = false` can explicitly disable Messages for a native
provider. Adaptive routes exclude incompatible providers before model scoring;
direct physical IDs and deterministic routes are checked after resolution but
before JIT activation or upstream network traffic. The diagnostic catalog
publishes this provider declaration as the per-model
`capabilities.anthropic_messages` field.

## Client setup

Use the Infernaut server root as an Anthropic SDK base URL. SDKs append
`/v1/messages` themselves:

```python
import os
from anthropic import Anthropic

client = Anthropic(
    base_url="http://127.0.0.1:4040",
    api_key=os.environ["INFERNAUT_API_KEY"],
)

message = client.messages.create(
    model="lmstudio/qwen3.5-9b",
    max_tokens=256,
    messages=[{"role": "user", "content": "Hello"}],
)
print(message.content)
```

### Claude Code

Use one Infernaut credential mechanism. `ANTHROPIC_AUTH_TOKEN` is the
recommended gateway form and becomes an `Authorization: Bearer` header:

```bash
env -u ANTHROPIC_API_KEY \
  ANTHROPIC_BASE_URL="http://127.0.0.1:4040" \
  ANTHROPIC_AUTH_TOKEN="$INFERNAUT_API_KEY" \
  ANTHROPIC_DEFAULT_OPUS_MODEL="route:private:auto" \
  ANTHROPIC_DEFAULT_SONNET_MODEL="route:private:auto" \
  ANTHROPIC_DEFAULT_HAIKU_MODEL="route:private:auto" \
  CLAUDE_CODE_DISABLE_1M_CONTEXT=1 \
  CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1 \
  claude --model "route:private:auto"
```

The repository also includes a launcher containing the complete environment
and command:

```bash
export INFERNAUT_API_KEY="replace-with-your-infernaut-key"
./scripts/run-claude-infernaut.sh
```

It defaults to `http://127.0.0.1:4040` and `route:private:auto`. Override
either value without editing the script:

```bash
INFERNAUT_BASE_URL="http://127.0.0.1:4040" \
INFERNAUT_CLAUDE_MODEL="route:private:code" \
./scripts/run-claude-infernaut.sh
```

Arguments are passed through to Claude Code, for example:

```bash
./scripts/run-claude-infernaut.sh --continue
```

For different primary, background, subagent, auto-mode classifier, and compression
targets, bind a Claude Code route recipe to the named client key and launch it
with:

```bash
INFERNAUT_CLAUDE_RECIPE="claude-lab" \
./scripts/run-claude-infernaut.sh
```

The launcher maps Claude's main aliases to `recipe:claude-lab:primary`, its
Haiku/background alias to `recipe:claude-lab:background`, and
`CLAUDE_CODE_SUBAGENT_MODEL` to `recipe:claude-lab:subagent`. Recognized
compaction requests are redirected internally to the recipe's compression
slot. See [Route recipes](ROUTE_RECIPES.md) for the TUI and TOML setup.
Set `INFERNAUT_CLAUDE_ENABLE_1M_CONTEXT=1` to let the launcher select Claude's
`[1m]` variant only for recipe slots that Infernaut verifies at 1,000,000 input
tokens or greater. Smaller and unknown targets remain on Claude's conservative
standard 200K gateway behavior. The catalog decision uses the effective detected
context, operator override, or route contract rather than trusting a model name.

Claude Code skips gateway `GET /v1/models` discovery unless
`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` is set. Discovery populates its
model picker, but Claude Code 2.1.x retains only discovered model IDs and
display names; it does not consume Infernaut's `max_input_tokens`. It also
filters picker entries whose IDs do not begin with `claude` or `anthropic`.
Pinned Infernaut route IDs continue to work for inference, but discovery
cannot communicate an arbitrary route context window to Claude Code.

Claude Code budgets gateway and third-party models at 200K by default or 1M
when its extended-context behavior is active. For a Infernaut route backed
by 256K-or-larger models, use `CLAUDE_CODE_DISABLE_1M_CONTEXT=1` and configure a
matching route contract:

```toml
[[routes]]
id = "private/auto"
strategy = "adaptive"
allowed_realms = ["local"]
context_length = 200000
```

This makes Claude Code compact against its 200K budget while Infernaut admits
only physical models that can safely satisfy it. A route with models below
200K cannot be represented exactly by Claude Code's current gateway model
metadata; Infernaut still enforces its configured contract and rejects an
oversized request before provider contact. Only omit
`CLAUDE_CODE_DISABLE_1M_CONTEXT` for a route that genuinely contracts for 1M.

When the same route should retain its full physical context for Hermes,
OpenCode, or another harness, leave the route contract at that full capacity
and set Claude Code's own lower compaction window instead:

```bash
CLAUDE_CODE_AUTO_COMPACT_WINDOW=200000 \
CLAUDE_CODE_DISABLE_1M_CONTEXT=1 \
./scripts/run-claude-infernaut.sh
```

This setting is local to the Claude Code process and does not reduce the route
available to other clients. Some Claude Code builds contain the setting but do
not expose the `/autocompact` command, so the environment variable is the
portable launcher control.

For physical models, `max_input_tokens` is the provider-reported effective
context limit. vLLM uses the live `max_model_len` from
`/server_info?config_format=json`. A route with `context_length` reports that
stable contract and excludes physical models below it or with unknown context.
An uncontracted adaptive route reports the smallest eligible window only when
every eligible model has a known limit; a deterministic route reports its
selected model's window. Unknown limits are omitted rather than guessed.

Claude Code also sends an unauthenticated `HEAD /api/hello` connectivity
preflight. Infernaut answers this narrowly scoped endpoint without exposing
catalog or health details; inference and model discovery remain authenticated.

### Dedicated Claude Code compaction models

Claude Code uses its active model for `/compact` and automatic compaction; it
does not currently expose a separate compaction-model override. A key-bound
Claude recipe is the most explicit solution: it redirects only the recognized
compound compaction request to its configured compression slot. The earlier
adaptive-route mechanism remains available for clients without a recipe and
can select an operator-tagged model:

```toml
[[routing.models]]
model = "agamotto/qwen2.5-14b-instruct-1m"
auto_routing = false
roles = ["fast", "compression"]
```

The reserved model remains unavailable to ordinary adaptive requests. For a
recognized compaction request, compression-role models form an explicit
allowlist and still must satisfy the virtual route's context contract and the
actual serialized request size. Use the Models screen's `6` key to toggle the
role and the Simulator's **Claude Code compaction** scenario to verify selection
without consuming the live session.

Infernaut conservatively estimates tokenizer-independent input at three
serialized bytes per token and then reserves the complete requested output.
For a measured Claude Code workload whose provider tokenizer reports a safer
ratio, tune only the route used by Claude:

```toml
[[routes]]
id = "private/auto"
strategy = "adaptive"
allowed_realms = ["local"]
token_estimate_bytes_per_token = 3.5
compression_max_output_tokens = 16384
```

This does not enlarge or misreport a physical model's context window. It only
changes preflight estimation, so use it with observed tokenizer evidence. The
Routes screen displays the ratio in `B/token` and cycles it with `i`.
The `Comp max` column displays the detected-compression output limit and `o`
cycles it. This limit applies only to recognized Claude Code compaction
requests, both for context admission and the forwarded `max_tokens`.

Do not use a dummy credential when `server.api_key_env` is configured.
Infernaut validates the supplied value against that environment variable.
Quoting the model ID is safe but not required; colons are valid shell variable
content.

Infernaut accepts its configured local key as either:

- `x-api-key: <INFERNAUT_API_KEY>`
- `Authorization: Bearer <INFERNAUT_API_KEY>`

The client credential is consumed at the Infernaut boundary and is never
forwarded. `anthropic-version`, `anthropic-beta`, user-agent, and other
non-hop-by-hop protocol headers are preserved. If a provider has
`api_key_env`, Infernaut applies that provider credential independently.

## Routing and response behavior

Physical model IDs and virtual routes use the same catalog as OpenAI requests:

```json
{
  "model": "route/private/auto",
  "system": "You are a careful coding assistant.",
  "max_tokens": 1024,
  "messages": [
    {"role": "user", "content": "Inspect this function and explain the bug."}
  ]
}
```

The existing privacy lock, realm constraints, deterministic routes, and
adaptive routes all apply. Adaptive classification understands top-level
Anthropic `system` content and treats a user turn containing `tool_result` as
an active tool loop for model pinning.

Infernaut replaces `model` with the resolved upstream model ID and forwards
the remaining JSON unchanged. Buffered Anthropic response objects are returned
unchanged. Streaming responses preserve `message_start`,
`content_block_start`, `content_block_delta`, `content_block_stop`,
`message_delta`, and `message_stop` SSE events. Routing headers, content-free
metrics, request logs, diagnostics, and enabled observer plugins behave exactly
as they do for OpenAI-compatible inference.

## Failure behavior and current boundary

- An adaptive route rejects providers without native Messages support before
  scoring. A direct physical ID or deterministic route that resolves to one
  returns an Anthropic-shaped `invalid_request_error` before model loading or
  upstream traffic.
- Privacy violations and Infernaut validation failures also use an
  Anthropic-shaped error envelope on this endpoint.
- If at least one route candidate is otherwise eligible but the request plus
  its output reservation exceeds that model's context ceiling, Infernaut
  returns HTTP 400 `invalid_request_error` with the recognizable
  `Prompt is too long` signal. Claude Code can then invoke reactive compaction
  instead of retrying a generic HTTP 503.
- There is no implicit provider fallback, retry, or protocol translation.
- Native token counts are provider-specific. LM Studio and generic compatible
  providers use a tokenizer-independent estimate of the complete request
  payload at three bytes per token. The estimate intentionally favors context
  safety over billing-grade precision.
- Direct Anthropic cloud API configuration is not yet a first-class provider.

These boundaries keep compatibility honest: enabling an OpenAI-compatible
provider alone never claims that its Anthropic surface exists.
