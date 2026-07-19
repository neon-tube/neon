#!/usr/bin/env python3
"""Regenerate fuzz/neon.dict from the token alphabet.

The authoritative list of Neon keywords and operators is
`compiler/src/lexer/token.rs` -- `Token::keyword` for the reserved words and the
`Display` impl for everything with a fixed spelling. Both tables are exhaustive
by construction (`Display` has no catch-all arm, so a new token cannot be added
without giving it a name), which makes them worth deriving from rather than
transcribing. A hand-written dictionary goes stale the first time someone adds a
keyword; this does not.

Usage: fuzz/gen-dict.py
"""

import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parent.parent
TOKEN_RS = ROOT / "compiler/src/lexer/token.rs"
DICT = ROOT / "fuzz/neon.dict"

# Lexical constructs that are not single tokens with a fixed spelling: string
# and comment delimiters, integer-base prefixes, the interpolation opener. The
# lexer handles each in `mod.rs` rather than in the token table, so they have to
# be listed -- but they are delimiters, not vocabulary, so the list stays short
# and does not go stale the way a keyword list would.
EXTRAS = {
    "str_quote": '"',
    "interp_open": "#{",
    "comment_line": "// ",
    "comment_doc": "/// ",
    "comment_block_open": "/*",
    "comment_block_close": "*/",
    "hex_prefix": "0x",
    "bin_prefix": "0b",
    "oct_prefix": "0o",
    "underscore": "_",
    "rune_quote": "'",
}


def escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def main() -> None:
    src = TOKEN_RS.read_text()
    keywords = sorted(set(re.findall(r'"([a-z_]+)" => Token::', src)))
    symbols = sorted(
        {s for s in re.findall(r'Token::\w+ => "([^"]+)",', src) if s not in keywords},
        key=lambda s: (-len(s), s),
    )
    if not keywords or not symbols:
        raise SystemExit(f"parsed nothing out of {TOKEN_RS}; did the tables move?")

    lines = [
        "# Neon keywords and operators. GENERATED -- do not edit by hand.",
        "# Source: compiler/src/lexer/token.rs. Regenerate with fuzz/gen-dict.py.",
        "",
        "# Keywords (Token::keyword)",
    ]
    lines += [f'kw_{w}="{w}"' for w in keywords]
    lines += ["", "# Operators and punctuation (Token: Display)"]
    lines += [f'op_{i:02d}="{escape(s)}"' for i, s in enumerate(symbols)]
    lines += ["", "# Delimiters and prefixes the token table does not name"]
    lines += [f'{name}="{escape(value)}"' for name, value in EXTRAS.items()]

    DICT.write_text("\n".join(lines) + "\n")
    print(f"{DICT}: {len(keywords)} keywords, {len(symbols)} operators, {len(EXTRAS)} extras")


if __name__ == "__main__":
    main()
