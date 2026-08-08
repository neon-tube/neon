#include "libneon_rt.h"

#include "internal.h"

#include <math.h>
#include <stdio.h>

// ---- math ----
//
// Thin over libm. `f64` keeps IEEE semantics throughout the language (see "Comparison is
// structural" in docs/decisions.md), so these do not trap or throw: `sqrt(-1)` is NaN,
// `1.0/0.0` is infinity, and a caller who cares tests for them. That is consistent with
// `==` and `<`, which already answer the IEEE way for NaN.
//
// `i64` is the opposite and stays so: `neon_i64_abs(INT64_MIN)` has no representable
// answer, so it traps, exactly as division does.
double neon_f64_sqrt(double x) { return sqrt(x); }
double neon_f64_pow(double a, double b) { return pow(a, b); }
double neon_f64_floor(double x) { return floor(x); }
double neon_f64_ceil(double x) { return ceil(x); }
double neon_f64_round(double x) { return round(x); }
double neon_f64_abs(double x) { return fabs(x); }
bool neon_f64_is_nan(double x) { return x != x; }
bool neon_f64_is_infinite(double x) { return isinf(x) != 0; }
double neon_f64_ln(double x) { return log(x); }
double neon_f64_log10(double x) { return log10(x); }
double neon_f64_exp(double x) { return exp(x); }
double neon_f64_sin(double x) { return sin(x); }
double neon_f64_cos(double x) { return cos(x); }
double neon_f64_tan(double x) { return tan(x); }
double neon_f64_atan2(double y, double x) { return atan2(y, x); }
// fmin/fmax, with libm's NaN rule: one NaN answers the OTHER operand, both-NaN answers
// NaN. That treats NaN as "no data" rather than poison -- the useful reading for "the
// smaller of these" -- and it is the same rule Rust's f64::min chose.
double neon_f64_min(double a, double b) { return fmin(a, b); }
double neon_f64_max(double a, double b) { return fmax(a, b); }

int64_t neon_i64_abs(int64_t a) {
    if (a == INT64_MIN) {
        neon_trap("integer overflow");
    }
    return a < 0 ? -a : a;
}

// `f64` from `i64` is exact only up to 2^53; beyond that it rounds, like every language
// with these two types. Truncation toward zero the other way, and a value outside the
// integer range traps rather than being undefined -- a C cast there is UB.
double neon_i64_to_f64(int64_t a) { return (double)a; }

int64_t neon_f64_to_i64(double x) {
    if (x != x || x >= 9223372036854775808.0 || x < -9223372036854775808.0) {
        neon_trap("f64 out of i64 range");
    }
    return (int64_t)x;
}

// Fixed-point rendering, for `fmt`. `%g` (what `to_string` uses) is right for "show me
// this number" and wrong for a table, which needs a fixed width.
neon_str neon_f64_to_fixed(double x, int64_t places) {
    if (places < 0) places = 0;
    if (places > 17) places = 17;
    char buf[64];
    int len = snprintf(buf, sizeof buf, "%.*f", (int)places, x);
    return neon_str_new(buf, (size_t)len);
}
