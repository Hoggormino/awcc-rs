#!/usr/bin/env bash
# Build and launch the native desktop against the in-memory device simulator.
set -euo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "${repo_root}"
exec cargo run -p awcc-gui --locked -- --sim "$@"
