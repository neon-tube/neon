// `runtime/src/list.c`: the growable list. It is copy-on-write — a write to a uniquely
// owned list mutates in place, a write to a shared one copies first — and it carries a
// value-witness so it can retain/release/compare refcounted elements. The public struct
// (`len`, `cap`, `data`) is read directly here to inspect a list without consuming it, since
// `neon_list_len` consumes its argument.

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("list");

TEST(new_is_empty) {
    neon_list* l = neon_list_new(&nt_i64_w);
    EXPECT_EQ(l->len, 0u);
    neon_release((neon_header*)l);
}

TEST(new_with_capacity_preallocates) {
    neon_list* l = neon_list_new_with_capacity(&nt_i64_w, 16);
    EXPECT_EQ(l->len, 0u);
    EXPECT_GE(l->cap, 16u);
    neon_release((neon_header*)l);
}

TEST(push_grows_and_preserves) {
    neon_list* l = neon_list_new(&nt_i64_w);
    for (int64_t i = 0; i < 10; i++) {
        l = neon_list_push(l, &i);
    }
    EXPECT_EQ(l->len, 10u);
    for (int64_t i = 0; i < 10; i++) {
        EXPECT_EQ(*(int64_t*)neon_list_at(l, i), i); // survived the reallocating growth
    }
    neon_release((neon_header*)l);
}

TEST(at_traps_out_of_bounds) {
    neon_list* l = neon_list_new(&nt_i64_w);
    int64_t v = 1;
    l = neon_list_push(l, &v);
    EXPECT_TRAP(neon_list_at(l, 5));
    EXPECT_TRAP(neon_list_at(l, -1));
    neon_release((neon_header*)l);
}

TEST(set_replaces_and_traps) {
    neon_list* l = neon_list_new(&nt_i64_w);
    int64_t a = 1, b = 2;
    l = neon_list_push(l, &a);
    l = neon_list_push(l, &b);
    int64_t nine = 9;
    l = neon_list_set(l, 0, &nine);
    EXPECT_EQ(*(int64_t*)neon_list_at(l, 0), 9);
    EXPECT_EQ(*(int64_t*)neon_list_at(l, 1), 2);
    int64_t z = 0;
    EXPECT_TRAP(neon_list_set(l, 5, &z));
    neon_release((neon_header*)l);
}

TEST(mutating_a_shared_list_copies_it) {
    neon_list* a = neon_list_new(&nt_i64_w);
    int64_t one = 1;
    a = neon_list_push(a, &one);
    neon_retain((neon_header*)a); // a second reference: a is now shared

    int64_t two = 2;
    neon_list* b = neon_list_push(a, &two); // must copy rather than mutate the shared buffer

    EXPECT_EQ(a->len, 1u); // the shared original is untouched
    EXPECT_EQ(b->len, 2u);
    EXPECT_NE(a->data, b->data); // genuinely separate buffers
    neon_release((neon_header*)a);
    neon_release((neon_header*)b);
}

TEST(eq_and_cmp) {
    neon_list* a = neon_list_new(&nt_i64_w);
    neon_list* b = neon_list_new(&nt_i64_w);
    for (int64_t i = 0; i < 3; i++) {
        a = neon_list_push(a, &i);
        b = neon_list_push(b, &i);
    }
    EXPECT(neon_list_eq(a, b));
    EXPECT_EQ(neon_list_cmp(a, b), 0);

    int64_t big = 99;
    b = neon_list_set(b, 2, &big);
    EXPECT(!neon_list_eq(a, b));
    EXPECT_EQ(neon_list_cmp(a, b), -1); // a's third element is smaller
    neon_release((neon_header*)a);
    neon_release((neon_header*)b);
}

TEST(concat_joins_and_consumes) {
    neon_list* a = neon_list_new(&nt_i64_w);
    neon_list* b = neon_list_new(&nt_i64_w);
    int64_t x = 1, y = 2, z = 3;
    a = neon_list_push(a, &x);
    b = neon_list_push(b, &y);
    b = neon_list_push(b, &z);
    neon_list* c = neon_list_concat(a, b); // consumes both
    EXPECT_EQ(c->len, 3u);
    EXPECT_EQ(*(int64_t*)neon_list_at(c, 0), 1);
    EXPECT_EQ(*(int64_t*)neon_list_at(c, 2), 3);
    neon_release((neon_header*)c);
}

TEST(refcounted_elements_are_released_with_the_list) {
    // A List[str]: pushing owned strings hands their references to the list, and releasing
    // the list must release every element. ASan is the oracle — a missed element release is
    // a leak, a double release a fault.
    neon_list* l = neon_list_new(&nt_str_w);
    for (int i = 0; i < 5; i++) {
        neon_str s = nt_owned("element");
        l = neon_list_push(l, &s); // moves the reference into the list
    }
    EXPECT_EQ(l->len, 5u);
    EXPECT(nt_str_is(*(neon_str*)neon_list_at(l, 0), "element"));
    neon_release((neon_header*)l);
}

