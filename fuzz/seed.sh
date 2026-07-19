#!/usr/bin/env bash
# Re-seed the fuzz corpora from the real programs in the tree.
#
# `tests/lang/**` is the language specification and `stdlib/**` is the largest
# body of Neon anyone has written; between them they reach far more of the
# grammar than libFuzzer will stumble into from an empty corpus. Files are named
# by content hash so re-running is idempotent and so two seeds that happen to be
# identical collapse into one.
#
# Usage: fuzz/seed.sh
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

for target in lex parse format; do
    mkdir -p "$root/fuzz/corpus/$target"
done

count=0
while IFS= read -r -d '' file; do
    hash="$(sha1sum "$file" | cut -c1-16)"
    for target in lex parse format; do
        cp "$file" "$root/fuzz/corpus/$target/$hash"
    done
    count=$((count + 1))
done < <(find "$root/tests/lang" "$root/stdlib" -name '*.neon' -print0)

echo "seeded $count files into fuzz/corpus/{lex,parse,format}"
