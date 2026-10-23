#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

cargo build --quiet --bins
rm -rf target/sniffer-e2e-output

python3 tests/mock_provider.py &
mock_pid=$!
INFERNAUT_API_KEY=e2e-secret \
  E2E_HERMES_KEY=hermes-e2e-secret \
  cargo run --quiet -- tests/e2e.toml &
router_pid=$!
cloud_router_pid=""
trap 'kill "$router_pid" ${cloud_router_pid:-} "$mock_pid" 2>/dev/null || true' EXIT

for _ in {1..50}; do
  if curl --silent --fail --max-time 1 http://127.0.0.1:14040/health >/dev/null; then
    break
  fi
  sleep 0.1
done

[[ "$(curl --silent --head --output /dev/null --write-out '%{http_code}' \
  http://127.0.0.1:14040/api/hello)" == "200" ]]
[[ "$(curl --silent --output /dev/null --write-out '%{http_code}' \
  http://127.0.0.1:14040/api/hello)" == "401" ]]

INFERNAUT_API_KEY=e2e-secret \
  target/debug/infernaut config validate tests/e2e.toml >/dev/null
doctor=$(INFERNAUT_API_KEY=e2e-secret \
  target/debug/infernaut config doctor tests/e2e.toml --json)
[[ "$(jq -r '.providers[] | select(.id == "mock") | .available' <<<"$doctor")" == "true" ]]
[[ "$(jq '[.models[] | select(.state == "unclassified")] | length' <<<"$doctor")" -ge 1 ]]
routing_explanation=$(INFERNAUT_API_KEY=e2e-secret \
  target/debug/infernaut routing explain route/private/auto \
  --prompt "Architect and implement a new authentication component across the codebase." \
  --config tests/e2e.toml)
[[ "$(jq -r '.classification.task' <<<"$routing_explanation")" == "code" ]]
[[ "$(jq -r '.selected_model' <<<"$routing_explanation")" == "mock/org/beta" ]]

INFERNAUT_API_KEY=e2e-secret \
  OPENAI_E2E_API_KEY=openai-e2e-secret \
  XAI_E2E_API_KEY=xai-e2e-secret \
  OLLAMA_E2E_API_KEY=ollama-e2e-secret \
  target/debug/infernaut tests/cloud-e2e.toml &
cloud_router_pid=$!
for _ in {1..50}; do
  if curl --silent --fail --max-time 1 http://127.0.0.1:14041/health >/dev/null; then
    break
  fi
  sleep 0.1
done

cloud_models=$(curl --silent --fail http://127.0.0.1:14041/v1/models \
  -H 'Authorization: Bearer e2e-secret')
[[ "$cloud_models" == *'"id":"openai/alpha"'* ]]
[[ "$cloud_models" != *'"id":"openai/org/beta"'* ]]
[[ "$cloud_models" == *'"id":"xai/grok-4.3"'* ]]
[[ "$cloud_models" == *'"id":"xai/grok-latest"'* ]]
[[ "$cloud_models" == *'"id":"ollama-cloud/ollama-cloud-model"'* ]]
[[ "$cloud_models" == *'"id":"route/cloud"'* ]]

