#!/usr/bin/env bash
set -euo pipefail

# Attach Infernaut Mission Control to a running Infernaut process.
# This read-only observer cannot mutate or restart the service.
# The executable loads .env beside the selected configuration.

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/.." && pwd)"

config_path="${1:-${INFERNAUT_CONFIG:-${repo_root}/infernaut.toml}}"
observer_url="${2:-${INFERNAUT_URL:-}}"
sound_enabled=0
case "${INFERNAUT_OBSERVER_SOUND:-0}" in
  1|true|TRUE|yes|YES|on|ON) sound_enabled=1 ;;
esac

if [[ ! -f "${config_path}" ]]; then
  printf 'Infernaut configuration not found: %s\n' "${config_path}" >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  printf '%s\n' "Cargo was not found in PATH." >&2
  exit 1
fi

cd "${repo_root}"

if [[ -n "${observer_url}" ]]; then
  if [[ "${sound_enabled}" -eq 1 ]]; then
    exec cargo run --release -- observe "${config_path}" --url "${observer_url}" --sound
  fi
  exec cargo run --release -- observe "${config_path}" --url "${observer_url}"
fi

if [[ "${sound_enabled}" -eq 1 ]]; then
  exec cargo run --release -- observe "${config_path}" --sound
fi

exec cargo run --release -- observe "${config_path}"
