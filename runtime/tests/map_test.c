// `runtime/src/map.c`: the open-addressed hash map. Keys are content-hashed and
// content-compared through the key-witness; values carry a value-witness. Like the list it
// is copy-on-write. The public struct's `len` is read directly to avoid `neon_map_len`'s
// consuming semantics.

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("map");

// The updater `neon_map_update` calls back through, plus the closure it wraps — both are
// codegen-emitted in a real program, and written by hand here. `inc_fn` is the `(i64) -> i64`
// the closure holds; `inc_updater` is the shim that reads the slot, calls it, stores back.
static int64_t inc_fn(neon_header* env, int64_t v) {
    (void)env;
    return v + 1;
}
static void inc_updater(neon_closure f, const void* in, void* out) {
    int64_t v = *(const int64_t*)in;
    *(int64_t*)out = ((int64_t (*)(neon_header*, int64_t))f.fn)(f.env, v);
}

TEST(new_is_empty) {
    neon_map* m = neon_map_new(&nt_i64_key, &nt_i64_w);
    EXPECT_EQ(m->len, 0u);
    neon_release((neon_header*)m);
}

TEST(set_get_and_overwrite) {
    neon_map* m = neon_map_new(&nt_i64_key, &nt_i64_w);
    int64_t k = 7, v = 100;
    m = neon_map_set(m, &k, &v);
    EXPECT_EQ(m->len, 1u);
    EXPECT_EQ(*(int64_t*)neon_map_at(m, &k), 100);

    int64_t v2 = 200;
    m = neon_map_set(m, &k, &v2); // same key: overwrite, not grow
    EXPECT_EQ(m->len, 1u);
    EXPECT_EQ(*(int64_t*)neon_map_at(m, &k), 200);
    neon_release((neon_header*)m);
}

TEST(find_and_contains) {
    neon_map* m = neon_map_new(&nt_i64_key, &nt_i64_w);
    int64_t k = 1, v = 9;
    m = neon_map_set(m, &k, &v);

    int64_t absent = 2;
    EXPECT_NOT_NULL(neon_map_find(m, &k));
    EXPECT_NULL(neon_map_find(m, &absent));
    // contains consumes m and the key, so read it last.
    EXPECT(neon_map_contains(m, &k));
}

TEST(at_traps_on_missing_key) {
    neon_map* m = neon_map_new(&nt_i64_key, &nt_i64_w);
    int64_t k = 1, v = 9;
    m = neon_map_set(m, &k, &v);
    int64_t absent = 42;
    EXPECT_TRAP(neon_map_at(m, &absent));
    neon_release((neon_header*)m);
}

TEST(remove) {
    neon_map* m = neon_map_new(&nt_i64_key, &nt_i64_w);
    int64_t k1 = 1, k2 = 2, v = 0;
    m = neon_map_set(m, &k1, &v);
    m = neon_map_set(m, &k2, &v);
    m = neon_map_remove(m, &k1);
    EXPECT_EQ(m->len, 1u);
    EXPECT_NULL(neon_map_find(m, &k1));
    EXPECT_NOT_NULL(neon_map_find(m, &k2));
    // Removing an absent key is not an error and changes nothing.
    int64_t absent = 99;
    m = neon_map_remove(m, &absent);
    EXPECT_EQ(m->len, 1u);
    neon_release((neon_header*)m);
}

TEST(grows_past_the_initial_capacity) {
    neon_map* m = neon_map_new(&nt_i64_key, &nt_i64_w);
    for (int64_t i = 0; i < 100; i++) {
        m = neon_map_set(m, &i, &i);
    }
    EXPECT_EQ(m->len, 100u);
    for (int64_t i = 0; i < 100; i++) {
        EXPECT_EQ(*(int64_t*)neon_map_at(m, &i), i); // every entry survived resizing
    }
    neon_release((neon_header*)m);
}

TEST(equality_ignores_insertion_order) {
    neon_map* a = neon_map_new(&nt_i64_key, &nt_i64_w);
    neon_map* b = neon_map_new(&nt_i64_key, &nt_i64_w);
    for (int64_t i = 0; i < 5; i++) {
        int64_t v = i * 10;
        a = neon_map_set(a, &i, &v);
    }
    for (int64_t i = 4; i >= 0; i--) { // reverse order
        int64_t v = i * 10;
        b = neon_map_set(b, &i, &v);
    }
    EXPECT(neon_map_eq(a, b));

    int64_t k = 0, differ = 999;
    b = neon_map_set(b, &k, &differ);
    EXPECT(!neon_map_eq(a, b)); // one differing value breaks it
    neon_release((neon_header*)a);
    neon_release((neon_header*)b);
}

TEST(update_reads_modifies_writes_in_one_probe) {
    neon_map* m = neon_map_new(&nt_i64_key, &nt_i64_w);
    int64_t k = 5, zero = 0;
    neon_closure f = {(void*)inc_fn, NULL};

    // Absent: the fallback is fed to the closure.
    m = neon_map_update(m, &k, &zero, f, inc_updater);
    EXPECT_EQ(*(int64_t*)neon_map_at(m, &k), 1);
    // Present: the stored value is fed to the closure.
    m = neon_map_update(m, &k, &zero, f, inc_updater);
    EXPECT_EQ(*(int64_t*)neon_map_at(m, &k), 2);
    neon_release((neon_header*)m);
}

TEST(string_keys_and_values) {
    // Map[str, i64]: exercises the key-witness (content hash/eq) and, on release, the key's
    // and value's counted parts. ASan is the oracle for the ownership.
    neon_map* m = neon_map_new(&nt_str_key, &nt_i64_w);
    for (int i = 0; i < 20; i++) {
        neon_str key = neon_i64_to_string(i);
        int64_t v = i;
        m = neon_map_set(m, &key, &v); // consumes the key
    }
    EXPECT_EQ(m->len, 20u);
    neon_str probe = neon_str_lit("7", 1);
    EXPECT_EQ(*(int64_t*)neon_map_at(m, &probe), 7);
    neon_release((neon_header*)m);
}

TEST(keys_and_values_lists) {
    neon_map* m = neon_map_new(&nt_i64_key, &nt_i64_w);
    int64_t k = 3, v = 30;
    m = neon_map_set(m, &k, &v);
    neon_retain((neon_header*)m); // keys/values each consume a reference

    neon_list* keys = neon_map_keys(m, &nt_i64_w);
    neon_list* vals = neon_map_values(m, &nt_i64_w);
    EXPECT_EQ(keys->len, 1u);
    EXPECT_EQ(vals->len, 1u);
    EXPECT_EQ(*(int64_t*)neon_list_at(keys, 0), 3);
    EXPECT_EQ(*(int64_t*)neon_list_at(vals, 0), 30);
    neon_release((neon_header*)keys);
    neon_release((neon_header*)vals);
}