openai_response=$(curl --silent --fail \
  http://127.0.0.1:14041/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"openai/alpha","messages":[{"role":"user","content":"hello"}]}')
[[ "$openai_response" == *'"model":"alpha"'* ]]

xai_response=$(curl --silent --fail \
  http://127.0.0.1:14041/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"xai/grok-latest","messages":[{"role":"user","content":"hello"}]}')
[[ "$xai_response" == *'"model":"grok-latest"'* ]]

ollama_response=$(curl --silent --fail \
  http://127.0.0.1:14041/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"ollama-cloud/ollama-cloud-model","messages":[{"role":"user","content":"hello"}]}')
[[ "$ollama_response" == *'"model":"ollama-cloud-model"'* ]]

unsupported_messages=$(curl --silent \
  --write-out $'\n%{http_code}' \
  http://127.0.0.1:14041/v1/messages \
  -H 'x-api-key: e2e-secret' \
  -H 'anthropic-version: 2023-06-01' \
  -H 'Content-Type: application/json' \
  --data '{"model":"openai/alpha","max_tokens":16,"messages":[{"role":"user","content":"hello"}]}')
unsupported_messages_status="${unsupported_messages##*$'\n'}"
unsupported_messages_body="${unsupported_messages%$'\n'*}"
[[ "$unsupported_messages_status" == "400" ]]
[[ "$unsupported_messages_body" == *'"type":"error"'* ]]
[[ "$unsupported_messages_body" == *'"code":"unsupported_endpoint"'* ]]

privacy_status=$(curl --silent --output /dev/null --write-out '%{http_code}' \
  http://127.0.0.1:14041/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'x-infernaut-privacy: local_only' \
  -H 'Content-Type: application/json' \
  --data '{"model":"openai/alpha","messages":[{"role":"user","content":"private"}]}')
[[ "$privacy_status" == "403" ]]

# A provider redirect must be rejected, never replaying the private body.
redirect_status=$(curl --silent --output /dev/null --write-out '%{http_code}' \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'x-infernaut-privacy: local_only' \
  -H 'Content-Type: application/json' \
  --data '{"model":"mock/org/beta","messages":[{"role":"user","content":"redirect-test"}]}')
[[ "$redirect_status" == "502" ]]

models=$(curl --silent --fail http://127.0.0.1:14040/v1/models \
  -H 'Authorization: Bearer e2e-secret')
[[ "$models" == *'"id":"mock/alpha"'* ]]
[[ "$models" == *'"id":"mock/compressor"'* ]]
[[ "$models" == *'"id":"mock/org/beta"'* ]]
[[ "$models" == *'"id":"llamacpp/llama-alpha"'* ]]
[[ "$models" == *'"id":"llamacpp/org/llama-beta"'* ]]
[[ "$(jq -r '.data[] | select(.id == "vllm/vllm-only") | .max_input_tokens' <<<"$models")" == "262144" ]]
[[ "$(jq -r '.data[] | select(.id == "route/vllm-test") | .max_input_tokens' <<<"$models")" == "262144" ]]
[[ "$(jq -r '.data[] | select(.id == "route/private/auto") | .max_input_tokens' <<<"$models")" == "32000" ]]
[[ "$models" == *'"id":"route/private"'* ]]
[[ "$models" == *'"id":"route/private/auto"'* ]]
[[ "$models" != *'forbidden-cloud'* ]]
[[ "$models" != *'nemotron-parakeet'* ]]
[[ "$models" != *'tts-1'* ]]
[[ "$models" != *'private/transcription'* ]]
[[ "$models" != *'private/speech'* ]]

transcription=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/audio/transcriptions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'x-infernaut-privacy: local_only' \
  -F 'file=@/dev/null;filename=sample.wav;type=audio/wav' \
  -F 'model=route/private/transcription')
[[ "$(jq -r '.text' <<<"$transcription")" == "mock transcription" ]]

voices=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/audio/voices \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'x-infernaut-privacy: local_only')
[[ "$(jq -r '.data[0].id' <<<"$voices")" == "tara" ]]

speech_wav=$(mktemp)
curl --silent --fail \
  http://127.0.0.1:14040/v1/audio/speech \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'x-infernaut-privacy: local_only' \
  -H 'Content-Type: application/json' \
  --data '{"model":"route/private/speech","input":"Hello from the e2e suite.","voice":"tara"}' \
  --output "$speech_wav"
grep -a -q '^RIFFMOCKWAVE$' "$speech_wav"

speech_stream_wav=$(mktemp)
curl --silent --fail \
  http://127.0.0.1:14040/v1/audio/speech/stream \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'x-infernaut-privacy: local_only' \
  -H 'Content-Type: application/json' \
  --data '{"model":"route/private/speech","input":"Stream this.","voice":"tara"}' \
  --output "$speech_stream_wav"
grep -a -q '^RIFFMOCKWAVE$' "$speech_stream_wav"

anthropic_model=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/models/route/vllm-test \
  -H 'x-api-key: e2e-secret' \
  -H 'anthropic-version: 2023-06-01')
[[ "$(jq -r '.max_input_tokens' <<<"$anthropic_model")" == "262144" ]]
[[ "$(jq -r '.display_name' <<<"$anthropic_model")" == "vLLM Test" ]]
[[ "$(jq -r '.type' <<<"$anthropic_model")" == "model" ]]

