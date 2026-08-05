#include "libneon_rt.h"

#include "internal.h"
#include "platform.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifndef _WIN32
#include <dirent.h>
#include <sys/stat.h>
#include <sys/types.h>
#endif
#ifndef ENOSYS
#define ENOSYS 38
#endif

// The batch size for `writev`. `IOV_MAX` is only visible under feature-test macros we do
// not set, and a *smaller* batch is always valid -- the call just runs more than once -- so
// this pins the value POSIX guarantees Linux provides rather than probing for it.
#define NEON_IOV_MAX 1024

// A NUL-terminated copy of a `neon_str`, for the C APIs that demand one. `neon_str` is a
// length-delimited *view* -- a slice of a larger buffer is not terminated -- so this cannot
// be skipped by passing the data pointer. Caller frees.
static char* neon_cstr(neon_str s) {
    size_t len = neon_str_len(&s);
    char* p = (char*)malloc(len + 1);
    if (p == NULL) neon_trap("out of memory");
    // `&s` -- the parameter, which lives until this function returns. Under SSO that is
    // where an inline string's bytes are, so the pointer must be derived from this copy
    // and not from whatever the caller passed.
    if (len) memcpy(p, neon_str_data(&s), len);
    p[len] = 0;
    return p;
}
// ---- files ----
//
// Descriptors, not `FILE*`: `writev` wants one, and buffering here would only sit between
// the caller's iolist and the kernel.
//
// Failure travels as a *value*. Every fallible call returns `-errno`, and the ones that
// also return data use an out-parameter, which codegen turns into a Neon tuple (see
// `emit_native_out`). An earlier draft kept an errno-style flag in a static; any
// intervening call could clobber it and it said nothing at the call site.
//
// `neon_io_strerror` is a pure function of a code, so rendering a failure needs no state
// at all.

// `mode`: 0 read, 1 write (truncate), 2 append. The open flags stay behind `platform.h`
// because they are platform constants -- and on Windows one of them, `_O_BINARY`, is a
// correctness requirement rather than a spelling.
int64_t neon_io_open(neon_str path, int64_t mode) {
    char* p = neon_cstr(path);
    int fd = neon_plat_open(p, (int)mode);
    int64_t r = fd < 0 ? -(int64_t)errno : (int64_t)fd;
    free(p);
    neon_str_release(path);
    return r;
}

// A bare descriptor: the armed flag that stops a double close lives in the `Resource`
// wrapping this, not here.
int64_t neon_io_close(int64_t fd) {
    return neon_plat_close((int)fd) != 0 ? -(int64_t)errno : 0;
}

neon_str neon_io_strerror(int64_t code) {
    const char* m = strerror(code < 0 ? (int)-code : (int)code);
    return neon_str_new(m, strlen(m));
}

// Everything left in the descriptor. `err` is the out-parameter: 0, or `-errno`. The data
// and the status come back together rather than through hidden state.
neon_str neon_io_read_all(int64_t fd, int64_t* err) {
    *err = 0;
    size_t cap = 4096, len = 0;
    char* buf = (char*)malloc(cap);
    if (buf == NULL) neon_trap("out of memory");
    for (;;) {
        if (len == cap) {
            cap *= 2;
            char* grown = (char*)realloc(buf, cap);
            if (grown == NULL) neon_trap("out of memory");
            buf = grown;
        }
        neon_ssize got = neon_plat_read((int)fd, buf + len, cap - len);
        if (got < 0) {
            if (errno == EINTR) continue;
            *err = -(int64_t)errno;
            break;
        }
        if (got == 0) break;
        len += (size_t)got;
    }
    neon_str r = neon_str_new(buf, len);
    free(buf);
    return r;
}

// Write a list of `neon_str` views as one `writev`, so the pieces reach the kernel without
// ever being concatenated. `neon_str` is `{data, len, owner}` and `neon_iovec` is
// `{iov_base, iov_len}` -- the first two fields line up, but the strides differ, so this
// copies pointer/length pairs and never a payload byte.
//
// Longer lists go in batches, and a short write resumes where it stopped rather than
// counting as failure -- that is the contract `writev` actually offers.
int64_t neon_io_writev(int64_t fd, neon_list* parts) {
    const neon_str* items = (const neon_str*)parts->data;
    int64_t status = 0;
    size_t i = 0;
    while (status == 0 && i < parts->len) {
        size_t batch = parts->len - i;
        if (batch > NEON_IOV_MAX) batch = NEON_IOV_MAX;
        neon_iovec vec[NEON_IOV_MAX];
        size_t n = 0, total = 0;
        for (size_t j = 0; j < batch; j++) {
            size_t plen = neon_str_len(&items[i + j]);
            if (plen == 0) continue; // an empty piece is not a write
            // Borrowed for the duration of the `writev` below. Sound because `items`
            // points into the list's buffer, which outlives this call and is not mutated
            // here -- so under SSO an inline piece's bytes stay put too. Copying the
            // `neon_str` into a local first would NOT be sound: the local dies at the end
            // of this iteration while the iovec is read after the loop.
            vec[n].iov_base = (void*)(uintptr_t)neon_str_data(&items[i + j]);
            vec[n].iov_len = plen;
            total += plen;
            n++;
        }
        i += batch;
        size_t done = 0, first = 0;
        while (done < total) {
            neon_ssize got = neon_plat_writev((int)fd, vec + first, (int)(n - first));
            if (got < 0) {
                if (errno == EINTR) continue;
                status = -(int64_t)errno;
                break;
            }
            done += (size_t)got;
            size_t adv = (size_t)got;
            while (first < n && adv >= vec[first].iov_len) {
                adv -= vec[first].iov_len;
                first++;
            }
            if (first < n && adv) {
                vec[first].iov_base = (char*)vec[first].iov_base + adv;
                vec[first].iov_len -= adv;
            }
        }
    }
    neon_release((neon_header*)parts);
    return status;
}

