// `runtime/src/math.c`: the floating-point and numeric-conversion natives. Thin wrappers
// over libm plus the int/float conversions and `f64_to_fixed`, which formats to a fixed
// number of decimal places.

#include <minunit/minunit.h>

#include <cmath>

#include "support.h"

TEST_SUITE(math_suite);

TEST(sqrt_pow) {
    TEST_EXPECT(neon_f64_sqrt(9.0) == 3.0);
    TEST_EXPECT(neon_f64_sqrt(0.0) == 0.0);
    TEST_EXPECT(neon_f64_pow(2.0, 10.0) == 1024.0);
    TEST_EXPECT(neon_f64_pow(5.0, 0.0) == 1.0);
}

TEST(rounding) {
    TEST_EXPECT(neon_f64_floor(2.9) == 2.0);
    TEST_EXPECT(neon_f64_floor(-2.1) == -3.0);
    TEST_EXPECT(neon_f64_ceil(2.1) == 3.0);
    TEST_EXPECT(neon_f64_ceil(-2.9) == -2.0);
    TEST_EXPECT(neon_f64_round(2.5) == 3.0);
    TEST_EXPECT(neon_f64_round(-2.5) == -3.0); // round half away from zero
}

TEST(abs_values) {
    TEST_EXPECT(neon_f64_abs(-3.5) == 3.5);
    TEST_EXPECT(neon_f64_abs(3.5) == 3.5);
    TEST_EXPECT(neon_i64_abs(-5) == 5);
    TEST_EXPECT(neon_i64_abs(5) == 5);
}

TEST(nan_and_infinity_predicates) {
    TEST_EXPECT(neon_f64_is_nan(NAN));
    TEST_EXPECT(!neon_f64_is_nan(1.0));
    TEST_EXPECT(neon_f64_is_infinite(INFINITY));
    TEST_EXPECT(neon_f64_is_infinite(-INFINITY));
    TEST_EXPECT(!neon_f64_is_infinite(1.0));
    TEST_EXPECT(!neon_f64_is_infinite(NAN));
}

TEST(int_float_conversions) {
    TEST_EXPECT(neon_i64_to_f64(42) == 42.0);
    TEST_EXPECT(neon_f64_to_i64(3.9) == 3); // truncates toward zero
    TEST_EXPECT(neon_f64_to_i64(-3.9) == -3);
}

TEST(to_fixed_formats_decimals) {
    neon_str a = neon_f64_to_fixed(3.14159, 2);
    TEST_EXPECT(nt_str_is(a, "3.14"));
    neon_str_release(a);

    neon_str b = neon_f64_to_fixed(2.0, 3);
    TEST_EXPECT(nt_str_is(b, "2.000"));
    neon_str_release(b);

    neon_str c = neon_f64_to_fixed(-0.5, 1);
    TEST_EXPECT(nt_str_is(c, "-0.5"));
    neon_str_release(c);
}
