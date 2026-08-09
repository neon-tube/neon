// `runtime/src/random.c`: the generator is a pure function of its state, and these pin
// the properties the Neon-side contract leans on — determinism by seed, bounded values
// in range with the empty range trapping, floats in [0, 1).

#include "tinyunit.h"

#include "support.h"

TEST_SUITE("random");

TEST(same_seed_same_sequence) {
    int64_t a1, a2, a3, b1, b2, b3;
    int64_t s0a = neon_rng_seed(42, &a1, &a2, &a3);
    int64_t s0b = neon_rng_seed(42, &b1, &b2, &b3);
    EXPECT_EQ(s0a, s0b);
    EXPECT_EQ(a3, b3);
    int64_t x1, x2, x3, x4, y1, y2, y3, y4;
    int64_t va = neon_rng_next(s0a, a1, a2, a3, &x1, &x2, &x3, &x4);
    int64_t vb = neon_rng_next(s0b, b1, b2, b3, &y1, &y2, &y3, &y4);
    EXPECT_EQ(va, vb);
    EXPECT_EQ(x4, y4);
}

TEST(different_seeds_diverge) {
    int64_t a1, a2, a3, b1, b2, b3;
    int64_t s0a = neon_rng_seed(1, &a1, &a2, &a3);
    int64_t s0b = neon_rng_seed(2, &b1, &b2, &b3);
    int64_t x1, x2, x3, x4, y1, y2, y3, y4;
    int64_t va = neon_rng_next(s0a, a1, a2, a3, &x1, &x2, &x3, &x4);
    int64_t vb = neon_rng_next(s0b, b1, b2, b3, &y1, &y2, &y3, &y4);
    EXPECT(va != vb);
}

TEST(seeding_never_yields_the_dead_state) {
    // The all-zero state is xoshiro's fixed point; splitmix64 seeding must avoid it,
    // zero seed included.
    int64_t s1, s2, s3;
    int64_t s0 = neon_rng_seed(0, &s1, &s2, &s3);
    EXPECT(s0 != 0 || s1 != 0 || s2 != 0 || s3 != 0);
}

TEST(bounded_stays_in_range) {
    int64_t s1, s2, s3;
    int64_t s0 = neon_rng_seed(7, &s1, &s2, &s3);
    for (int i = 0; i < 1000; i++) {
        int64_t o0, o1, o2, o3;
        int64_t v = neon_rng_bounded(s0, s1, s2, s3, 6, &o0, &o1, &o2, &o3);
        EXPECT(v >= 0 && v < 6);
        s0 = o0;
        s1 = o1;
        s2 = o2;
        s3 = o3;
    }
}

TEST(an_empty_range_traps) {
    int64_t o0, o1, o2, o3;
    EXPECT_TRAP((void)neon_rng_bounded(1, 2, 3, 4, 0, &o0, &o1, &o2, &o3));
    EXPECT_TRAP((void)neon_rng_bounded(1, 2, 3, 4, -5, &o0, &o1, &o2, &o3));
}

TEST(floats_live_in_the_unit_interval) {
    int64_t s1, s2, s3;
    int64_t s0 = neon_rng_seed(9, &s1, &s2, &s3);
    for (int i = 0; i < 1000; i++) {
        int64_t o0, o1, o2, o3;
        double v = neon_rng_float(s0, s1, s2, s3, &o0, &o1, &o2, &o3);
        EXPECT(v >= 0.0 && v < 1.0);
        s0 = o0;
        s1 = o1;
        s2 = o2;
        s3 = o3;
    }
}

TEST(the_clocks_behave) {
    int64_t m1 = neon_time_monotonic();
    int64_t m2 = neon_time_monotonic();
    EXPECT(m2 >= m1);
    EXPECT(neon_time_unix_millis() > 1600000000000); // after 2020
    int64_t began = neon_time_monotonic();
    neon_time_sleep(5);
    EXPECT(neon_time_monotonic() - began >= 5000000);
    neon_time_sleep(0); // returns at once, no error
    int64_t e1 = neon_os_entropy();
    int64_t e2 = neon_os_entropy();
    EXPECT(e1 != e2); // 2^-64 flake odds, worth the assertion
}
