#!/usr/bin/env bash
set -euo pipefail
script_dir="$(cd "$(dirname "$0")" && pwd)"
python3 "$script_dir/build-bundle.py" "$@"
