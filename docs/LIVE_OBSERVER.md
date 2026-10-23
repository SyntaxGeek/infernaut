# Infernaut Mission Control

Infernaut Mission Control is Infernaut's separate, read-only terminal UI for
watching inference traffic move from clients through routing decisions to
provider models. The space motif is a presentation layer over precise lifecycle
telemetry; it does not rename API fields or replace technical status labels.

Start the router normally in one terminal:

```bash
cargo run --release -- infernaut.toml
```

Attach the observer in another:

```bash
cargo run --release -- observe infernaut.toml
```

Start with animation disabled when preferred:

```bash
cargo run --release -- observe infernaut.toml --reduced-motion
```

Enable the optional 8-bit lifecycle soundscape at startup:

```bash
cargo run --release -- observe infernaut.toml --sound
```

Or use the launcher that loads the same ignored `.env` file as the server:

```bash
./scripts/run-infernaut-observe.sh
```

The observer derives its URL from `server.listen` and reads the same
`server.api_key_env` environment variable as the router. For a router on
another host, override only the connection URL:

```bash
cargo run --release -- observe infernaut.toml \
  --url http://127.0.0.1:4040
```

The launcher accepts the configuration path and URL as its first and second
arguments, and also honors `INFERNAUT_CONFIG`, `INFERNAUT_URL`, and
`INFERNAUT_ENV_FILE`. Set `INFERNAUT_OBSERVER_SOUND=1` to start its observer
with sound enabled.

The observer has no configuration mutation API. It cannot add providers,
change model profiles, edit routes, load models, or run inference. Continue to
use `infernaut configure` for intentional configuration changes.

## What appears

The graph is based exclusively on observed inference traffic. It never expands
the full provider model catalog.

- Clients appear on the left after sending a request.
- Infernaut remains at the center.
- Providers and resolved models appear on the right only after receiving
  traffic in the selected scope.
- Request objects use a size vocabulary derived from serialized request bytes.
- Compression requests use a magenta compression capsule.
- Local traffic is green, cloud traffic is blue, activation is amber, and
  failures are red.
- Streaming paths remain active until the upstream stream completes or the
client disconnects.

Mission Control uses terminal-safe text symbols rather than emoji:

| Symbol | Operational meaning |
| --- | --- |
| `›` | Ordinary outbound request probe |
| `◇` | Medium request payload |
| `◆` | Large request payload of at least 512 KiB |
| `◈` | Detected compression request |
| `✦` | Active streamed response |
| `●` | Completed request |
| `✹` | Provider or routing failure |
| `×` | Client disconnect or timeout |

The symbols supplement the exact lifecycle stage shown in the timeline. They
do not infer agent behavior, model residency, or generated-token content.
Graph labels are width-bounded and use an ellipsis before reaching a node
border. Full client, route, provider-qualified model, and request details remain
available in the scope picker and selected timeline detail.

Animation follows lifecycle state rather than looping every nonterminal
request. `forwarding` travels to the selected provider once and then waits
there for headers. `headers` marks provider acceptance/prefill. Return traffic
animates only after the first upstream body bytes establish `streaming`.
Dropping the client before headers or during a stream produces a terminal
`disconnected` event instead of leaving a permanently animated forwarding
path.

Provider/model status is deliberately evidence-based:

- **Serving** means an active request is being processed successfully.
- **Recent** means the destination successfully participated in recent
  traffic.
- **Error** means the most recent visible path failed.

The observer does not label a model permanently loaded merely because a
provider such as vLLM served a request. Successful traffic proves that it was
serving at that moment; it does not prove current residency after the request.

## Scopes and controls

Press `s` or `/` to choose from values observed during the current router
process:

- all traffic;
- a specific virtual route;
- a specific provider;
- a specific client;
- compression requests only;
- failures only.

Other controls:

| Key | Action |
| --- | --- |
| `q` | Exit the observer |
| `Space` | Pause or resume ingestion |
| `a` | Return to all traffic |
| `j` / `k` | Inspect newer or older requests |
| `m` | Toggle reduced motion |
| `8` | Toggle optional 8-bit lifecycle audio |

