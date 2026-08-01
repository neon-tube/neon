// `runtime/src/io.c`: `print`/`println`/`eprintln`. Each writes a `neon_str` to a standard
// stream and consumes it. To assert on what reached the stream, a test redirects the stream
// to a temp file for the duration of the call, then reads the bytes back. tinyunit runs
// each test in its own process, so the redirect never escapes into another test or into
// the harness's own output.

#include "tinyunit.h"

#include <stdio.h>
#include <stdlib.h>

#include "support.h"

TEST_SUITE("io");

// Point `stream` at a fresh temp file, returning the saved descriptor to restore later. The
// chosen path is written into `path` (capacity `NT_PATH_MAX`).
//
// **"wb", not "w", and it is load-bearing.** These tests count bytes: `println("hi")` must
// be exactly three. `neon_rt_init` puts the standard streams in binary mode for precisely
// that reason, but `freopen` reopens the stream and takes its mode from *this* string, so a
// "w" here would silently hand text mode back and turn every `\n` the runtime wrote into
// two bytes on a Windows CRT. The reader below is "rb" for the same reason.
static int capture_begin(FILE* stream, char* path) {
    nt_temp_path(path, "io");
    int saved = nt_dup(nt_fileno(stream));
    (void)!freopen(path, "wb", stream);
    return saved;
}

// Restore `stream` to `saved`, then read the captured bytes into `buf` (capacity `n`) and
// return how many there were. The temp file is removed.
static size_t capture_end(FILE* stream, int saved, const char* path, char* buf, size_t n) {
    fflush(stream);
    nt_dup2(saved, nt_fileno(stream));
    nt_close(saved);
    FILE* f = fopen(path, "rb");
    size_t got = f ? fread(buf, 1, n, f) : 0;
    if (f) fclose(f);
    remove(path);
    return got;
}

TEST(println_writes_the_string_then_a_newline) {
    char path[NT_PATH_MAX], buf[64];
    int saved = capture_begin(stdout, path);
    neon_io_println(nt_owned("hi")); // consumes the string
    size_t n = capture_end(stdout, saved, path, buf, sizeof buf);
    EXPECT_EQ(n, 3u);
    EXPECT_EQ(memcmp(buf, "hi\n", 3), 0);
}

TEST(print_writes_the_string_without_a_newline) {
    char path[NT_PATH_MAX], buf[64];
    int saved = capture_begin(stdout, path);
    neon_io_print(nt_owned("abc"));
    size_t n = capture_end(stdout, saved, path, buf, sizeof buf);
    EXPECT_EQ(n, 3u);
    EXPECT_EQ(memcmp(buf, "abc", 3), 0);
}

TEST(eprintln_writes_to_stderr_with_a_newline) {
    char path[NT_PATH_MAX], buf[64];
    int saved = capture_begin(stderr, path);
    neon_io_eprintln(nt_owned("oops"));
    size_t n = capture_end(stderr, saved, path, buf, sizeof buf);
    EXPECT_EQ(n, 5u);
    EXPECT_EQ(memcmp(buf, "oops\n", 5), 0);
}

TEST(println_of_the_empty_string_is_just_a_newline) {
    char path[NT_PATH_MAX], buf[64];
    int saved = capture_begin(stdout, path);
    neon_io_println(nt_owned(""));
    size_t n = capture_end(stdout, saved, path, buf, sizeof buf);
    EXPECT_EQ(n, 1u);
    EXPECT_EQ(buf[0], '\n');
}
