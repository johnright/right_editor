#!/usr/bin/env bash
# Build and run the Windows exe from WSL.
#   scripts/run-win.sh [--release] [-- right_editor args]
# --release runs the installed copy (see build-win.sh).
set -euo pipefail
cd "$(dirname "$0")/.."

profile=debug
cargo_args=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --release) profile=release; cargo_args+=(--release); shift ;;
        --) shift; break ;;
        *) cargo_args+=("$1"); shift ;;
    esac
done

if [[ $profile == release ]]; then
    exe=$(scripts/build-win.sh "${cargo_args[@]}")
else
    scripts/build-win.sh "${cargo_args[@]}"
    exe="target/x86_64-pc-windows-msvc/debug/right_editor.exe"
fi
exec "$exe" "$@"
