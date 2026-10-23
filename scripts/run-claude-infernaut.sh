#!/usr/bin/env bash
set -euo pipefail

# Claude Code through Infernaut.
#
# Required:
#   export INFERNAUT_CLAUDE_CODE_KEY="your-named-client-key"
#   (falls back to INFERNAUT_API_KEY for backward compatibility)
#
# Optional overrides:
#   export INFERNAUT_BASE_URL="http://127.0.0.1:4040"
#   export INFERNAUT_CLAUDE_MODEL="route:private:auto"
#   export INFERNAUT_CLAUDE_RECIPE="claude-code"
#   export INFERNAUT_CLAUDE_ENABLE_1M_CONTEXT=1
#
# Any arguments supplied to this script are passed through to Claude Code.

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/.." && pwd)"
env_file="${INFERNAUT_ENV_FILE:-${repo_root}/.env}"

if [[ -f "${env_file}" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "${env_file}"
  set +a
fi

base_url="${INFERNAUT_BASE_URL:-http://127.0.0.1:4040}"
model="${INFERNAUT_CLAUDE_MODEL:-route:private:auto}"
recipe="${INFERNAUT_CLAUDE_RECIPE:-}"
gateway_key="${INFERNAUT_CLAUDE_CODE_KEY:-${INFERNAUT_API_KEY:-}}"

if [[ -n "${recipe}" ]]; then
  primary_model="recipe:${recipe}:primary"
  background_model="recipe:${recipe}:background"
  subagent_model="recipe:${recipe}:subagent"
else
  primary_model="${model}"
  background_model="${model}"
  subagent_model="${model}"
fi

if [[ -z "${gateway_key}" ]]; then
  printf '%s\n' \
    "Neither INFERNAUT_CLAUDE_CODE_KEY nor INFERNAUT_API_KEY is set." \
    "Export the named Claude client key (preferred) or the administrative server key." >&2
  exit 1
fi

if ! command -v claude >/dev/null 2>&1; then
  printf '%s\n' "Claude Code was not found in PATH." >&2
  exit 1
fi

if [[ "${INFERNAUT_CLAUDE_ENABLE_1M_CONTEXT:-0}" == "1" ]]; then
  if ! command -v curl >/dev/null 2>&1; then
    printf '%s\n' \
      "curl is required to verify Infernaut's advertised Claude context variants." >&2
    exit 1
  fi
  if ! recipe_catalog="$(
    curl --fail --silent --show-error \
      --header "Authorization: Bearer ${gateway_key}" \
      "${base_url%/}/v1/models"
  )"; then
    printf '%s\n' \
      "Could not read Infernaut's recipe model catalog; refusing to guess 1M support." >&2
    exit 1
  fi

  advertised_1m_variant() {
    local base_model="$1"
    local slash_model="${base_model//:/\/}"
    if grep -Fq "\"id\":\"${base_model}[1m]\"" <<<"${recipe_catalog}" \
      || grep -Fq "\"id\":\"${slash_model}[1m]\"" <<<"${recipe_catalog}"; then
      printf '%s[1m]' "${base_model}"
    else
      printf '%s' "${base_model}"
    fi
  }

  primary_model="$(advertised_1m_variant "${primary_model}")"
  background_model="$(advertised_1m_variant "${background_model}")"
  subagent_model="$(advertised_1m_variant "${subagent_model}")"
fi

# Claude Code should use the gateway bearer token exclusively. A separately
# configured Anthropic API key can otherwise take precedence over this route.
unset ANTHROPIC_API_KEY

export ANTHROPIC_BASE_URL="${base_url}"
export ANTHROPIC_AUTH_TOKEN="${gateway_key}"
export ANTHROPIC_MODEL="${primary_model}"
export ANTHROPIC_DEFAULT_OPUS_MODEL="${primary_model}"
export ANTHROPIC_DEFAULT_SONNET_MODEL="${primary_model}"
export ANTHROPIC_DEFAULT_HAIKU_MODEL="${background_model}"
export CLAUDE_CODE_SUBAGENT_MODEL="${subagent_model}"
if [[ "${INFERNAUT_CLAUDE_ENABLE_1M_CONTEXT:-0}" == "1" ]]; then
  unset CLAUDE_CODE_DISABLE_1M_CONTEXT
  if [[ "${primary_model}" == *'[1m]' ]]; then
    context_label="1M verified by Infernaut"
  else
    context_label="200K standard (primary target is below or missing verified 1M context)"
  fi
else
  export CLAUDE_CODE_DISABLE_1M_CONTEXT=1
  context_label="200K gateway default"
fi
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1

printf 'Starting Claude Code through Infernaut\n'
printf '  endpoint: %s\n' "${ANTHROPIC_BASE_URL}"
printf '  primary:  %s\n' "${primary_model}"
printf '  background: %s\n' "${background_model}"
printf '  subagent: %s\n' "${subagent_model}"
if [[ -n "${recipe}" ]]; then
  printf '  classifier: recipe:%s:classifier (signature-dispatched)\n' "${recipe}"
  printf '  compact:  recipe:%s:compression (signature-dispatched)\n' "${recipe}"
fi
printf '  context:  %s\n' "${context_label}"

exec claude --model "${primary_model}" "$@"
