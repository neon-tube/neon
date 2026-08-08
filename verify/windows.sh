#!/bin/sh
# Run the Windows build and the C runtime's tests locally, without a Windows machine.
#
# WHY THIS EXISTS: two CI breaks in three days came from the same mistake — a green local
# build taken as evidence for a platform the local build does not cover. `-lm` was in the
# wrong position and glibc 2.34+ hid it; `cap` was unused under `#ifdef _WIN32` and the
# local build did not carry `-Werror`. Both were a `cc` invocation away from being caught.
#
# WHAT IT COVERS: mingw-w64 builds the runtime archives and the tinyunit suite, and Wine
# runs the suite — which is what CI's `runtime-windows` job does, including the re-exec
# path tinyunit uses for per-test isolation on Windows.
#
# WHAT IT DOES NOT: MSVC. `cl.exe` under Wine is possible (see the msvc-wine project) but
# needs the Visual Studio toolchain installed and licensed, and CI covers that separately.
# So this catches mingw-w64 and Windows *semantics*, not MSVC-specific diagnostics.
#
# Requires: mingw-w64 (`x86_64-w64-mingw32-gcc`), wine, cmake.
set -e

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
out=${NEON_WIN_BUILD:-/tmp/neon-win}

for tool in x86_64-w64-mingw32-gcc wine cmake; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "missing: $tool" >&2
        exit 1
    }
done

# Two configures on purpose.
#
# The archives get `-Werror`, because they are OUR code and that flag is what CI's Windows
# job builds them with -- an unused variable inside a `#ifdef _WIN32` is exactly the class
# this script exists to catch, and it is invisible to a default local build.
#
# The tests do not, because `-Werror` there fails inside `tinyunit.h`, which is fetched
# rather than written here (a signed/unsigned compare at line 403). Failing on a dependency's
# warnings would make this script useless without making the runtime any more correct.
cmake -S "$root/runtime" -B "$out/strict" \
    -DCMAKE_SYSTEM_NAME=Windows \
    -DCMAKE_C_COMPILER=x86_64-w64-mingw32-gcc \
    -DCMAKE_AR=/usr/bin/x86_64-w64-mingw32-ar \
    -DCMAKE_RANLIB=/usr/bin/x86_64-w64-mingw32-ranlib \
    -DCMAKE_C_FLAGS="-Werror -Wall -Wextra" >/dev/null
echo "== archives, mingw-w64, -Werror =="
cmake --build "$out/strict" >/dev/null
echo "ok"

cmake -S "$root/runtime" -B "$out/tests" \
    -DCMAKE_SYSTEM_NAME=Windows \
    -DCMAKE_C_COMPILER=x86_64-w64-mingw32-gcc \
    -DCMAKE_AR=/usr/bin/x86_64-w64-mingw32-ar \
    -DCMAKE_RANLIB=/usr/bin/x86_64-w64-mingw32-ranlib \
    -DNEON_RT_TESTS=ON >/dev/null
cmake --build "$out/tests" >/dev/null

echo "== runtime tests, under wine =="
WINEDEBUG=${WINEDEBUG:--all} wine "$out/tests/neon_rt_tests.exe"

# The other half of the loop: a Neon program built FOR Windows and run. This is what makes
# a `@cfg(windows)` branch testable rather than only type-checkable -- the Win32 half of
# `std::fs` was written against this.
#
# `--cfg windows` selects the branch, `--cc` cross-compiles, and `--runtime` names an archive
# the sysroot has no way to pick: it chooses by `cc` flavour for the HOST, and a mingw
# archive is neither the gcc nor the clang it knows about.
prog=${1:-$root/tests/lang/collections/fs_directories_and_metadata.neon}
golden=${prog%.neon}.stdout
echo "== $(basename "$prog"), built for windows, under wine =="
"$root/target/debug/neon" compile \
    --cfg windows --mode debug \
    --cc x86_64-w64-mingw32-gcc \
    --runtime "$out/strict/libneon_rt_debug.a" \
    -o "$out/prog.exe" "$prog" 2>&1 | grep -v "^warning" || true

WINEDEBUG=${WINEDEBUG:--all} wine "$out/prog.exe" > "$out/prog.out" 2>&1 || true
if [ -f "$golden" ] && diff -q "$golden" "$out/prog.out" >/dev/null; then
    echo "output matches the linux golden"
else
    echo "output DIFFERS from the linux golden:"
    diff "$golden" "$out/prog.out" || true
    exit 1
fi
