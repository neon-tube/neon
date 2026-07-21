// `runtime/src/string.c`: the string natives. Most *consume* their arguments (release them),
// matching the calling convention codegen emits; the comparison and `+` operators borrow.
// A literal (`neon_str_lit`) has a NULL owner, so releasing it is a no-op — which is why the
// tests can pass literals freely and only have to release the *owned* results.

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("string");

TEST(lit_and_new) {
    neon_str s = neon_str_lit("hello", 5);
    EXPECT_EQ(neon_str_len(&s), 5u);
    EXPECT(nt_str_is(s, "hello"));
    EXPECT_NULL(s.owner); // static: never freed

    neon_str o = nt_owned("world");
    EXPECT_EQ(neon_str_len(&o), 5u);
    EXPECT(nt_str_is(o, "world"));
    EXPECT_NOT_NULL(o.owner); // heap-backed
    neon_str_release(o);
}

TEST(eq_is_by_content) {
    // Borrows both, so no release is owed on literals.
    EXPECT(neon_str_eq(neon_str_lit("abc", 3), neon_str_lit("abc", 3)));
    EXPECT(!neon_str_eq(neon_str_lit("abc", 3), neon_str_lit("abd", 3)));
    EXPECT(!neon_str_eq(neon_str_lit("abc", 3), neon_str_lit("ab", 2)));
    // Same content, different storage: still equal.
    neon_str o = nt_owned("abc");
    EXPECT(neon_str_eq(o, neon_str_lit("abc", 3)));
    neon_str_release(o);
}

TEST(cmp_is_bytewise) {
    EXPECT_EQ(neon_str_cmp(neon_str_lit("a", 1), neon_str_lit("b", 1)), -1);
    EXPECT_EQ(neon_str_cmp(neon_str_lit("b", 1), neon_str_lit("a", 1)), 1);
    EXPECT_EQ(neon_str_cmp(neon_str_lit("a", 1), neon_str_lit("a", 1)), 0);
    // A prefix sorts before the longer string.
    EXPECT_EQ(neon_str_cmp(neon_str_lit("ab", 2), neon_str_lit("abc", 3)), -1);
}

TEST(concat_consumes_and_joins) {
    // concat consumes both operands, so the owned inputs are freed by the call.
    neon_str r = neon_str_concat(nt_owned("foo"), nt_owned("bar"));
    EXPECT(nt_str_is(r, "foobar"));
    neon_str_release(r);

    // Empty operands.
    neon_str e = neon_str_concat(neon_str_lit("", 0), neon_str_lit("x", 1));
    EXPECT(nt_str_is(e, "x"));
    neon_str_release(e);
}

TEST(add_borrows) {
    // `+` borrows, so the inputs are still ours to release.
    neon_str a = nt_owned("foo");
    neon_str b = nt_owned("bar");
    neon_str r = neon_str_add(a, b);
    EXPECT(nt_str_is(r, "foobar"));
    neon_str_release(a);
    neon_str_release(b);
    neon_str_release(r);
}

TEST(case_conversion) {
    neon_str up = neon_str_to_upper(neon_str_lit("aB3z", 4));
    EXPECT(nt_str_is(up, "AB3Z"));
    neon_str_release(up);

    neon_str lo = neon_str_to_lower(neon_str_lit("aB3z", 4));
    EXPECT(nt_str_is(lo, "ab3z"));
    neon_str_release(lo);
}

TEST(repeat) {
    neon_str r = neon_str_repeat(neon_str_lit("ab", 2), 3);
    EXPECT(nt_str_is(r, "ababab"));
    neon_str_release(r);

    // n <= 0 yields the static empty string (no owner, nothing to release).
    neon_str z = neon_str_repeat(neon_str_lit("ab", 2), 0);
    EXPECT_EQ(neon_str_len(&z), 0u);
}

