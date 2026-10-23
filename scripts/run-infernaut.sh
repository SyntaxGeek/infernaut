#!/usr/bin/env bash
set -euo pipefail

# Start the Infernaut server from the repository root.
#
# The executable loads .env beside CONFIG when present. Override its location
# with INFERNAUT_ENV_FILE, or export the variables in your shell instead.

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/.." && pwd)"

config_path="${1:-${INFERNAUT_CONFIG:-${repo_root}/infernaut.toml}}"

if [[ ! -f "${config_path}" ]]; then
  printf 'Infernaut configuration not found: %s\n' "${config_path}" >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  printf '%s\n' "Cargo was not found in PATH." >&2
  exit 1
fi

cd "${repo_root}"

printf 'Starting Infernaut\n'
printf '  config: %s\n' "${config_path}"
printf '  logs:   %s\n' "${RUST_LOG:-from .env or infernaut=info default}"
printf '  command: cargo run --release -- %q\n' "${config_path}"

exec cargo run --release -- "${config_path}"
