// `runtime/src/string.c`: the string natives. Most *consume* their arguments (release them),
// matching the calling convention codegen emits; the comparison and `+` operators borrow.
// A literal (`neon_str_lit`) has a NULL owner, so releasing it is a no-op — which is why the
// tests can pass literals freely and only have to release the *owned* results.

#include <minunit/minunit.h>

#include "support.h"

TEST_SUITE(string_suite);

TEST(lit_and_new) {
    neon_str s = neon_str_lit("hello", 5);
    TEST_EXPECT(neon_str_len(&s) == 5);
    TEST_EXPECT(nt_str_is(s, "hello"));
    TEST_EXPECT(s.owner == nullptr); // static: never freed

    neon_str o = nt_owned("world");
    TEST_EXPECT(neon_str_len(&o) == 5);
    TEST_EXPECT(nt_str_is(o, "world"));
    TEST_EXPECT(o.owner != nullptr); // heap-backed
    neon_str_release(o);
}

TEST(eq_is_by_content) {
    // Borrows both, so no release is owed on literals.
    TEST_EXPECT(neon_str_eq(neon_str_lit("abc", 3), neon_str_lit("abc", 3)));
    TEST_EXPECT(!neon_str_eq(neon_str_lit("abc", 3), neon_str_lit("abd", 3)));
    TEST_EXPECT(!neon_str_eq(neon_str_lit("abc", 3), neon_str_lit("ab", 2)));
    // Same content, different storage: still equal.
    neon_str o = nt_owned("abc");
    TEST_EXPECT(neon_str_eq(o, neon_str_lit("abc", 3)));
    neon_str_release(o);
}

TEST(cmp_is_bytewise) {
    TEST_EXPECT(neon_str_cmp(neon_str_lit("a", 1), neon_str_lit("b", 1)) == -1);
    TEST_EXPECT(neon_str_cmp(neon_str_lit("b", 1), neon_str_lit("a", 1)) == 1);
    TEST_EXPECT(neon_str_cmp(neon_str_lit("a", 1), neon_str_lit("a", 1)) == 0);
    // A prefix sorts before the longer string.
    TEST_EXPECT(neon_str_cmp(neon_str_lit("ab", 2), neon_str_lit("abc", 3)) == -1);
}

TEST(concat_consumes_and_joins) {
    // concat consumes both operands, so the owned inputs are freed by the call.
    neon_str r = neon_str_concat(nt_owned("foo"), nt_owned("bar"));
    TEST_EXPECT(nt_str_is(r, "foobar"));
    neon_str_release(r);

    // Empty operands.
    neon_str e = neon_str_concat(neon_str_lit("", 0), neon_str_lit("x", 1));
    TEST_EXPECT(nt_str_is(e, "x"));
    neon_str_release(e);
}

TEST(add_borrows) {
    // `+` borrows, so the inputs are still ours to release.
    neon_str a = nt_owned("foo");
    neon_str b = nt_owned("bar");
    neon_str r = neon_str_add(a, b);
    TEST_EXPECT(nt_str_is(r, "foobar"));
    neon_str_release(a);
    neon_str_release(b);
    neon_str_release(r);
}

TEST(case_conversion) {
    neon_str up = neon_str_to_upper(neon_str_lit("aB3z", 4));
    TEST_EXPECT(nt_str_is(up, "AB3Z"));
    neon_str_release(up);

    neon_str lo = neon_str_to_lower(neon_str_lit("aB3z", 4));
    TEST_EXPECT(nt_str_is(lo, "ab3z"));
    neon_str_release(lo);
}

TEST(repeat) {
    neon_str r = neon_str_repeat(neon_str_lit("ab", 2), 3);
    TEST_EXPECT(nt_str_is(r, "ababab"));
    neon_str_release(r);

    // n <= 0 yields the static empty string (no owner, nothing to release).
    neon_str z = neon_str_repeat(neon_str_lit("ab", 2), 0);
    TEST_EXPECT(neon_str_len(&z) == 0);
}

