// The generator behind `std::random`: xoshiro256++, the state as four i64 the Neon side
// threads through an opaque record. Nothing here is stateful — every native takes the
// state and hands the successor back through out-parameters (the tuple convention) — so
// the one place randomness IS an effect, acquiring entropy for a seed, is one function
// deep and everything else is `@pure` and means it.
//
// xoshiro256++ because it is the boring right answer: 256 bits of state (four words, a
// cheap record), fast (a rotate and some xors), passes BigCrush, and its authors supply
// the exact seeding discipline used here — splitmix64 over the seed, which cannot
// produce the all-zero state xoshiro must never sit in.

#include "libneon_rt.h"

#include "platform.h"

#include <stdint.h>

static inline uint64_t rotl64(uint64_t x, int k) {
    return (x << k) | (x >> (64 - k));
}

static inline uint64_t splitmix64(uint64_t* s) {
    uint64_t z = (*s += 0x9E3779B97F4A7C15ULL);
    z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ULL;
    z = (z ^ (z >> 27)) * 0x94D049BB133111EBULL;
    return z ^ (z >> 31);
}

// (s0, s1, s2, s3) from one seed. Same seed, same state, forever: this is the
// reproducibility contract `random::new` makes.
int64_t neon_rng_seed(int64_t seed, int64_t* s1, int64_t* s2, int64_t* s3) {
    uint64_t sm = (uint64_t)seed;
    int64_t s0 = (int64_t)splitmix64(&sm);
    *s1 = (int64_t)splitmix64(&sm);
    *s2 = (int64_t)splitmix64(&sm);
    *s3 = (int64_t)splitmix64(&sm);
    return s0;
}

static inline uint64_t step(uint64_t s[4]) {
    uint64_t result = rotl64(s[0] + s[3], 23) + s[0];
    uint64_t t = s[1] << 17;
    s[2] ^= s[0];
    s[3] ^= s[1];
    s[1] ^= s[2];
    s[0] ^= s[3];
    s[2] ^= t;
    s[3] = rotl64(s[3], 45);
    return result;
}

// (value, s0', s1', s2', s3'): the raw 64 bits, any of them.
int64_t neon_rng_next(int64_t s0, int64_t s1, int64_t s2, int64_t s3, int64_t* o0,
                      int64_t* o1, int64_t* o2, int64_t* o3) {
    uint64_t s[4] = {(uint64_t)s0, (uint64_t)s1, (uint64_t)s2, (uint64_t)s3};
    uint64_t v = step(s);
    *o0 = (int64_t)s[0];
    *o1 = (int64_t)s[1];
    *o2 = (int64_t)s[2];
    *o3 = (int64_t)s[3];
    return (int64_t)v;
}

// (value in [0, n), state'). Lemire's multiply-then-rejection rather than modulo: `% n`
// over-represents the low residues whenever 2^64 is not a multiple of `n`, and the whole
// point of a seeded generator is that its distribution is what the algorithm says.
// Traps on `n <= 0` — an element of an empty range is a mistake, exactly as dividing by
// zero is, and a quiet 0 would be a wrong answer wearing a plausible one's face.
int64_t neon_rng_bounded(int64_t s0, int64_t s1, int64_t s2, int64_t s3, int64_t n,
                         int64_t* o0, int64_t* o1, int64_t* o2, int64_t* o3) {
    if (n <= 0) {
        neon_trap("random range is empty");
    }
    uint64_t s[4] = {(uint64_t)s0, (uint64_t)s1, (uint64_t)s2, (uint64_t)s3};
    uint64_t bound = (uint64_t)n;
    // Reject while the low half of v * bound falls under the threshold, answer with the
    // high half. The 128-bit multiply is a gcc/clang builtin; both families have it.
    uint64_t threshold = (0 - bound) % bound;
    __uint128_t m = (__uint128_t)step(s) * bound;
    while ((uint64_t)m < threshold) {
        m = (__uint128_t)step(s) * bound;
    }
    *o0 = (int64_t)s[0];
    *o1 = (int64_t)s[1];
    *o2 = (int64_t)s[2];
    *o3 = (int64_t)s[3];
    return (int64_t)(uint64_t)(m >> 64);
}

// (value in [0, 1), state'): the top 53 bits over 2^53 — every double in the range is
// reachable and none is favoured.
double neon_rng_float(int64_t s0, int64_t s1, int64_t s2, int64_t s3, int64_t* o0,
                      int64_t* o1, int64_t* o2, int64_t* o3) {
    uint64_t s[4] = {(uint64_t)s0, (uint64_t)s1, (uint64_t)s2, (uint64_t)s3};
    uint64_t v = step(s);
    *o0 = (int64_t)s[0];
    *o1 = (int64_t)s[1];
    *o2 = (int64_t)s[2];
    *o3 = (int64_t)s[3];
    return (double)(v >> 11) * (1.0 / 9007199254740992.0);
}

// The one effect in the module: 64 bits from the OS, for `random::from_entropy`.
int64_t neon_os_entropy(void) {
    return neon_plat_entropy();
}
