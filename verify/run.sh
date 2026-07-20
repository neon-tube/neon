#!/usr/bin/env bash
# One-command entry point for the Kani proofs. verify/README.md has the full story.
#
#   verify/run.sh                  # every harness
#   verify/run.sh fold_int         # only harnesses matching a filter
#   verify/run.sh '' --output-format=terse   # extra cargo-kani flags pass through

set -eu
cd "$(dirname "$0")"

filter=${1:-}
shift $(($# ? 1 : 0))

if [ -n "$filter" ]; then
	exec cargo kani --harness "$filter" "$@"
fi

exec cargo kani "$@"