`--reduced-motion` selects the same static lifecycle positioning at startup.
The `m` key can still toggle animation during the session.

## Optional 8-bit audio

Mission Control can map lifecycle transitions to short, procedurally generated
square-wave cues:

| Cue | Meaning |
| --- | --- |
| rising launch chirp | Request accepted from a client |
| high navigation blip | Route classified |
| low stepping tone | Provider model activation |
| four-note warp cue | Compression request detected |
| return chirp | First streamed response bytes |
| ascending confirmation | Request completed |
| noise burst and low descent | Provider or routing failure |
| broken descending tone | Client disconnect or timeout |

Audio is disabled by default. Pass `--sound` or press `8` to enable it, and
press `8` again to mute it without leaving the observer. Mission Control
synthesizes small PCM wave files at startup and plays them on a bounded
background worker through `afplay`, PipeWire, PulseAudio, or ALSA when one is
available. The footer reports `unavailable` when no supported system player is
present.

The queue is bounded and duplicate cues are rate-limited. Audio is also
suppressed while reconnect history is ingested and after resuming from pause,
so old telemetry does not replay as a burst. Sound generation and playback are
observer-local: neither executes in Infernaut's inference process nor adds
work to a provider request.

The lower timeline shows request size, classification, resolved model, stage,
HTTP status, context-budget estimate, activation time, time to headers, time to
first and last streamed bytes, total duration, and a sanitized failure summary.
The gap between last byte and disconnect helps distinguish initial prefill
timeouts from mid-stream stalls. `Wire in` uses KiB/MiB of serialized request
JSON; `Budget` is the conservative estimated token requirement including the
requested output reserve. It does not show prompt or response content.

## Client identity

Client names use the following precedence:

1. a named inference client key;
2. `X-Infernaut-Client`, when explicitly supplied;
3. a recognized `User-Agent`, including Hermes, Claude Code, Pi, OpenCode,
   OpenClaw, and Codex;
4. the source IP address;
5. `unknown`.

Named keys provide stable identity when the same harness runs from different
devices:

```toml
[[clients]]
id = "claude-code"
title = "Claude Code"
api_key_env = "INFERNAUT_CLAUDE_CODE_KEY"

[[clients]]
id = "hermes"
title = "Hermes Telegram"
api_key_env = "INFERNAUT_HERMES_KEY"
```

```bash
export INFERNAUT_CLAUDE_CODE_KEY="a-distinct-long-random-value"
export INFERNAUT_HERMES_KEY="another-distinct-long-random-value"
```

Give each harness the corresponding value instead of the administrative
`INFERNAUT_API_KEY`. Named client keys can use `/v1` model and inference
endpoints but receive `403 client_key_scope_denied` from administrative
`/infernaut/v1/*` endpoints. The observer itself continues using the
administrative server key. `scripts/run-claude-infernaut.sh` automatically
loads the same ignored `.env`, prefers `INFERNAUT_CLAUDE_CODE_KEY`, and falls back
to the administrative key for backward compatibility.

Clients that support custom headers can provide a stable display label:

```text
X-Infernaut-Client: Hermes Telegram
```

The header is consumed by Infernaut and is not forwarded upstream.

## Runtime and privacy behavior

The authenticated `GET /infernaut/v1/events` endpoint uses Server-Sent
Events. The process retains at most 1,024 content-free lifecycle events and
broadcasts through a bounded live channel. Publication never waits for an
observer. A slow observer may miss events, but it cannot delay classification,
provider activation, inference, or streaming.

Lifecycle records contain request identity, client label and network metadata,
requested and resolved model IDs, route classification, byte counts, estimated
tokens, timing, status, and sanitized error details. They contain no prompt,
tool schema, response, credential, or generated-token content.

The history is process-local and disappears when Infernaut exits. Durable
retention belongs in an explicitly configured observer plugin or external
logging system.
