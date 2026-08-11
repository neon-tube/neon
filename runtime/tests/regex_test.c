// `runtime/src/regex.c`: the PCRE2 seam. Pins the wrapper's own contracts — a compiled
// pattern as a refcounted value, the ovector copied into a match object, group offsets in
// BYTES, UTF/UCP on, the substitute glue — not PCRE2 itself. Each native consumes the
// references it is handed, exactly as the compiled code would feed them, so a live regex is
// retained before every call here.

#include <string.h>

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("regex");

static neon_str lit(const char* s) {
    return neon_str_lit(s, strlen(s));
}

static bool str_is(neon_str s, const char* want) {
    bool eq = neon_str_len(&s) == strlen(want) &&
              memcmp(neon_str_data(&s), want, neon_str_len(&s)) == 0;
    neon_str_release(s);
    return eq;
}

// Compile a pattern the test knows is good, discarding the diagnostic. Returns the live
// object (rc 1). A pattern that failed would leave `code` NULL and fault the first use, so
// the tests that call this still catch a mistake here.
static neon_regex* ok_compile(const char* pat) {
    bool ok = false;
    int64_t off = 0;
    neon_str msg;
    neon_regex* re = neon_regex_compile(lit(pat), &ok, &off, &msg);
    neon_str_release(msg);
    return re;
}

// A borrowed use: retain so the consuming native does not free the caller's reference.
static neon_regex* use(neon_regex* re) {
    neon_retain((neon_header*)re);
    return re;
}

// The same, for a match object.
static neon_regex_match* use_match(neon_regex_match* m) {
    neon_retain((neon_header*)m);
    return m;
}

TEST(compile_reports_a_bad_pattern) {
    bool ok = true;
    int64_t off = -1;
    neon_str msg;
    neon_regex* re = neon_regex_compile(lit("a(b"), &ok, &off, &msg);
    EXPECT(!ok);
    EXPECT(off >= 0);              // PCRE2 points at the unclosed group
    EXPECT(neon_str_len(&msg) > 0);
    neon_str_release(msg);
    neon_release((neon_header*)re); // the failed-compile object still releases cleanly
}

TEST(is_match_literal_and_anchor) {
    neon_regex* re = ok_compile("^ab+c$");
    EXPECT(neon_regex_is_match(use(re), lit("abbbc")));
    EXPECT(!neon_regex_is_match(use(re), lit("xabbbc")));
    EXPECT(!neon_regex_is_match(use(re), lit("ac")));
    neon_release((neon_header*)re);
}

TEST(exec_reports_group_zero_span) {
    neon_regex* re = ok_compile("b+");
    neon_regex_match* m = neon_regex_exec(use(re), lit("aabbbc"), 0);
    int64_t s = -1, e = -1;
    EXPECT(neon_regex_match_group(use_match(m), 0, &s, &e));
    EXPECT_EQ(s, 2);
    EXPECT_EQ(e, 5);
    neon_release((neon_header*)m);
    neon_release((neon_header*)re);
}

TEST(no_match_is_group_zero_not_participating) {
    neon_regex* re = ok_compile("z+");
    neon_regex_match* m = neon_regex_exec(use(re), lit("aabbbc"), 0);
    int64_t s = 0, e = 0;
    EXPECT(!neon_regex_match_group(use_match(m), 0, &s, &e));
    EXPECT_EQ(s, -1);
    EXPECT_EQ(e, -1);
    neon_release((neon_header*)m);
    neon_release((neon_header*)re);
}

TEST(numbered_groups_and_a_non_participating_one) {
    // group 1 always participates; the alternation's group 2 does not on this subject.
    neon_regex* re = ok_compile("(a+)(b+)?");
    EXPECT_EQ(neon_regex_group_count(use(re)), 3); // 0, 1, 2
    neon_regex_match* m = neon_regex_exec(use(re), lit("aaa"), 0);
    int64_t s = 0, e = 0;
    EXPECT(neon_regex_match_group(use_match(m), 1, &s, &e));
    EXPECT_EQ(s, 0);
    EXPECT_EQ(e, 3);
    EXPECT(!neon_regex_match_group(use_match(m), 2, &s, &e)); // (b+)? absent
    EXPECT_EQ(s, -1);
    neon_release((neon_header*)m);
    neon_release((neon_header*)re);
}

TEST(substitute_once_and_global_with_group_refs) {
    neon_regex* re = ok_compile("(\\w+)=(\\w+)");
    EXPECT(str_is(neon_regex_substitute(use(re), lit("a=1 b=2"), lit("$2:$1"), false), "1:a b=2"));
    EXPECT(str_is(neon_regex_substitute(use(re), lit("a=1 b=2"), lit("$2:$1"), true), "1:a 2:b"));
    neon_release((neon_header*)re);
}

TEST(utf8_offsets_are_bytes) {
    // "é" is two bytes (0xC3 0xA9); the match after it starts at byte 2.
    neon_regex* re = ok_compile("x");
    neon_regex_match* m = neon_regex_exec(use(re), lit("\xc3\xa9x"), 0);
    int64_t s = -1, e = -1;
    EXPECT(neon_regex_match_group(use_match(m), 0, &s, &e));
    EXPECT_EQ(s, 2);
    EXPECT_EQ(e, 3);
    neon_release((neon_header*)m);
    neon_release((neon_header*)re);
}

TEST(unicode_property_matches_accented_letter) {
    // \w with UCP treats é as a word character — the UTF/UCP compile flags at work.
    neon_regex* re = ok_compile("^\\w+$");
    EXPECT(neon_regex_is_match(use(re), lit("caf\xc3\xa9")));
    neon_release((neon_header*)re);
}
