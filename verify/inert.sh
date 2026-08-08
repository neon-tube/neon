#!/bin/sh
# Prove a change is inert: snapshot what the compiler emits for the whole corpus, make the
# change, and diff. Anything that moves is either intended or a bug.
#
# WHY THIS EXISTS: the corpus is 265 expected-pass programs, and passing it is weak
# evidence. In one session of 2026-08 roughly twenty silent wrong answers were found in
# `typecheck/`, `ir/` and `backend/` — container covariance miscompiling, structural `is`
# always false, return-position dispatch running the wrong impl — every one of them while
# the suite was green. A refactor that quietly changes behaviour in an unexercised corner
# looks exactly like a refactor that does not.
#
# So this does not test behaviour. It tests that behaviour did not *change*, which is the
# actual contract of a cleanup and a far stronger claim than any pass/fail: not "the cases
# we thought of still work" but "the compiler is emitting the same thing for 358 programs".
#
# WHAT IT COVERS, per corpus file, as four independent observables:
#
#   parse   the syntax tree            — lexer, parser
#   check   diagnostics, verbatim      — the type checker, including what it *rejects*
#   ir      the lowered IR             — lowering, monomorphisation, the optimiser passes
#   c       the generated C            — repr and the C backend
#
# `check` is the one worth understanding: it captures stderr, so the compile-fail corpus
# pins diagnostic *text*. A refactor that changes an error message shows up here, which is
# usually intended and occasionally the first sign a type got reshaped.
#
# The C is captured with a `--cc` wrapper that pockets the file and exits 0, so no C
# compiler runs and a full-pipeline snapshot costs about as much as type-checking.
#
# WHAT IT DOES NOT COVER: anything absent from the corpus, runtime behaviour (the C is
# hashed, never run), and the stdlib except as the corpus exercises it. An inert diff is
# evidence a change preserved behaviour, not that the behaviour is right.
#
# WHAT IT REQUIRES OF THE COMPILER: that emission be a function of the input. It very
# nearly is — `ir::partial` iterated a `HashMap<BlockId, _>` and let the hash order pick
# SSA numbers, which is fixed. If a future pass reintroduces that, `record` twice with no
# change between and this reports it as a diff.
#
# Usage:
#   verify/inert.sh record [file]   snapshot to [file]      (default target/inert.txt)
#   verify/inert.sh check  [file]   re-snapshot and diff against it
#
# Typical:
#   verify/inert.sh record          # before
#   ...edit, cargo build --release...
#   verify/inert.sh check           # after; silence means inert
set -e

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
mode=${1:-check}
out=${2:-$root/target/inert.txt}
neon=$root/target/release/neon
jobs=$(command -v nproc >/dev/null 2>&1 && nproc || echo 4)

case "$mode" in
    record | check) ;;
    *)
        echo "usage: $0 record|check [file]" >&2
        exit 2
        ;;
esac

[ -x "$neon" ] || {
    echo "missing: $neon (cargo build --release)" >&2
    exit 1
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# The `cc` the compiler will invoke: keep the .c, compile nothing. `neon compile` passes
# the source among many flags, so this picks it out by extension rather than by position.
cat > "$tmp/cc" <<'WRAPPER'
#!/bin/sh
for a in "$@"; do
    case "$a" in
    *.c) [ -n "$NEON_INERT_C" ] && cp "$a" "$NEON_INERT_C" ;;
    esac
done
exit 0
WRAPPER
chmod +x "$tmp/cc"

# Each file's observables are computed in a child shell, so the body lives in the `sh -c`
# below rather than in a function here: `export -f` is a bashism and this is `/bin/sh`.
# What the children need arrives through the environment instead.
export root neon tmp

# One file's five observables, as `<path> <stage> <md5>` lines. Stdout and stderr both, so
# a diagnostic counts; exit status is deliberately ignored, since a compile-fail test
# failing to compile *is* its observable. A file with no `main` has no C, and `-` records
# that — distinguishing "no C to compare" from "the C changed".
run_all() {
    find "$root/tests/lang" -name '*.neon' | sort | xargs -P "$jobs" -I{} sh -c '
        f={}
        rel=${f#"$root"/}
        for stage in parse check ir; do
            printf "%s %s %s\n" "$rel" "$stage" \
                "$("$neon" "$stage" "$f" 2>&1 | md5sum | cut -d" " -f1)"
        done
        printf "%s %s %s\n" "$rel" fmt \
            "$("$neon" fmt "$f" 2>&1 | md5sum | cut -d" " -f1)"
        c=$tmp/$$.c
        rm -f "$c"
        NEON_INERT_C=$c "$neon" compile --cc "$tmp/cc" -o "$tmp/$$.bin" "$f" >/dev/null 2>&1 || true
        if [ -f "$c" ]; then
            printf "%s %s %s\n" "$rel" c "$(md5sum < "$c" | cut -d" " -f1)"
        else
            printf "%s %s -\n" "$rel" c
        fi
        rm -f "$c" "$tmp/$$.bin"
    ' | sort
}

mkdir -p "$(dirname -- "$out")"

if [ "$mode" = record ]; then
    run_all > "$out"
    echo "recorded $(wc -l < "$out" | tr -d ' ') observables from \
$(find "$root/tests/lang" -name '*.neon' | wc -l | tr -d ' ') files -> $out"
    exit 0
fi

[ -f "$out" ] || {
    echo "no baseline at $out (run: $0 record)" >&2
    exit 1
}

run_all > "$tmp/now.txt"

if diff -q "$out" "$tmp/now.txt" > /dev/null; then
    echo "inert: $(wc -l < "$out" | tr -d ' ') observables unchanged"
    exit 0
fi

echo "CHANGED — every line below is a behaviour difference, intended or not:"
echo
# `<` is the baseline and `>` is now; printing the pair keeps it obvious which stage of
# which file moved without needing the file open.
diff "$out" "$tmp/now.txt" | grep -E '^[<>]' | sort -k2 | awk '
    $1 == "<" { was[$2 " " $3] = $4; next }
    $1 == ">" { now[$2 " " $3] = $4 }
    END {
        for (k in now)
            if (was[k] != now[k])
                printf "  %s: %s -> %s\n", k, (k in was ? was[k] : "absent"), now[k]
        for (k in was)
            if (!(k in now)) printf "  %s: %s -> absent\n", k, was[k]
    }
' | sort
echo
echo "to see one: neon <stage> <file> | diff - <(git stash; neon <stage> <file>; git stash pop)"
exit 1
