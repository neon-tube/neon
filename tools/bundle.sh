#!/usr/bin/env bash
# Stage the sysroot next to the built CLI, in the layout an install uses:
#
#     dev                          installed
#     target/<profile>/neon        prefix/bin/neon
#     target/<profile>/include/    prefix/include/
#     target/<profile>/lib/        prefix/lib/
#     target/<profile>/stdlib/     prefix/stdlib/
#
# so `Sysroot::find` resolves both the same way. This used to be `cli/build.rs` writing
# outside its own OUT_DIR — a documented cargo anti-pattern with a real race against
# rust-analyzer — and is now the `cargo make bundle` step: sequencing belongs to the
# orchestrator, not a build script's side effects.
#
# Usage: bundle.sh <profile>       (debug | release)
set -euo pipefail

profile="${1:?usage: bundle.sh <profile>}"
repo="$(cd "$(dirname "$0")/.." && pwd)"
rt="${NEON_RT_DIST:-$repo/target/neon-rt}"
out="$repo/target/$profile"

[[ -d "$out" ]] || { echo "bundle: $out does not exist; build first" >&2; exit 1; }

# Headers are compiler-independent; take them from any flavor present.
src_include=""
for flavor in gcc clang; do
    [[ -d "$rt/$flavor/include" ]] && { src_include="$rt/$flavor/include"; break; }
done
[[ -n "$src_include" ]] || { echo "bundle: no archives under $rt; run \`cargo make rt\`" >&2; exit 1; }

# Trees are cleared first so a deleted source file (or a flavor that stopped being built)
# cannot linger in the sysroot and keep "working".
rm -rf "$out/include" "$out/lib" "$out/stdlib"
cp -r "$src_include" "$out/include"
for flavor in gcc clang; do
    if [[ -d "$rt/$flavor/lib" ]]; then
        mkdir -p "$out/lib/$flavor"
        cp "$rt/$flavor/lib/"libneon_rt*.a "$out/lib/$flavor/"
    fi
done
cp -r "$repo/stdlib" "$out/stdlib"
echo "bundle: sysroot staged into $out" >&2
