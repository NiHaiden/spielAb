#!/usr/bin/env bash
set -euo pipefail
project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"
export CARGO_HOME="${CARGO_HOME:-$project_root/.cargo-home}"

# Some distributions install the xkbcommon-x11 runtime without its linker name.
# GPUI 0.2.2 still links this library when built with only its Wayland feature.
# Provide a build-local linker name; no system library files are modified.
if ! pkg-config --exists xkbcommon-x11; then
    for library in /usr/lib64/libxkbcommon-x11.so.0 /usr/lib/x86_64-linux-gnu/libxkbcommon-x11.so.0; do
        if [[ -f "$library" ]]; then
            mkdir -p "$project_root/target/native"
            ln -sf "$library" "$project_root/target/native/libxkbcommon-x11.so"
            export LIBRARY_PATH="$project_root/target/native${LIBRARY_PATH:+:$LIBRARY_PATH}"
            break
        fi
    done
fi

# Reuse development pairing identities, when present, across local launches.
if [[ -d "$project_root/.state" && -z "${SPIELAB_STATE_DIR:-}" ]]; then
    export SPIELAB_STATE_DIR="$project_root/.state"
fi
exec cargo run --locked --bin spielab -- "$@"