int64_t neon_io_remove(neon_str path) {
    char* p = neon_cstr(path);
    int64_t r = neon_plat_remove(p) == 0 ? 0 : -(int64_t)errno;
    free(p);
    neon_str_release(path);
    return r;
}

// ---- directories and metadata ----
//
// POSIX bodies, with Windows stubs that return `-ENOSYS`. The stubs exist so the runtime
// still COMPILES and LINKS on Windows -- the `runtime-windows` CI job builds this file --
// while the Neon side refuses to call them at all: `std::fs`'s Windows branch is a
// `TODO(..)`, which is a compile error naming what is missing. So a Windows program cannot
// reach these, and if the Neon guard were ever removed the failure would still be an errno
// rather than undefined behaviour.

int64_t neon_io_mkdir(neon_str path) {
    char* p = neon_cstr(path);
#ifdef _WIN32
    (void)p;
    int64_t r = -(int64_t)ENOSYS;
#else
    int64_t r = mkdir(p, 0777) == 0 ? 0 : -(int64_t)errno;
#endif
    free(p);
    neon_str_release(path);
    return r;
}

int64_t neon_io_rename(neon_str from, neon_str to) {
    char* a = neon_cstr(from);
    char* b = neon_cstr(to);
#ifdef _WIN32
    (void)a;
    (void)b;
    int64_t r = -(int64_t)ENOSYS;
#else
    int64_t r = rename(a, b) == 0 ? 0 : -(int64_t)errno;
#endif
    free(a);
    free(b);
    neon_str_release(from);
    neon_str_release(to);
    return r;
}

// Kind, as its own call: 1 directory, 0 not, negative -errno. Split from `size` rather than
// returned together through out-parameters because two plain `int64_t` natives need no
// witness and no tuple, and the pair is never read atomically anyway -- anything that cared
// would have to hold the file open.
int64_t neon_io_is_dir(neon_str path) {
    char* p = neon_cstr(path);
#ifdef _WIN32
    (void)p;
    int64_t r = -(int64_t)ENOSYS;
#else
    struct stat st;
    int64_t r = stat(p, &st) != 0 ? -(int64_t)errno : (S_ISDIR(st.st_mode) ? 1 : 0);
#endif
    free(p);
    neon_str_release(path);
    return r;
}

// The size in bytes, or negative -errno.
int64_t neon_io_size(neon_str path) {
    char* p = neon_cstr(path);
#ifdef _WIN32
    (void)p;
    int64_t r = -(int64_t)ENOSYS;
#else
    struct stat st;
    int64_t r = stat(p, &st) != 0 ? -(int64_t)errno : (int64_t)st.st_size;
#endif
    free(p);
    neon_str_release(path);
    return r;
}

// The entries of a directory, NUL-separated, excluding `.` and `..`.
//
// A single string rather than a list because a runtime function cannot build one: a
// `List[str]` needs the element's value-witness, which codegen generates per type and only
// hands to the natives it special-cases. NUL is the one byte a POSIX filename cannot
// contain, so splitting on it in Neon is lossless -- and `std::string` is byte-oriented, so
// a NUL inside a `str` is ordinary data.
//
// Names, not paths: joining them onto the directory is `std::path`'s job, and doing it here
// would bake a separator into the runtime.
neon_str neon_io_read_dir(neon_str path, int64_t* err) {
    char* p = neon_cstr(path);
    *err = 0;
#ifdef _WIN32
    (void)p;
    *err = -(int64_t)ENOSYS;
    free(p);
    neon_str_release(path);
    return neon_str_new("", 0);
#else
    // The buffer lives inside this branch, not above the `#ifdef`: on Windows it would be
    // unused, and the runtime builds with `-Werror`.
    char* buf = NULL;
    size_t len = 0;
    size_t cap = 0;
    DIR* d = opendir(p);
    if (d == NULL) {
        *err = -(int64_t)errno;
    } else {
        struct dirent* e;
        while ((e = readdir(d)) != NULL) {
            if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) continue;
            size_t n = strlen(e->d_name);
            if (len + n + 1 > cap) {
                cap = (len + n + 1) * 2;
                buf = (char*)realloc(buf, cap);
                if (buf == NULL) neon_trap("out of memory");
            }
            memcpy(buf + len, e->d_name, n);
            len += n;
            buf[len++] = 0;
        }
        closedir(d);
    }
    free(p);
    neon_str_release(path);
    // Drop the trailing NUL so the split does not produce an empty last entry.
    neon_str out = neon_str_new(buf == NULL ? "" : buf, len == 0 ? 0 : len - 1);
    free(buf);
    return out;
#endif
}

bool neon_io_exists(neon_str path) {
    char* p = neon_cstr(path);
    bool ok = neon_plat_exists(p) == 0;
    free(p);
    neon_str_release(path);
    return ok;
}
