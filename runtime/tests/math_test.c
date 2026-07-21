// `runtime/src/math.c`: the floating-point and numeric-conversion natives. Thin wrappers
// over libm plus the int/float conversions and `f64_to_fixed`, which formats to a fixed
// number of decimal places.

#include "tinyunit.h"

#include <math.h>

#include "support.h"

TEST_SUITE("math");

TEST(sqrt_pow) {
    EXPECT_EQ(neon_f64_sqrt(9.0), 3.0);
    EXPECT_EQ(neon_f64_sqrt(0.0), 0.0);
    EXPECT_EQ(neon_f64_pow(2.0, 10.0), 1024.0);
    EXPECT_EQ(neon_f64_pow(5.0, 0.0), 1.0);
}

TEST(rounding) {
    EXPECT_EQ(neon_f64_floor(2.9), 2.0);
    EXPECT_EQ(neon_f64_floor(-2.1), -3.0);
    EXPECT_EQ(neon_f64_ceil(2.1), 3.0);
    EXPECT_EQ(neon_f64_ceil(-2.9), -2.0);
    EXPECT_EQ(neon_f64_round(2.5), 3.0);
    EXPECT_EQ(neon_f64_round(-2.5), -3.0); // round half away from zero
}

TEST(abs_values) {
    EXPECT_EQ(neon_f64_abs(-3.5), 3.5);
    EXPECT_EQ(neon_f64_abs(3.5), 3.5);
    EXPECT_EQ(neon_i64_abs(-5), 5);
    EXPECT_EQ(neon_i64_abs(5), 5);
}

TEST(nan_and_infinity_predicates) {
    EXPECT(neon_f64_is_nan(NAN));
    EXPECT(!neon_f64_is_nan(1.0));
    EXPECT(neon_f64_is_infinite(INFINITY));
    EXPECT(neon_f64_is_infinite(-INFINITY));
    EXPECT(!neon_f64_is_infinite(1.0));
    EXPECT(!neon_f64_is_infinite(NAN));
}

TEST(int_float_conversions) {
    EXPECT_EQ(neon_i64_to_f64(42), 42.0);
    EXPECT_EQ(neon_f64_to_i64(3.9), 3); // truncates toward zero
    EXPECT_EQ(neon_f64_to_i64(-3.9), -3);
}

TEST(to_fixed_formats_decimals) {
    neon_str a = neon_f64_to_fixed(3.14159, 2);
    EXPECT(nt_str_is(a, "3.14"));
    neon_str_release(a);

    neon_str b = neon_f64_to_fixed(2.0, 3);
    EXPECT(nt_str_is(b, "2.000"));
    neon_str_release(b);

    neon_str c = neon_f64_to_fixed(-0.5, 1);
    EXPECT(nt_str_is(c, "-0.5"));
    neon_str_release(c);
}

TEST(to_string_is_shortest_round_trip) {
    // `%g`: the shortest form that reads back to the same double, so a whole number loses its
    // trailing zero and a fraction keeps only the digits it needs.
    neon_str a = neon_f64_to_string(3.5);
    EXPECT(nt_str_is(a, "3.5"));
    neon_str_release(a);

    neon_str b = neon_f64_to_string(2.0);
    EXPECT(nt_str_is(b, "2")); // not "2.000000"
    neon_str_release(b);

    neon_str c = neon_f64_to_string(-0.25);
    EXPECT(nt_str_is(c, "-0.25"));
    neon_str_release(c);

    // The non-finite values render as words, not as garbage or a trap.
    neon_str inf = neon_f64_to_string(INFINITY);
    EXPECT(nt_str_is(inf, "inf"));
    neon_str_release(inf);

    neon_str nan = neon_f64_to_string(NAN);
    EXPECT(nt_str_is(nan, "nan"));
    neon_str_release(nan);
}
