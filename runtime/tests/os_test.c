// `runtime/src/os.c`: the process interface and its lossy UTF-8 boundary. The converter
// itself is static, so it is exercised through `neon_os_arg` and `neon_os_env_get` — the
// two doors OS bytes actually come through. Each test plants its own argv/environment;
// tinyunit's per-test process keeps that from leaking into the next test.

#include "tinyunit.h"

#include <stdlib.h>

#include "support.h"

TEST_SUITE("os");

// Setting an environment variable, portably: `setenv` is POSIX, `_putenv_s` is the CRT.
static void set_env(const char* k, const char* v) {
#ifdef _WIN32
    _putenv_s(k, v);
#else
    setenv(k, v, 1);
#endif
}

TEST(args_are_stashed_and_read_back) {
    char* argv[] = {"prog", "alpha", "beta"};
    neon_os_set_args(3, argv);
    EXPECT_EQ(neon_os_argc(), 3);
    EXPECT(nt_str_is(neon_os_arg(0), "prog"));
    EXPECT(nt_str_is(neon_os_arg(2), "beta"));
}

TEST(a_valid_arg_is_zero_copy) {
    char* argv[] = {"prog", "héllo\xF0\x9F\x92\xA1"};
    neon_os_set_args(2, argv);
    neon_str s = neon_os_arg(1);
    // Immortal literal over argv itself: same bytes, no owner to free.
    EXPECT_EQ((const char*)neon_str_data(&s), argv[1]);
    EXPECT(s.owner == NULL);
}

TEST(arg_out_of_range_traps) {
    char* argv[] = {"prog"};
    neon_os_set_args(1, argv);
    EXPECT_TRAP((void)neon_os_arg(1));
    EXPECT_TRAP((void)neon_os_arg(-1));
}

// A lossy result is an owned copy, so this captures, compares and releases; the
// all-valid cases above get literals, where release is a no-op.
static bool arg_is(int64_t i, const char* want) {
    neon_str s = neon_os_arg(i);
    bool ok = nt_str_is(s, want);
    neon_str_release(s);
    return ok;
}

TEST(an_invalid_byte_becomes_the_replacement_character) {
    char* argv[] = {"prog", "a\xffz"};
    neon_os_set_args(2, argv);
    EXPECT(arg_is(1, "a\xEF\xBF\xBDz"));
}

TEST(overlong_and_surrogate_encodings_do_not_pass) {
    // C0 80 is overlong NUL; ED A0 80 is a UTF-16 surrogate; F5 starts nothing. Each
    // byte of a rejected sequence is replaced on its own — none of them may reach a
    // `str` looking valid.
    char* argv[] = {"prog", "\xC0\x80", "\xED\xA0\x80", "\xF5"};
    neon_os_set_args(4, argv);
    EXPECT(arg_is(1, "\xEF\xBF\xBD\xEF\xBF\xBD"));
    EXPECT(arg_is(2, "\xEF\xBF\xBD\xEF\xBF\xBD\xEF\xBF\xBD"));
    EXPECT(arg_is(3, "\xEF\xBF\xBD"));
}

TEST(a_sequence_truncated_by_end_of_string_is_replaced) {
    char* argv[] = {"prog", "ok\xE2\x82"}; // E2 82 needs a third byte that never comes
    neon_os_set_args(2, argv);
    EXPECT(arg_is(1, "ok\xEF\xBF\xBD\xEF\xBF\xBD"));
}

TEST(env_round_trips_and_distinguishes_unset) {
    set_env("NEON_RT_TEST_VAR", "value");
    EXPECT(neon_os_has_env(nt_owned("NEON_RT_TEST_VAR")));
    neon_str v = neon_os_env_get(nt_owned("NEON_RT_TEST_VAR"));
    EXPECT(nt_str_is(v, "value"));
    neon_str_release(v);
    EXPECT(!neon_os_has_env(nt_owned("NEON_RT_TEST_NOBODY_SETS_THIS")));
}

TEST(an_env_value_is_owned_not_a_view) {
    set_env("NEON_RT_TEST_OWNED", "copy me");
    neon_str v = neon_os_env_get(nt_owned("NEON_RT_TEST_OWNED"));
    // `getenv`'s buffer has no lifetime promise, so the value must be a copy.
    EXPECT(v.owner != NULL);
    EXPECT(nt_str_is(v, "copy me"));
    neon_str_release(v);
}

TEST(a_name_with_an_embedded_nul_is_unset) {
    // `getenv` reads to the first NUL; answering for the truncated prefix would be
    // answering a different question, so the name reads as unset instead.
    set_env("AB", "trap");
    EXPECT(!neon_os_has_env(neon_str_new("AB\0CD", 5)));
}
