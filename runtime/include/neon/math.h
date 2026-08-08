#ifndef NEON_MATH_H
#define NEON_MATH_H

// Math: IEEE for f64 -- no traps, NaN and infinity propagate. The i64 entries here are the
// non-trapping ones (abs, conversions); the trapping operators live in `neon/arith.h`.

#include <stdbool.h>
#include <stdint.h>

#include "neon/core.h"

double neon_f64_sqrt(double x);
double neon_f64_pow(double a, double b);
double neon_f64_floor(double x);
double neon_f64_ceil(double x);
double neon_f64_round(double x);
double neon_f64_abs(double x);
bool neon_f64_is_nan(double x);
bool neon_f64_is_infinite(double x);
double neon_f64_ln(double x);
double neon_f64_log10(double x);
double neon_f64_exp(double x);
double neon_f64_sin(double x);
double neon_f64_cos(double x);
double neon_f64_tan(double x);
double neon_f64_atan2(double y, double x);
double neon_f64_min(double a, double b); // fmin: a NaN yields the other operand
double neon_f64_max(double a, double b);
int64_t neon_i64_abs(int64_t a);
double neon_i64_to_f64(int64_t a);
int64_t neon_f64_to_i64(double x);
neon_str neon_f64_to_fixed(double x, int64_t places);

#endif
