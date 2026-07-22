// `runtime/src/any.c`: boxed erasure. `neon_box_new` copies a payload behind a heap box
// carrying its witness and a type tag; the tag and payload are read back through the inline
// accessors, and releasing the box releases the payload's counted parts.

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("any");

TEST(box_carries_its_tag_and_payload) {
    int64_t payload = 1234;
    neon_value v = neon_box_new(&payload, &nt_i64_w, 77);
    EXPECT_EQ(neon_box_tag(v), 77);
    EXPECT_EQ(*(int64_t*)neon_box_payload(v), 1234);
    neon_release((neon_header*)v);
}

TEST(distinct_tags_are_preserved) {
    int64_t p = 0;
    neon_value a = neon_box_new(&p, &nt_i64_w, 1);
    neon_value b = neon_box_new(&p, &nt_i64_w, 2);
    EXPECT_EQ(neon_box_tag(a), 1);
    EXPECT_EQ(neon_box_tag(b), 2); // each box keeps its own tag
    neon_release((neon_header*)a);
    neon_release((neon_header*)b);
}

TEST(box_expect_hands_back_the_payload_on_a_matching_tag) {
    int64_t payload = 1234;
    neon_value v = neon_box_new(&payload, &nt_i64_w, 77);
    EXPECT_EQ(*(int64_t*)neon_box_expect(v, 77), 1234);
    neon_release((neon_header*)v);
}

TEST(box_expect_traps_on_a_mismatched_tag) {
    // The checked unbox behind `as`-from-`any`: a cast to a type the box does not hold
    // must trap, not reinterpret the payload bytes at the claimed type.
    int64_t payload = 1234;
    neon_value v = neon_box_new(&payload, &nt_i64_w, 77);
    EXPECT_TRAP((void)neon_box_expect(v, 78));
    neon_release((neon_header*)v);
}

TEST(boxing_a_string_releases_it_with_the_box) {
    // A counted payload: dropping the box must release the boxed string. ASan catches a
    // leak (missed release) or a fault (double release).
    neon_str s = nt_owned("boxed");
    neon_value v = neon_box_new(&s, &nt_str_w, 5);
    EXPECT(nt_str_is(*(neon_str*)neon_box_payload(v), "boxed"));
    neon_release((neon_header*)v);
}
