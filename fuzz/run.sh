#!/usr/bin/env bash
# One-command entry point for the fuzzers. fuzz/README.md has the full story;
# this encodes it so nobody has to retype the parse/format incantation.
#
#   fuzz/run.sh                     # format for 300s — the target worth the hours
#   fuzz/run.sh lex 60              # target is lex|parse|format, time in seconds
#   fuzz/run.sh format 3600 -jobs=8 -workers=8   # extra libFuzzer flags pass through

set -eu
cd "$(dirname "$0")/.."

target=${1:-format}
seconds=${2:-300}
shift $(($# < 2 ? $# : 2))

case $target in
lex | parse | format) ;;
*)
	echo "usage: fuzz/run.sh [lex|parse|format] [seconds] [extra libFuzzer flags]" >&2
	exit 2
	;;
esac

cargo +nightly fuzz build "$target"

triple=$(rustc +nightly -vV | sed -n 's/^host: //p')
bin=fuzz/target/$triple/release/$target
mkdir -p "fuzz/artifacts/$target"

# Reconstitute the seed corpus after a clean checkout.
[ -n "$(ls -A "fuzz/corpus/$target" 2>/dev/null)" ] || fuzz/seed.sh

opts=(
	-dict=fuzz/neon.dict
	-max_total_time="$seconds"
	-max_len=16384
	-artifact_prefix="fuzz/artifacts/$target/"
	-print_final_stats=1
)

# parse and format leak on every iteration (chumsky Rc cycle — see README).
# LSan must be silenced via the env var so fork children inherit it, and fork
# mode is what keeps the leak from hitting the 2 GB RSS cap.
if [ "$target" != lex ]; then
	export ASAN_OPTIONS=detect_odr_violation=0:detect_leaks=0
	opts+=(-fork=1)
fi

exec "$bin" "fuzz/corpus/$target" "${opts[@]}" "$@"
