// `runtime/src/io.c`: `print`/`println`/`eprintln`. Each writes a `neon_str` to a standard
// stream and consumes it. To assert on what reached the stream, a test redirects the stream
// to a temp file for the duration of the call, then reads the bytes back. tinyunit forks per
// test, so the redirect never escapes into another test or into the harness's own output.

#include "tinyunit.h"

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#include "support.h"

TEST_SUITE("io");

// Point `stream` at a fresh temp file, returning the saved descriptor to restore later. The
// chosen path is written into `path` (>= 32 bytes).
static int capture_begin(FILE* stream, char* path) {
    __builtin_strcpy(path, "/tmp/neon_rt_io_XXXXXX");
    int t = mkstemp(path);
    if (t >= 0) close(t);
    int saved = dup(fileno(stream));
    (void)!freopen(path, "w", stream);
    return saved;
}

// Restore `stream` to `saved`, then read the captured bytes into `buf` (capacity `n`) and
// return how many there were. The temp file is removed.
static size_t capture_end(FILE* stream, int saved, const char* path, char* buf, size_t n) {
    fflush(stream);
    dup2(saved, fileno(stream));
    close(saved);
    FILE* f = fopen(path, "r");
    size_t got = f ? fread(buf, 1, n, f) : 0;
    if (f) fclose(f);
    remove(path);
    return got;
}

TEST(println_writes_the_string_then_a_newline) {
    char path[32], buf[64];
    int saved = capture_begin(stdout, path);
    neon_io_println(nt_owned("hi")); // consumes the string
    size_t n = capture_end(stdout, saved, path, buf, sizeof buf);
    EXPECT_EQ(n, 3u);
    EXPECT_EQ(__builtin_memcmp(buf, "hi\n", 3), 0);
}

TEST(print_writes_the_string_without_a_newline) {
    char path[32], buf[64];
    int saved = capture_begin(stdout, path);
    neon_io_print(nt_owned("abc"));
    size_t n = capture_end(stdout, saved, path, buf, sizeof buf);
    EXPECT_EQ(n, 3u);
    EXPECT_EQ(__builtin_memcmp(buf, "abc", 3), 0);
}

TEST(eprintln_writes_to_stderr_with_a_newline) {
    char path[32], buf[64];
    int saved = capture_begin(stderr, path);
    neon_io_eprintln(nt_owned("oops"));
    size_t n = capture_end(stderr, saved, path, buf, sizeof buf);
    EXPECT_EQ(n, 5u);
    EXPECT_EQ(__builtin_memcmp(buf, "oops\n", 5), 0);
}

TEST(println_of_the_empty_string_is_just_a_newline) {
    char path[32], buf[64];
    int saved = capture_begin(stdout, path);
    neon_io_println(nt_owned(""));
    size_t n = capture_end(stdout, saved, path, buf, sizeof buf);
    EXPECT_EQ(n, 1u);
    EXPECT_EQ(buf[0], '\n');
}