TEST(at_scalar_indexes_and_traps_out_of_bounds) {
    // The scalar accessor takes the element size directly and does its own bounds check
    // in-header, trapping rather than returning on an out-of-range index.
    neon_list* l = neon_list_new(&nt_i64_w);
    int64_t v0 = 10, v1 = 20;
    l = neon_list_push(l, &v0);
    l = neon_list_push(l, &v1);
    EXPECT_EQ(*(int64_t*)neon_list_at_scalar(l, 0, sizeof(int64_t)), 10);
    EXPECT_EQ(*(int64_t*)neon_list_at_scalar(l, 1, sizeof(int64_t)), 20);
    EXPECT_TRAP(neon_list_at_scalar(l, 2, sizeof(int64_t)));
    EXPECT_TRAP(neon_list_at_scalar(l, -1, sizeof(int64_t)));
    neon_release((neon_header*)l);
}

TEST(len_reports_the_count_and_consumes_the_list) {
    neon_list* l = neon_list_new(&nt_i64_w);
    EXPECT_EQ(neon_list_len(l), 0); // consumes l

    neon_list* l2 = neon_list_new(&nt_i64_w);
    for (int64_t i = 0; i < 3; i++) l2 = neon_list_push(l2, &i);
    EXPECT_EQ(neon_list_len(l2), 3); // consumes l2
}

TEST(ensure_unique_returns_the_same_list_when_sole_owned) {
    neon_list* l = neon_list_new(&nt_i64_w);
    int64_t v = 1;
    l = neon_list_push(l, &v);
    neon_list* same = neon_list_ensure_unique(l); // rc == 1: no copy
    EXPECT(same == l);
    neon_release((neon_header*)same);
}

TEST(ensure_unique_copies_a_shared_list) {
    neon_list* l = neon_list_new(&nt_i64_w);
    int64_t x = 10;
    l = neon_list_push(l, &x);
    neon_retain((neon_header*)l); // rc == 2: now shared

    neon_list* copy = neon_list_ensure_unique(l); // consumes one ref, hands back a fresh copy
    EXPECT(copy != l);
    EXPECT_EQ(*(int64_t*)neon_list_at(copy, 0), 10); // same contents

    // The copy is sole-owned, so a write to it must not reach the original.
    int64_t y = 20;
    neon_list_set_scalar_inplace(copy, 0, &y, sizeof(int64_t));
    EXPECT_EQ(*(int64_t*)neon_list_at(l, 0), 10);
    EXPECT_EQ(*(int64_t*)neon_list_at(copy, 0), 20);

    neon_release((neon_header*)l);
    neon_release((neon_header*)copy);
}

TEST(set_scalar_writes_in_place_when_sole_owned_and_traps_out_of_bounds) {
    neon_list* l = neon_list_new(&nt_i64_w);
    int64_t a = 1, b = 2;
    l = neon_list_push(l, &a);
    l = neon_list_push(l, &b);

    int64_t nine = 9;
    l = neon_list_set_scalar(l, 0, &nine, sizeof(int64_t)); // sole-owned: same pointer back
    EXPECT_EQ(*(int64_t*)neon_list_at(l, 0), 9);
    EXPECT_TRAP(neon_list_set_scalar(l, 5, &nine, sizeof(int64_t)));
    EXPECT_TRAP(neon_list_set_scalar(l, -1, &nine, sizeof(int64_t)));
    neon_release((neon_header*)l);
}

TEST(set_scalar_copies_a_shared_list_before_writing) {
    neon_list* s = neon_list_new(&nt_i64_w);
    int64_t z = 100;
    s = neon_list_push(s, &z);
    neon_retain((neon_header*)s); // shared

    int64_t w = 200;
    neon_list* c = neon_list_set_scalar(s, 0, &w, sizeof(int64_t)); // clones, then writes
    EXPECT(c != s);
    EXPECT_EQ(*(int64_t*)neon_list_at(s, 0), 100); // original untouched
    EXPECT_EQ(*(int64_t*)neon_list_at(c, 0), 200);
    neon_release((neon_header*)s);
    neon_release((neon_header*)c);
}

TEST(set_scalar_inplace_overwrites_the_slot_and_traps_out_of_bounds) {
    neon_list* l = neon_list_new(&nt_i64_w);
    int64_t a = 1, b = 2;
    l = neon_list_push(l, &a);
    l = neon_list_push(l, &b);

    int64_t seven = 7;
    neon_list_set_scalar_inplace(l, 1, &seven, sizeof(int64_t));
    EXPECT_EQ(*(int64_t*)neon_list_at(l, 1), 7);
    EXPECT_EQ(*(int64_t*)neon_list_at(l, 0), 1); // its neighbour is left alone
    EXPECT_TRAP(neon_list_set_scalar_inplace(l, 2, &seven, sizeof(int64_t)));
    EXPECT_TRAP(neon_list_set_scalar_inplace(l, -1, &seven, sizeof(int64_t)));
    neon_release((neon_header*)l);
}
