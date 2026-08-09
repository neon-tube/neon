#ifndef NEON_RANDOM_H
#define NEON_RANDOM_H

// xoshiro256++ as pure state-in, state-out natives; `std::random` threads the four
// words through an opaque record. The tuple convention: first element is the C return,
// the rest are out-parameters.

#include <stdint.h>

int64_t neon_rng_seed(int64_t seed, int64_t* s1, int64_t* s2, int64_t* s3);
int64_t neon_rng_next(int64_t s0, int64_t s1, int64_t s2, int64_t s3, int64_t* o0,
                      int64_t* o1, int64_t* o2, int64_t* o3);
int64_t neon_rng_bounded(int64_t s0, int64_t s1, int64_t s2, int64_t s3, int64_t n,
                         int64_t* o0, int64_t* o1, int64_t* o2, int64_t* o3); // traps n <= 0
double neon_rng_float(int64_t s0, int64_t s1, int64_t s2, int64_t s3, int64_t* o0,
                      int64_t* o1, int64_t* o2, int64_t* o3);
int64_t neon_os_entropy(void); // the module's one effect

#endif
