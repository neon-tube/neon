#ifndef NEON_REGEX_H
#define NEON_REGEX_H

// The PCRE2 seam behind `std::regex` (`runtime/src/regex.c` wraps it).
//
// A `neon_regex` is an ordinary refcounted runtime value: an immutable compiled pattern,
// NOT a resource. It is born from `neon_regex_compile`, shared by retain/release like any
// heap object, and its drop frees the underlying `pcre2_code` exactly once. PCRE2 runs in
// interpreted mode (JIT is off in the build), so matching is `pcre2_match`; a fresh
// `pcre2_match_data` is allocated and freed per call. UTF-8 is on (PCRE2_UTF|PCRE2_UCP), so
// patterns work over UTF-8 subjects; every offset below is a BYTE offset into the subject.
//
// A `neon_regex_match` captures the ovector of one match: whether it matched, and the
// (start,end) byte span of each group. It too is a plain refcounted object, produced by
// `neon_regex_exec` and read with `neon_regex_match_group`.
//
// Calling convention, as everywhere in the runtime: each native CONSUMES the references it
// is handed — the `neon_str` subjects/patterns and the object arguments are released before
// it returns. The compiled `neon_regex` is fiber-agnostic (an immutable value with no
// interior state), unlike the http parser.

#include <stdbool.h>
#include <stdint.h>

#include "neon/core.h"

typedef struct neon_regex neon_regex;
typedef struct neon_regex_match neon_regex_match;

// Compile `pattern` (UTF/UCP). On success returns the compiled object and sets `*ok` true,
// `*offset` to -1, `*message` to "". On failure returns an object wrapping no code (safe to
// release, never otherwise used), sets `*ok` false, `*offset` to PCRE2's error offset, and
// `*message` to PCRE2's error text. Consumes `pattern`.
neon_regex* neon_regex_compile(neon_str pattern, bool* ok, int64_t* offset, neon_str* message);

// Whether `re` matches anywhere in `subject`. Consumes both.
bool neon_regex_is_match(neon_regex* re, neon_str subject);

// The number of groups the pattern defines, group 0 (the whole match) included — i.e.
// PCRE2's capture count plus one. A property of the pattern, no subject needed. Consumes re.
int64_t neon_regex_group_count(neon_regex* re);

// Match `re` against `subject` starting at byte offset `start`, leftmost. The returned
// object records the whole ovector; read it with `neon_regex_match_group`. Never NULL — a
// non-match is reported by group 0 not participating. Consumes `re` and `subject`.
neon_regex_match* neon_regex_exec(neon_regex* re, neon_str subject, int64_t start);

// The byte span of group `group` in `m`: returns whether that group participated (group 0
// participating == the pattern matched at all), writing its start/end when it did and
// (-1,-1) when it did not. Consumes `m`.
bool neon_regex_match_group(neon_regex_match* m, int64_t group, int64_t* start, int64_t* end);

// Substitute matches of `re` in `subject` with `replacement`, once or globally. The
// replacement uses PCRE2's syntax: `$0`/`${0}` the whole match, `$1`..`$n`/`${n}` a group
// (an unset group is empty), and `$$` a literal `$`. Consumes `re`, `subject`, `replacement`.
neon_str neon_regex_substitute(neon_regex* re, neon_str subject, neon_str replacement,
                               bool global);

#endif
