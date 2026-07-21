// `runtime/include/neon/core.h`: the header-only scalar and hashing helpers codegen emits
// inline -- FNV-1a byte hashing, the two-hash mix, the atom/`f64` bit reinterpretations, and
// the unit singletons. Small, but on the hot path of every map and every `f64` literal, so
// their contracts (determinism, order-sensitivity, exact bit round-trip) are worth pinning.

#include "tinyunit.h"

#include <string.h>

#include "support.h"

TEST_SUITE("core");

TEST(hash_bytes_is_deterministic_and_content_sensitive) {
    const char* a = "hello";
    const char* b = "hellp"; // one byte different
    EXPECT_EQ(neon_hash_bytes(a, 5), neon_hash_bytes(a, 5)); // same input, same hash
    EXPECT_NE(neon_hash_bytes(a, 5), neon_hash_bytes(b, 5)); // one byte flips it
    EXPECT_NE(neon_hash_bytes("ab", 2), neon_hash_bytes("ba", 2)); // order matters

    // The empty input is the FNV-1a offset basis, untouched by the loop.
    EXPECT_EQ(neon_hash_bytes("", 0), 0xcbf29ce484222325ULL);
}

TEST(hash_mix_is_deterministic_and_input_sensitive) {
    EXPECT_EQ(neon_hash_mix(3, 7), neon_hash_mix(3, 7)); // same pair, same result
    EXPECT_NE(neon_hash_mix(3, 7), neon_hash_mix(3, 8)); // multiply by an odd prime is injective
    // `(a ^ b) * prime` is symmetric in a and b -- mixing the two field hashes of a compound
    // type is order-independent. Documenting the behaviour, not endorsing it.
    EXPECT_EQ(neon_hash_mix(3, 7), neon_hash_mix(7, 3));
}

TEST(atom_is_the_identity_on_its_hash) {
    EXPECT_EQ(neon_atom(0), 0u);
    EXPECT_EQ(neon_atom(0x123456789abcdefULL), 0x123456789abcdefULL);
}

TEST(f64_bits_reinterprets_the_payload) {
    // The IEEE-754 bit patterns for a few exact doubles: same width, no conversion.
    EXPECT_EQ(neon_f64_bits(0x3FF0000000000000ULL), 1.0);
    EXPECT_EQ(neon_f64_bits(0xC000000000000000ULL), -2.0);
    EXPECT_EQ(neon_f64_bits(0x0000000000000000ULL), 0.0);

    // Round-trips whatever bits a double already has.
    double x = -3.14159;
    uint64_t bits;
    memcpy(&bits, &x, sizeof bits);
    EXPECT_EQ(neon_f64_bits(bits), x);
}

TEST(unit_and_null_are_the_zero_unit) {
    neon_unit u = neon_unit_v();
    neon_unit n = neon_null();
    EXPECT_EQ(u._unit, 0);
    EXPECT_EQ(n._unit, 0);
    EXPECT_EQ(memcmp(&u, &n, sizeof(neon_unit)), 0); // the two spellings are the same value
}

TEST(str_data_mut_aliases_the_bytes) {
    neon_str s = nt_owned("mutable");
    // The writable view points at the same bytes the read-only view does; writing through it
    // is sound here because this string was just allocated and is sole-owned.
    char* w = neon_str_data_mut(&s);
    EXPECT(w == neon_str_data(&s));
    w[0] = 'M';
    EXPECT(nt_str_is(s, "Mutable"));
    neon_str_release(s);
}