TEST(search_predicates) {
    EXPECT(neon_str_contains(neon_str_lit("hello", 5), neon_str_lit("ell", 3)));
    EXPECT(!neon_str_contains(neon_str_lit("hello", 5), neon_str_lit("xyz", 3)));
    EXPECT(neon_str_starts_with(neon_str_lit("hello", 5), neon_str_lit("he", 2)));
    EXPECT(!neon_str_starts_with(neon_str_lit("hello", 5), neon_str_lit("lo", 2)));
    EXPECT(neon_str_ends_with(neon_str_lit("hello", 5), neon_str_lit("lo", 2)));
    EXPECT(!neon_str_ends_with(neon_str_lit("hello", 5), neon_str_lit("he", 2)));
    // An empty needle is contained, and found at 0.
    EXPECT(neon_str_contains(neon_str_lit("hi", 2), neon_str_lit("", 0)));
    EXPECT_EQ(neon_str_index_of(neon_str_lit("hello", 5), neon_str_lit("l", 1)), 2);
    EXPECT_EQ(neon_str_index_of(neon_str_lit("hello", 5), neon_str_lit("z", 1)), -1);
}

TEST(slice_and_char_at) {
    neon_str sl = neon_str_slice_unchecked(neon_str_lit("hello", 5), 1, 4);
    EXPECT(nt_str_is(sl, "ell"));
    neon_str_release(sl);

    neon_str c = neon_str_char_at_unchecked(neon_str_lit("hello", 5), 0);
    EXPECT(nt_str_is(c, "h"));
    neon_str_release(c);
}

TEST(byte_len_and_is_empty) {
    EXPECT_EQ(neon_str_byte_len(neon_str_lit("h\xc3\xa9llo", 6)), 6); // bytes, not codepoints
    EXPECT(neon_str_is_empty(neon_str_lit("", 0)));
    EXPECT(!neon_str_is_empty(neon_str_lit("x", 1)));
}

TEST(is_int_and_parse) {
    EXPECT(neon_str_is_int(neon_str_lit("123", 3)));
    EXPECT(neon_str_is_int(neon_str_lit("-42", 3)));
    EXPECT(neon_str_is_int(neon_str_lit("+7", 2)));
    EXPECT(!neon_str_is_int(neon_str_lit("12a", 3)));
    EXPECT(!neon_str_is_int(neon_str_lit("", 0)));    // no digits
    EXPECT(!neon_str_is_int(neon_str_lit("-", 1)));   // sign only
    EXPECT_EQ(neon_str_parse_int(neon_str_lit("123", 3)), 123);
    EXPECT_EQ(neon_str_parse_int(neon_str_lit("-42", 3)), -42);
}

TEST(to_string_family) {
    neon_str a = neon_i64_to_string(0);
    EXPECT(nt_str_is(a, "0"));
    neon_str_release(a);
    neon_str b = neon_i64_to_string(-12345);
    EXPECT(nt_str_is(b, "-12345"));
    neon_str_release(b);
    neon_str big = neon_i64_to_string(INT64_MIN);
    EXPECT(nt_str_is(big, "-9223372036854775808"));
    neon_str_release(big);

    EXPECT(nt_str_is(neon_bool_to_string(true), "true"));
    EXPECT(nt_str_is(neon_bool_to_string(false), "false"));

    // Identity, ownership passes through.
    neon_str s = nt_owned("passthrough");
    neon_str t = neon_str_to_string(s);
    EXPECT(nt_str_is(t, "passthrough"));
    neon_str_release(t);
}

TEST(join_inserts_the_separator_between_parts) {
    // Consumes both the list (with its elements) and the separator. Owned strings all round,
    // so ASan witnesses that every one is released exactly once.
    neon_list* parts = neon_list_new(&nt_str_w);
    neon_str a = nt_owned("a"), b = nt_owned("b"), c = nt_owned("c");
    parts = neon_list_push(parts, &a);
    parts = neon_list_push(parts, &b);
    parts = neon_list_push(parts, &c);
    neon_str r = neon_str_join(parts, nt_owned(", ")); // consumes parts and the separator
    EXPECT(nt_str_is(r, "a, b, c"));
    neon_str_release(r);
}

TEST(join_edge_cases) {
    // Empty list: the empty string, and the separator appears nowhere.
    neon_list* none = neon_list_new(&nt_str_w);
    neon_str r0 = neon_str_join(none, nt_owned("-"));
    EXPECT_EQ(neon_str_len(&r0), 0u);
    neon_str_release(r0);

    // One element: no separator, since a separator only sits *between* parts.
    neon_list* one = neon_list_new(&nt_str_w);
    neon_str solo = nt_owned("solo");
    one = neon_list_push(one, &solo);
    neon_str r1 = neon_str_join(one, nt_owned("-"));
    EXPECT(nt_str_is(r1, "solo"));
    neon_str_release(r1);
}
