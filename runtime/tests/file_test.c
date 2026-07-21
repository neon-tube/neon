// `runtime/src/file.c`: the descriptor-based file natives. Failure travels as a value --
// every fallible call returns `-errno`, and `read_all` hands its status back through an
// out-parameter -- so these tests assert on returned codes, never on a hidden flag.
//
// Each test works against its own `mkstemp` file under `/tmp` and removes it at the end, so
// the suite leaves nothing behind and two runs never contend. tinyunit forks per test, so
// even a trap inside one leaves the others' files untouched.

#include "tinyunit.h"

#include <errno.h>
#include <stdlib.h>
#include <unistd.h>

#include "support.h"

TEST_SUITE("file");

// A unique, existing, empty temp file. `path` must hold at least 32 bytes. The caller owns
// cleanup -- every test here ends by removing it (or has already, when it tests removal).
static void tmp_path(char* path) {
    __builtin_strcpy(path, "/tmp/neon_rt_file_XXXXXX");
    int fd = mkstemp(path);
    if (fd >= 0) close(fd);
}

// `neon_io_open` consumes its path, so a test that opens the same file twice needs a fresh
// `neon_str` each time. This borrows nothing the runtime keeps.
static neon_str path_str(const char* p) { return nt_owned(p); }

TEST(write_then_read_round_trips) {
    char path[32];
    tmp_path(path);

    // Write two pieces as one `writev`; the runtime concatenates them into the file.
    int64_t fd = neon_io_open(path_str(path), 1); // write, truncate
    EXPECT(fd >= 0);
    neon_list* parts = neon_list_new(&nt_str_w);
    neon_str a = nt_owned("hello, ");
    neon_str b = nt_owned("world");
    parts = neon_list_push(parts, &a);
    parts = neon_list_push(parts, &b);
    EXPECT_EQ(neon_io_writev(fd, parts), 0); // consumes parts
    EXPECT_EQ(neon_io_close(fd), 0);

    // Read it all back.
    int64_t rfd = neon_io_open(path_str(path), 0); // read
    EXPECT(rfd >= 0);
    int64_t err = -1;
    neon_str got = neon_io_read_all(rfd, &err);
    EXPECT_EQ(err, 0);
    EXPECT(nt_str_is(got, "hello, world"));
    neon_str_release(got);
    EXPECT_EQ(neon_io_close(rfd), 0);

    EXPECT_EQ(neon_io_remove(path_str(path)), 0);
}

TEST(append_mode_adds_to_the_end) {
    char path[32];
    tmp_path(path);

    int64_t fd = neon_io_open(path_str(path), 1); // write, truncate
    neon_list* first = neon_list_new(&nt_str_w);
    neon_str ab = nt_owned("AB");
    first = neon_list_push(first, &ab);
    EXPECT_EQ(neon_io_writev(fd, first), 0);
    EXPECT_EQ(neon_io_close(fd), 0);

    fd = neon_io_open(path_str(path), 2); // append
    neon_list* second = neon_list_new(&nt_str_w);
    neon_str cd = nt_owned("CD");
    second = neon_list_push(second, &cd);
    EXPECT_EQ(neon_io_writev(fd, second), 0);
    EXPECT_EQ(neon_io_close(fd), 0);

    int64_t rfd = neon_io_open(path_str(path), 0);
    int64_t err = -1;
    neon_str got = neon_io_read_all(rfd, &err);
    EXPECT_EQ(err, 0);
    EXPECT(nt_str_is(got, "ABCD")); // appended, not truncated
    neon_str_release(got);
    neon_io_close(rfd);

    EXPECT_EQ(neon_io_remove(path_str(path)), 0);
}

TEST(writev_skips_empty_pieces) {
    char path[32];
    tmp_path(path);

    int64_t fd = neon_io_open(path_str(path), 1);
    neon_list* parts = neon_list_new(&nt_str_w);
    neon_str x = nt_owned("x");
    neon_str empty = nt_owned(""); // a zero-length piece is not a write
    neon_str yz = nt_owned("yz");
    parts = neon_list_push(parts, &x);
    parts = neon_list_push(parts, &empty);
    parts = neon_list_push(parts, &yz);
    EXPECT_EQ(neon_io_writev(fd, parts), 0);
    neon_io_close(fd);

    int64_t rfd = neon_io_open(path_str(path), 0);
    int64_t err = -1;
    neon_str got = neon_io_read_all(rfd, &err);
    EXPECT(nt_str_is(got, "xyz")); // the empty piece left no gap
    neon_str_release(got);
    neon_io_close(rfd);

    EXPECT_EQ(neon_io_remove(path_str(path)), 0);
}

TEST(read_all_of_an_empty_file_is_the_empty_string) {
    char path[32];
    tmp_path(path); // mkstemp leaves it empty

    int64_t fd = neon_io_open(path_str(path), 0);
    int64_t err = -1;
    neon_str got = neon_io_read_all(fd, &err);
    EXPECT_EQ(err, 0);
    EXPECT_EQ(neon_str_len(&got), 0u);
    neon_str_release(got);
    neon_io_close(fd);

    EXPECT_EQ(neon_io_remove(path_str(path)), 0);
}

TEST(opening_a_missing_file_for_read_fails_with_enoent) {
    char path[32];
    tmp_path(path);
    EXPECT_EQ(neon_io_remove(path_str(path)), 0); // now the path names nothing

    int64_t fd = neon_io_open(path_str(path), 0);
    EXPECT(fd < 0);            // the failure is a negative code, not a valid descriptor
    EXPECT_EQ(fd, -ENOENT);    // and it carries the errno as a value
}

TEST(closing_a_bad_descriptor_returns_errno) {
    EXPECT_EQ(neon_io_close(-1), -EBADF);
}

TEST(exists_then_remove) {
    char path[32];
    tmp_path(path);

    EXPECT(neon_io_exists(path_str(path)));
    EXPECT_EQ(neon_io_remove(path_str(path)), 0);
    EXPECT(!neon_io_exists(path_str(path)));
    EXPECT(neon_io_remove(path_str(path)) < 0); // removing what is gone reports -errno
}

TEST(strerror_renders_a_code_ignoring_sign) {
    // The runtime hands back whatever the C library says for the code; the exact wording is
    // the platform's, so the oracle is the platform's own `strerror`.
    neon_str pos = neon_io_strerror(ENOENT);
    EXPECT(nt_str_is(pos, strerror(ENOENT)));
    neon_str_release(pos);

    // A code arrives as `-errno` from every other call, so the sign must not matter.
    neon_str neg = neon_io_strerror(-ENOENT);
    EXPECT(nt_str_is(neg, strerror(ENOENT)));
    neon_str_release(neg);
}
