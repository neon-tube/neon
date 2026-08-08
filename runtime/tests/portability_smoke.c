// A standalone program that links the shipped archive and checks the three things a
// Windows port gets wrong.
//
// NOT part of the tinyunit suite, and deliberately not listed in `CMakeLists.txt`: that
// suite runs every test in a forked process, which is exactly what Windows cannot do, so
// the one platform these properties matter most on is the one that cannot run them there.
// This file has its own `main`, links `libneon_rt.a` like a real program, and needs
// nothing but a C compiler. CI drives it (`.github/workflows/ci.yml`); it is inert to
// every other build.
//
// Each check is a bug that was real before the port:
//
//   * a `\n` written to stdout leaving the process as `\r\n`, because a Windows CRT opens
//     the standard streams in text mode. The caller checks the byte count, because inside
//     the process there is nothing to see;
//   * a file gaining a byte per line on disk, for the same reason on a descriptor. Checked
//     with stdio's `"rb"` rather than by reading it back through the runtime: a text-mode
//     write and a text-mode read cancel out, so a round trip through one implementation
//     agrees with itself no matter what it did to the file. Only an outside reader can
//     tell;
//   * a path with non-ASCII bytes naming a *different* file, because the narrow file APIs
//     decode their argument in the process's ANSI code page and a Neon `str` is UTF-8.
//
// The third is a round trip and not a sharp check, and it is worth being clear about that:
// it catches a widening that fails outright, not one that is consistently wrong, because
// create/find/remove through the same broken conversion all agree on the same wrong name.
// Naming the file from outside the process is the only thing that would settle it, and
// that needs the wide Win32 API this file deliberately does not reach for.
//
// It passes trivially on Linux and macOS, which is the point of running it there too: the
// assertions are about behaviour the runtime promises everywhere, not about Windows.

#include "libneon_rt.h"

#include <stdio.h>

// The element witness for a `List[str]` of literals. Literals have no owner, so
// retain/release are no-ops and NULL is the honest spelling; nothing here is compared or
// ordered.
static const neon_witness str_w = {sizeof(neon_str), NULL, NULL, NULL, NULL};

// The bytes written, and what they must still weigh on disk.
#define SMOKE_BODY_LEN 8

// "漢字.txt" in UTF-8: ten bytes, four of them over 127.
#define SMOKE_WIDE_PATH "\xe6\xbc\xa2\xe5\xad\x97.txt"
#define SMOKE_WIDE_PATH_LEN 10

// ASCII, so stdio can open the same file the runtime wrote and act as the outside reader.
#define SMOKE_ASCII_PATH "neon_smoke.txt"
#define SMOKE_ASCII_PATH_LEN 14

static int fail(const char* what) {
    fprintf(stderr, "portability_smoke: %s\n", what);
    return 1;
}

// Write "one\ntwo\n" to `path` as two pieces through the gathering write, which on Windows
// is a loop of `_write`. Returns 0, or non-zero having already reported why.
static int write_two_lines(neon_str path) {
    int64_t fd = neon_io_open(path, 1);
    if (fd < 0) {
        return fail("could not create the file");
    }
    neon_list* parts = neon_list_new(&str_w);
    neon_str one = neon_str_lit("one\n", 4);
    neon_str two = neon_str_lit("two\n", 4);
    parts = neon_list_push(parts, &one);
    parts = neon_list_push(parts, &two);
    if (neon_io_writev(fd, parts) != 0) {
        return fail("writev failed");
    }
    if (neon_io_close(fd) != 0) {
        return fail("close failed");
    }
    return 0;
}

// What the file actually weighs, according to a reader that is not the runtime.
static long on_disk_size(const char* path) {
    FILE* f = fopen(path, "rb");
    if (f == NULL) {
        return -1;
    }
    if (fseek(f, 0, SEEK_END) != 0) {
        fclose(f);
        return -1;
    }
    long n = ftell(f);
    fclose(f);
    return n;
}

int main(void) {
    neon_rt_init(0, NULL);

    // ---- the bytes on disk are the bytes that were written ----
    if (write_two_lines(neon_str_lit(SMOKE_ASCII_PATH, SMOKE_ASCII_PATH_LEN)) != 0) {
        return 1;
    }
    long size = on_disk_size(SMOKE_ASCII_PATH);
    if (size != SMOKE_BODY_LEN) {
        fprintf(stderr, "portability_smoke: wrote %d bytes, the file holds %ld\n",
                SMOKE_BODY_LEN, size);
        return 1;
    }

    int64_t err = 0;
    int64_t fd = neon_io_open(neon_str_lit(SMOKE_ASCII_PATH, SMOKE_ASCII_PATH_LEN), 0);
    if (fd < 0) {
        return fail("could not reopen the file");
    }
    neon_str back = neon_io_read_all(fd, &err);
    neon_io_close(fd);
    if (err != 0 || neon_str_len(&back) != SMOKE_BODY_LEN) {
        fprintf(stderr, "portability_smoke: read back %d bytes of %d (err=%d)\n",
                (int)neon_str_len(&back), SMOKE_BODY_LEN, (int)err);
        return 1;
    }
    neon_str_release(back);
    if (neon_io_remove(neon_str_lit(SMOKE_ASCII_PATH, SMOKE_ASCII_PATH_LEN)) != 0) {
        return fail("remove failed");
    }

    // ---- a non-ASCII path survives every call that takes one ----
    if (write_two_lines(neon_str_lit(SMOKE_WIDE_PATH, SMOKE_WIDE_PATH_LEN)) != 0) {
        return 1;
    }
    if (!neon_io_exists(neon_str_lit(SMOKE_WIDE_PATH, SMOKE_WIDE_PATH_LEN))) {
        return fail("the path that created the file does not find it again");
    }
    if (neon_io_remove(neon_str_lit(SMOKE_WIDE_PATH, SMOKE_WIDE_PATH_LEN)) != 0) {
        return fail("remove failed on the non-ASCII path");
    }
    if (neon_io_exists(neon_str_lit(SMOKE_WIDE_PATH, SMOKE_WIDE_PATH_LEN))) {
        return fail("the file survived remove");
    }

    // ---- stdout is not translated ----
    // The caller checks that this reached it as exactly three bytes.
    neon_io_println(neon_str_lit("ok", 2));
    return 0;
}
