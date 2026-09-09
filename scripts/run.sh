#!/usr/bin/env bash
set -euo pipefail
project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"
export CARGO_HOME="${CARGO_HOME:-$project_root/.cargo-home}"

# Reuse development pairing identities, when present, across local launches.
if [[ -d "$project_root/.state" && -z "${SPIELAB_STATE_DIR:-}" ]]; then
    export SPIELAB_STATE_DIR="$project_root/.state"
fi
exec cargo run --locked --bin spielab -- "$@"
