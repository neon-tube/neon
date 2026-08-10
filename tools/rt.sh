#!/usr/bin/env bash
# Build the runtime archives: one full variant set per compiler FAMILY present, into the
# fixed, profile-independent location `target/neon-rt/<flavor>/{lib,include}`.
#
# This is the `cargo make rt` task — the single producer of the archives. The cargo side
# never builds them: `runtime/build.rs` is a LOCATOR that checks what this script staged
# is present and fresh, and refuses with this script's name when it is not (the repo's
# refuse-don't-substitute doctrine; a silently stale archive once cost an afternoon).
#
# Identification is by `--version`, not by name: on macOS `gcc` IS clang, and building the
# same compiler twice under two names would stage a lie. gcc must positively identify. A
# family whose compiler is absent is SKIPPED — and its stale directory is DELETED, so a
# machine that lost a compiler cannot keep linking yesterday's archives (the stale-flavor
# hole the old build.rs had).
#
# `-march=native` is opt-in via NEON_RT_NATIVE (set-but-falsey is off), sound here because
# this machine is the one that will run the programs; the release CI leaves it unset.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
dest="${NEON_RT_DIST:-$repo/target/neon-rt}"

native=OFF
case "${NEON_RT_NATIVE:-}" in
    "" | 0 | no | off | false | NO | OFF | FALSE) native=OFF ;;
    *) native=ON ;;
esac

identifies_as() { # $1 = command, $2 = family
    local text
    text="$("$1" --version 2>/dev/null | tr '[:upper:]' '[:lower:]')" || return 1
    case "$2" in
        clang) [[ "$text" == *clang* ]] ;;
        # gcc must positively identify, not merely "not clang".
        gcc) [[ "$text" == *gcc* && "$text" != *clang* ]] ;;
    esac
}

built=()
for flavor in gcc clang; do
    if ! identifies_as "$flavor" "$flavor"; then
        if [[ -d "$dest/$flavor" ]]; then
            echo "rt: $flavor is gone from PATH; removing its stale archives" >&2
            rm -rf "$dest/$flavor" "$dest/build-$flavor"
        fi
        continue
    fi
    echo "rt: building runtime archives with $flavor (native=$native)" >&2
    # `CMAKE_BUILD_TYPE=None` and no default flags: the variants carry hand-picked flags
    # (runtime/CMakeLists.txt + runtime/flags/), and cmake must not inject its own.
    cmake -S "$repo/runtime" -B "$dest/build-$flavor" \
        -DCMAKE_BUILD_TYPE=None \
        -DCMAKE_C_COMPILER="$flavor" \
        -DNEON_RT_NATIVE="$native" \
        -DCMAKE_INSTALL_PREFIX="$dest/$flavor" >/dev/null
    cmake --build "$dest/build-$flavor" --parallel >/dev/null
    cmake --install "$dest/build-$flavor" >/dev/null
    # The freshness stamp the locator compares source mtimes against. Written LAST, so an
    # interrupted build leaves no stamp and reads as stale.
    touch "$dest/$flavor/.stamp"
    built+=("$flavor")
done

if [[ ${#built[@]} -eq 0 ]]; then
    echo "rt: neither gcc nor clang is on PATH; the runtime cannot be built" >&2
    exit 1
fi
echo "rt: staged ${built[*]} under $dest" >&2