anthropic_response=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/messages \
  -H 'x-api-key: e2e-secret' \
  -H 'anthropic-version: 2023-06-01' \
  -H 'Content-Type: application/json' \
  --data '{"model":"route/private","system":"Keep replies short.","max_tokens":16,"messages":[{"role":"user","content":"hello"}]}')
[[ "$anthropic_response" == *'"type":"message"'* ]]
[[ "$anthropic_response" == *'"model":"alpha-loaded"'* ]]
[[ "$anthropic_response" == *'"type":"text","text":"ok"'* ]]

anthropic_count_headers=$(mktemp)
anthropic_count=$(curl --silent --fail --dump-header "$anthropic_count_headers" \
  http://127.0.0.1:14040/v1/messages/count_tokens \
  -H 'x-api-key: e2e-secret' \
  -H 'anthropic-version: 2023-06-01' \
  -H 'Content-Type: application/json' \
  --data '{"model":"route/private","system":"Keep replies short.","messages":[{"role":"user","content":"hello"}]}')
[[ "$(jq -r '.input_tokens' <<<"$anthropic_count")" -gt 0 ]]
tr -d '\r' < "$anthropic_count_headers" | grep -q '^x-infernaut-resolved-model: mock/alpha$'

anthropic_stream=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/messages \
  -H 'x-api-key: e2e-secret' \
  -H 'anthropic-version: 2023-06-01' \
  -H 'Content-Type: application/json' \
  --data '{"model":"route/private","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"hello"}]}')
[[ "$anthropic_stream" == *'event: message_start'* ]]
[[ "$anthropic_stream" == *'"model": "alpha-loaded"'* ]]
[[ "$anthropic_stream" == *'event: message_stop'* ]]

headers=$(mktemp)
response=$(curl --silent --fail --dump-header "$headers" \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"route/private","messages":[{"role":"user","content":"hello"}]}')

[[ "$response" == *'"model":"alpha-loaded"'* ]]
tr -d '\r' < "$headers" | grep -q '^x-infernaut-requested-model: route/private$'
tr -d '\r' < "$headers" | grep -q '^x-infernaut-resolved-model: mock/alpha$'
tr -d '\r' < "$headers" | grep -q '^x-infernaut-request-id: inf_'

light_routed=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"route/private/auto","messages":[{"role":"user","content":"Read src/config.rs and explain what the validate function does."}]}')
[[ "$light_routed" == *'"model":"alpha-loaded"'* ]]

compression_direct=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"mock/compressor","messages":[{"role":"user","content":"compress this"}]}')
[[ "$compression_direct" == *'"model":"compression-loaded"'* ]]

heavy_routed=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"route/private/auto","messages":[{"role":"user","content":"Architect and implement a new authentication component across the codebase with integration tests and concurrency controls."}]}')
[[ "$heavy_routed" == *'"model":"org/beta"'* ]]

named_client_response=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer hermes-e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"route/private/auto","messages":[{"role":"user","content":"hello"}]}')
[[ "$named_client_response" == *'"model":"alpha-loaded"'* ]]
named_client_models=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/models \
  -H 'Authorization: Bearer hermes-e2e-secret')
[[ "$named_client_models" == *'"id":"mock/alpha"'* ]]
[[ "$named_client_models" == *'"id":"recipe/hermes-private/primary"'* ]]
[[ "$named_client_models" != *'recipe/hermes-private/compression'* ]]
[[ "$named_client_models" != *'recipe/hermes-private/delegation'* ]]
[[ "$(curl --silent --output /dev/null --write-out '%{http_code}' \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer hermes-e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"recipe/hermes-private/compression","messages":[{"role":"user","content":"hello"}]}')" == "403" ]]
[[ "$(curl --silent --output /dev/null --write-out '%{http_code}' \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer hermes-e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"recipe/hermes-private/delegation","messages":[{"role":"user","content":"hello"}]}')" == "403" ]]
hermes_recipe_response=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer hermes-e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"recipe/hermes-private/primary","messages":[{"role":"user","content":"hello"}]}')
[[ "$hermes_recipe_response" == *'"model":"alpha-loaded"'* ]]
hermes_delegation_response=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer hermes-e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"recipe/hermes-private/primary","messages":[{"role":"system","content":"You are a focused subagent working on a specific delegated task.\n\nYOUR TASK:\nInspect the router.\n\nComplete this task using the tools available to you. Your response is returned to the parent agent as a summary."},{"role":"user","content":"Inspect the router."}]}')
[[ "$hermes_delegation_response" == *'"model":"org/beta"'* ]]
hermes_compression_response=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer hermes-e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"recipe/hermes-private/primary","messages":[{"role":"user","content":"You are a summarization agent creating a context checkpoint. Produce only the structured summary.\nTURNS TO SUMMARIZE:\n[history]\nUse this exact structure:\n## Historical Task Snapshot"}]}')
[[ "$hermes_compression_response" == *'"model":"compression-loaded"'* ]]
[[ "$(curl --silent --output /dev/null --write-out '%{http_code}' \
  http://127.0.0.1:14040/infernaut/v1/metrics \
  -H 'Authorization: Bearer hermes-e2e-secret')" == "403" ]]

