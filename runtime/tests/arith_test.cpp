// `runtime/src/arith.c`: the checked integer primitives. They wrap on overflow (two's
// complement, via a uint64_t round-trip) except division and remainder, which trap on a
// zero divisor and on the one signed overflow, INT64_MIN / -1.

#include <minunit/minunit.h>

#include "support.h"

TEST_SUITE(arith_suite);

TEST(add_sub_mul_wrap) {
    TEST_EXPECT(neon_i64_add(2, 3) == 5);
    TEST_EXPECT(neon_i64_sub(3, 5) == -2);
    TEST_EXPECT(neon_i64_mul(6, 7) == 42);

    // Wrap, not trap: the runtime rounds through uint64_t, so overflow is defined.
    TEST_EXPECT(neon_i64_add(INT64_MAX, 1) == INT64_MIN);
    TEST_EXPECT(neon_i64_sub(INT64_MIN, 1) == INT64_MAX);
    TEST_EXPECT(neon_i64_mul(INT64_MIN, -1) == INT64_MIN);
}

TEST(div_rem_values) {
    TEST_EXPECT(neon_i64_div(17, 5) == 3);
    TEST_EXPECT(neon_i64_rem(17, 5) == 2);
    // C truncates toward zero, and so does this.
    TEST_EXPECT(neon_i64_div(-17, 5) == -3);
    TEST_EXPECT(neon_i64_rem(-17, 5) == -2);
}

TEST(div_rem_trap_on_zero) {
    TEST_EXPECT(traps([] { neon_i64_div(1, 0); }));
    TEST_EXPECT(traps([] { neon_i64_rem(1, 0); }));
}

TEST(div_rem_trap_on_signed_overflow) {
    // INT64_MIN / -1 is +2^63, unrepresentable — a trap, not a wrap.
    TEST_EXPECT(traps([] { neon_i64_div(INT64_MIN, -1); }));
    TEST_EXPECT(traps([] { neon_i64_rem(INT64_MIN, -1); }));
}

TEST(neg_wraps_at_min) {
    TEST_EXPECT(neon_i64_neg(5) == -5);
    TEST_EXPECT(neon_i64_neg(-5) == 5);
    // -INT64_MIN is unrepresentable; the runtime negates through uint64_t, so it wraps to
    // INT64_MIN rather than trapping.
    TEST_EXPECT(neon_i64_neg(INT64_MIN) == INT64_MIN);
}
