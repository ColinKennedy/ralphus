#!/usr/bin/env bash
# Build the initial static Linux runner artifact used by SSH upload mode.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
target="${1:-x86_64-unknown-linux-musl}"
if [ "$target" != "x86_64-unknown-linux-musl" ]; then
  echo "unsupported target: $target (expected x86_64-unknown-linux-musl)" >&2
  exit 2
fi

cargo build --release --package ralphus-runner --target "$target" --manifest-path "$root/Cargo.toml"
output="$root/dist/remote-runners/$target"
mkdir -p "$output"
cp "$root/target/$target/release/ralphus-runner" "$output/ralphus-runner"
checksum="$(sha256sum "$output/ralphus-runner" | awk '{print $1}')"
printf '%s  ralphus-runner\n' "$checksum" > "$output/ralphus-runner.sha256"
echo "Built $output/ralphus-runner"
echo "SHA-256 $checksum"