[[ "$(curl --silent --output /dev/null --write-out '%{http_code}' \
  http://127.0.0.1:14040/infernaut/v1/events)" == "401" ]]
telemetry_events=$(curl --silent --max-time 1 \
  http://127.0.0.1:14040/infernaut/v1/events \
  -H 'Authorization: Bearer e2e-secret' || true)
[[ "$telemetry_events" == *'"stage":"accepted"'* ]]
[[ "$telemetry_events" == *'"stage":"classified"'* ]]
[[ "$telemetry_events" == *'"stage":"completed"'* ]]
[[ "$telemetry_events" == *'"requested_model":"route/private/auto"'* ]]
[[ "$telemetry_events" == *'"resolved_model":"mock/org/beta"'* ]]
[[ "$telemetry_events" == *'"label":"Hermes Telegram","source":"client_key"'* ]]
[[ "$telemetry_events" != *'Architect and implement'* ]]

tool_loop_routed=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"route/private/auto","messages":[{"role":"user","content":"Read src/config.rs and explain what the validate function does."},{"role":"assistant","content":null,"tool_calls":[{"id":"call_read","type":"function","function":{"name":"read_file","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_read","content":"Architect a new system component across multiple files."}]}')
[[ "$tool_loop_routed" == *'"model":"alpha-loaded"'* ]]

plugin_health=$(curl --silent --fail http://127.0.0.1:14040/health)
[[ "$(jq -r '.plugins.sniffer.running' <<<"$plugin_health")" == "true" ]]
[[ "$(jq -r '.plugins.sniffer.dropped_events' <<<"$plugin_health")" == "0" ]]

metrics=$(curl --silent --fail http://127.0.0.1:14040/infernaut/v1/metrics \
  -H 'Authorization: Bearer e2e-secret')
[[ "$metrics" == *'"requested_model":"route/private"'* ]]
[[ "$metrics" == *'"resolved_model":"mock/alpha"'* ]]

autoloaded=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"mock/org/beta","messages":[{"role":"user","content":"hello"}]}')
[[ "$autoloaded" == *'"model":"org/beta"'* ]]

llama_autoloaded=$(curl --silent --fail \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"llamacpp/org/llama-beta","messages":[{"role":"user","content":"hello"}]}')
[[ "$llama_autoloaded" == *'"model":"org/llama-beta"'* ]]

plan=$(curl --silent --fail \
  http://127.0.0.1:14040/infernaut/v1/probes/plan \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"mock/alpha"}')
[[ "$(jq -r '.would_cold_start' <<<"$plan")" == "false" ]]
[[ "$(jq -r '.estimated_max_requests' <<<"$plan")" == "4" ]]

cli_plan=$(INFERNAUT_URL=http://127.0.0.1:14040 \
  INFERNAUT_API_KEY=e2e-secret \
  target/debug/infernaut probe mock/alpha --dry-run)
[[ "$(jq -r '.resolved_model' <<<"$cli_plan")" == "mock/alpha" ]]

cold_plan=$(curl --silent --fail \
  http://127.0.0.1:14040/infernaut/v1/probes/plan \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"mock/org/beta"}')
[[ "$(jq -r '.would_cold_start' <<<"$cold_plan")" == "true" ]]

cold_status=$(curl --silent --output /dev/null --write-out '%{http_code}' \
  http://127.0.0.1:14040/infernaut/v1/probes \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"mock/org/beta"}')
[[ "$cold_status" == "409" ]]

probe=$(curl --silent --fail \
  http://127.0.0.1:14040/infernaut/v1/probes \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"mock/alpha"}')
probe_id=$(jq -r '.id' <<<"$probe")
for _ in {1..50}; do
  probe=$(curl --silent --fail \
    "http://127.0.0.1:14040/infernaut/v1/probes/$probe_id" \
    -H 'Authorization: Bearer e2e-secret')
  [[ "$(jq -r '.status' <<<"$probe")" == "completed" ]] && break
  sleep 0.1
done
[[ "$(jq -r '.status' <<<"$probe")" == "completed" ]]
[[ "$(jq '[.results[] | select(.passed == true)] | length' <<<"$probe")" == "4" ]]

cached=$(curl --silent --fail \
  http://127.0.0.1:14040/infernaut/v1/probes \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"mock/alpha"}')
[[ "$(jq -r '.cached' <<<"$cached")" == "true" ]]

slow_probe=$(curl --silent --fail \
  http://127.0.0.1:14040/infernaut/v1/probes \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"mock/slow"}')
slow_id=$(jq -r '.id' <<<"$slow_probe")
for _ in {1..30}; do
  slow_state=$(curl --silent --fail \
    "http://127.0.0.1:14040/infernaut/v1/probes/$slow_id" \
    -H 'Authorization: Bearer e2e-secret')
  [[ "$(jq -r '.status' <<<"$slow_state")" == "running" ]] && break
  sleep 0.05
done
[[ "$(jq -r '.status' <<<"$slow_state")" == "running" ]]
curl --silent --fail --request DELETE \
  "http://127.0.0.1:14040/infernaut/v1/probes/$slow_id" \
  -H 'Authorization: Bearer e2e-secret' >/dev/null
for _ in {1..30}; do
  cancelled=$(curl --silent --fail \
    "http://127.0.0.1:14040/infernaut/v1/probes/$slow_id" \
    -H 'Authorization: Bearer e2e-secret')
  [[ "$(jq -r '.status' <<<"$cancelled")" == "cancelled" ]] && break
  sleep 0.1
done
[[ "$(jq -r '.status' <<<"$cancelled")" == "cancelled" ]]

preempt_probe=$(curl --silent --fail \
  http://127.0.0.1:14040/infernaut/v1/probes \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"mock/slow"}')
preempt_id=$(jq -r '.id' <<<"$preempt_probe")
for _ in {1..30}; do
  preempt_state=$(curl --silent --fail \
    "http://127.0.0.1:14040/infernaut/v1/probes/$preempt_id" \
    -H 'Authorization: Bearer e2e-secret')
  [[ "$(jq -r '.status' <<<"$preempt_state")" == "running" ]] && break
  sleep 0.05
done
[[ "$(jq -r '.status' <<<"$preempt_state")" == "running" ]]
curl --silent --fail \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"mock/alpha","messages":[{"role":"user","content":"priority request"}]}' \
  >/dev/null
for _ in {1..30}; do
  preempted=$(curl --silent --fail \
    "http://127.0.0.1:14040/infernaut/v1/probes/$preempt_id" \
    -H 'Authorization: Bearer e2e-secret')
  [[ "$(jq -r '.status' <<<"$preempted")" == "preempted" ]] && break
  sleep 0.05
done
[[ "$(jq -r '.status' <<<"$preempted")" == "preempted" ]]

curl --silent --fail \
  http://127.0.0.1:14040/v1/chat/completions \
  -H 'Authorization: Bearer e2e-secret' \
  -H 'Content-Type: application/json' \
  --data '{"model":"mock/alpha","messages":[{"role":"user","content":"INFERNAUT_E2E_SIGNATURE"}]}' \
  >/dev/null
for _ in {1..30}; do
  if grep -q '"id":"e2e-signature"' target/sniffer-e2e-output/findings-*.jsonl 2>/dev/null; then
    break
  fi
  sleep 0.05
done
grep -q '"id":"e2e-signature"' target/sniffer-e2e-output/findings-*.jsonl

echo "Infernaut end-to-end test passed"
