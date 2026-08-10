#!/usr/bin/env bash
# Assemble an installable prefix from a bundled release build:
#
#     <prefix>/bin/neon  include/  lib/  stdlib/
#
# One definition of the layout, shared by the release workflow and the Dockerfile —
# previously each hand-copied the directories itself.
#
# Usage: dist.sh [prefix]          (default: target/dist)
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
prefix="${1:-$repo/target/dist}"
src="$repo/target/release"

for need in neon include lib stdlib; do
    [[ -e "$src/$need" ]] || { echo "dist: $src/$need missing; run \`cargo make release\` first" >&2; exit 1; }
done

rm -rf "$prefix"
mkdir -p "$prefix/bin"
cp "$src/neon" "$prefix/bin/"
cp -r "$src/include" "$src/lib" "$src/stdlib" "$prefix/"
echo "dist: prefix assembled at $prefix" >&2