TEST(search_predicates) {
    TEST_EXPECT(neon_str_contains(neon_str_lit("hello", 5), neon_str_lit("ell", 3)));
    TEST_EXPECT(!neon_str_contains(neon_str_lit("hello", 5), neon_str_lit("xyz", 3)));
    TEST_EXPECT(neon_str_starts_with(neon_str_lit("hello", 5), neon_str_lit("he", 2)));
    TEST_EXPECT(!neon_str_starts_with(neon_str_lit("hello", 5), neon_str_lit("lo", 2)));
    TEST_EXPECT(neon_str_ends_with(neon_str_lit("hello", 5), neon_str_lit("lo", 2)));
    TEST_EXPECT(!neon_str_ends_with(neon_str_lit("hello", 5), neon_str_lit("he", 2)));
    // An empty needle is contained, and found at 0.
    TEST_EXPECT(neon_str_contains(neon_str_lit("hi", 2), neon_str_lit("", 0)));
    TEST_EXPECT(neon_str_index_of(neon_str_lit("hello", 5), neon_str_lit("l", 1)) == 2);
    TEST_EXPECT(neon_str_index_of(neon_str_lit("hello", 5), neon_str_lit("z", 1)) == -1);
}

TEST(slice_and_char_at) {
    neon_str sl = neon_str_slice_unchecked(neon_str_lit("hello", 5), 1, 4);
    TEST_EXPECT(nt_str_is(sl, "ell"));
    neon_str_release(sl);

    neon_str c = neon_str_char_at_unchecked(neon_str_lit("hello", 5), 0);
    TEST_EXPECT(nt_str_is(c, "h"));
    neon_str_release(c);
}

TEST(byte_len_and_is_empty) {
    TEST_EXPECT(neon_str_byte_len(neon_str_lit("héllo", 6)) == 6); // bytes, not codepoints
    TEST_EXPECT(neon_str_is_empty(neon_str_lit("", 0)));
    TEST_EXPECT(!neon_str_is_empty(neon_str_lit("x", 1)));
}

TEST(is_int_and_parse) {
    TEST_EXPECT(neon_str_is_int(neon_str_lit("123", 3)));
    TEST_EXPECT(neon_str_is_int(neon_str_lit("-42", 3)));
    TEST_EXPECT(neon_str_is_int(neon_str_lit("+7", 2)));
    TEST_EXPECT(!neon_str_is_int(neon_str_lit("12a", 3)));
    TEST_EXPECT(!neon_str_is_int(neon_str_lit("", 0)));    // no digits
    TEST_EXPECT(!neon_str_is_int(neon_str_lit("-", 1)));   // sign only
    TEST_EXPECT(neon_str_parse_int(neon_str_lit("123", 3)) == 123);
    TEST_EXPECT(neon_str_parse_int(neon_str_lit("-42", 3)) == -42);
}

TEST(to_string_family) {
    neon_str a = neon_i64_to_string(0);
    TEST_EXPECT(nt_str_is(a, "0"));
    neon_str_release(a);
    neon_str b = neon_i64_to_string(-12345);
    TEST_EXPECT(nt_str_is(b, "-12345"));
    neon_str_release(b);
    neon_str big = neon_i64_to_string(INT64_MIN);
    TEST_EXPECT(nt_str_is(big, "-9223372036854775808"));
    neon_str_release(big);

    TEST_EXPECT(nt_str_is(neon_bool_to_string(true), "true"));
    TEST_EXPECT(nt_str_is(neon_bool_to_string(false), "false"));

    // Identity, ownership passes through.
    neon_str s = nt_owned("passthrough");
    neon_str t = neon_str_to_string(s);
    TEST_EXPECT(nt_str_is(t, "passthrough"));
    neon_str_release(t);
}
