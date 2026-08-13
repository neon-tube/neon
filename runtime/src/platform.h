#ifndef NEON_PLATFORM_H
#define NEON_PLATFORM_H

// The one place the runtime names an operating system.
//
// Everything below is a POSIX call that Windows either spells differently or does not have
// at all. It is deliberately a single seam rather than an `#ifdef` at each call site: the
// interesting code -- the resume-after-a-short-write loop, the grow-and-read loop -- is the
// same on both systems and is written once, in `file.c`, against these.
//
// The contract is POSIX's, because that is the one the callers were written to: a failing
// call returns -1 with `errno` set, and `errno` is what travels back to Neon as `IoError`.
// Windows' CRT sets `errno` for all of these, so the failure channel needs no translation
// even where the call does.
//
// Not installed, like `internal.h`: generated C never sees this file, and nothing here is
// part of the ABI.

#include "neon/portability.h" // NEON_NORETURN

#include <errno.h>
#include <stddef.h>
#include <stdint.h>

// A count of bytes transferred, or -1. `ssize_t` is POSIX and has no Windows spelling;
// `int64_t` says the same thing and is the width the callers already work in (`fd` and the
// error codes are `int64_t` all the way out to Neon).
typedef int64_t neon_ssize;

// `mode` for `neon_plat_open`, matching what `neon_io_open` takes from Neon.
#define NEON_OPEN_READ 0
#define NEON_OPEN_WRITE 1
#define NEON_OPEN_APPEND 2

#ifdef _WIN32

// Before <stdlib.h>: `rand_s` (the CRT's door to the OS CSPRNG) is only declared with it.
// The build sets this globally (runtime/CMakeLists.txt) so it holds even when another header
// reaches <stdlib.h> first; the guard keeps that -D and this fallback from colliding under
// -Werror when platform.h happens to be the first include.
#ifndef _CRT_RAND_S
#define _CRT_RAND_S
#endif

#include <direct.h>
#include <fcntl.h>
#include <io.h>
#include <limits.h>
#include <process.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <windows.h>

// No `writev`, so the vector type is ours. The field names are POSIX's because on the other
// branch this *is* `struct iovec`, which is what lets `file.c` be written once.
typedef struct neon_iovec {
    void* iov_base;
    size_t iov_len;
} neon_iovec;

// Windows' narrow file APIs decode their path in the process's ANSI code page, not UTF-8.
// A Neon `str` is UTF-8, so a path with any byte over 127 would open a different file --
// silently, and only for the users whose filenames are not ASCII. Widening to UTF-16 and
// using the `_w` entry points is the only way to pass the path through unchanged.
//
// The caller frees. NULL means failure with `errno` set, which is the same channel the
// calls themselves use.
static inline wchar_t* neon_plat_widen(const char* utf8) {
    int n = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, utf8, -1, NULL, 0);
    if (n <= 0) {
        errno = EILSEQ; // not valid UTF-8, so it names no file
        return NULL;
    }
    wchar_t* w = (wchar_t*)malloc((size_t)n * sizeof(wchar_t));
    if (w == NULL) {
        errno = ENOMEM;
        return NULL;
    }
    if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, utf8, -1, w, n) <= 0) {
        free(w);
        errno = EILSEQ;
        return NULL;
    }
    return w;
}

static inline int neon_plat_open(const char* path, int mode) {
    // `_O_BINARY` on every mode, and it is not optional: without it the CRT translates
    // LF to CRLF on write and back on read, so a program that writes a byte count reads
    // a different one back, and anything that is not text is corrupted outright. Neon's
    // file API is bytes -- `read_all` returns a `str` measured in bytes -- so translation
    // would be a lie about what was on disk.
    int flags = mode == NEON_OPEN_READ ? (_O_RDONLY | _O_BINARY)
                : mode == NEON_OPEN_WRITE
                    ? (_O_WRONLY | _O_CREAT | _O_TRUNC | _O_BINARY)
                    : (_O_WRONLY | _O_CREAT | _O_APPEND | _O_BINARY);
    wchar_t* w = neon_plat_widen(path);
    if (w == NULL) {
        return -1;
    }
    int fd = _wopen(w, flags, _S_IREAD | _S_IWRITE);
    free(w);
    return fd;
}

static inline int neon_plat_close(int fd) {
    return _close(fd);
}

static inline neon_ssize neon_plat_read(int fd, void* buf, size_t n) {
    // `_read` counts in `unsigned int`. A short read is not an error to any caller here --
    // `read_all` loops until it sees zero -- so clamping is a smaller read, not a failure.
    if (n > (size_t)INT_MAX) n = (size_t)INT_MAX;
    return _read(fd, buf, (unsigned int)n);
}

// One `writev`'s worth of work, as a sequence of `_write`s. Windows has no gathering write
// on a CRT descriptor, so the pieces do reach the kernel separately here -- but they still
// reach it without being concatenated into a temporary, which is the ownership property
// `neon_io_writev` is built on.
//
// The return is `writev`'s: bytes actually transferred, which may be short, and the caller
// resumes from there. Stopping at the first short `_write` rather than pressing on keeps
// that resume point exact.
static inline neon_ssize neon_plat_writev(int fd, neon_iovec* v, int n) {
    neon_ssize total = 0;
    for (int i = 0; i < n; i++) {
        size_t len = v[i].iov_len;
        if (len > (size_t)INT_MAX) len = (size_t)INT_MAX;
        int got = _write(fd, v[i].iov_base, (unsigned int)len);
        if (got < 0) {
            return total > 0 ? total : -1; // a partial write already made progress
        }
        total += got;
        if ((size_t)got < v[i].iov_len) {
            break;
        }
    }
    return total;
}

static inline int neon_plat_exists(const char* path) {
    wchar_t* w = neon_plat_widen(path);
    if (w == NULL) {
        return -1;
    }
    int r = _waccess(w, 0); // 0 == F_OK
    free(w);
    return r;
}

static inline int neon_plat_remove(const char* path) {
    wchar_t* w = neon_plat_widen(path);
    if (w == NULL) {
        return -1;
    }
    int r = _wremove(w);
    free(w);
    return r;
}

// The standard streams carry program output verbatim. The CRT opens them in text mode, so
// every `\n` a Neon program prints would leave as `\r\n` -- which changes the bytes a test
// compares and the length a pipe downstream reads. `println` writing one byte more than it
// was given is not a platform convention worth honouring.
static inline void neon_plat_stdio_binary(void) {
    _setmode(_fileno(stdout), _O_BINARY);
    _setmode(_fileno(stderr), _O_BINARY);
}

// A Win32 error as the closest `errno`. The rest of the runtime reports `-errno` and
// `neon_io_strerror` runs `strerror`, so a raw `GetLastError()` here would surface as a
// number from the wrong namespace with a message about the wrong failure.
static inline int neon_plat_errno_from_win32(DWORD e) {
    switch (e) {
        case ERROR_FILE_NOT_FOUND:
        case ERROR_PATH_NOT_FOUND:
        case ERROR_INVALID_NAME:
            return ENOENT;
        case ERROR_ALREADY_EXISTS:
        case ERROR_FILE_EXISTS:
            return EEXIST;
        case ERROR_ACCESS_DENIED:
        case ERROR_SHARING_VIOLATION:
            return EACCES;
        case ERROR_DIR_NOT_EMPTY:
            return ENOTEMPTY;
        case ERROR_DISK_FULL:
            return ENOSPC;
        case ERROR_NOT_ENOUGH_MEMORY:
        case ERROR_OUTOFMEMORY:
            return ENOMEM;
        default:
            return EIO;
    }
}

// UTF-16 back to UTF-8, the inverse of `neon_plat_widen`. Needed because the directory
// walk hands back wide names and a Neon `str` is UTF-8. Caller frees; NULL sets `errno`.
static inline char* neon_plat_narrow(const wchar_t* w) {
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, NULL, 0, NULL, NULL);
    if (n <= 0) {
        errno = EILSEQ;
        return NULL;
    }
    char* s = (char*)malloc((size_t)n);
    if (s == NULL) {
        errno = ENOMEM;
        return NULL;
    }
    if (WideCharToMultiByte(CP_UTF8, 0, w, -1, s, n, NULL, NULL) <= 0) {
        free(s);
        errno = EILSEQ;
        return NULL;
    }
    return s;
}

static inline int neon_plat_mkdir(const char* path) {
    wchar_t* w = neon_plat_widen(path);
    if (w == NULL) return -1;
    int r = CreateDirectoryW(w, NULL) ? 0 : -1;
    if (r != 0) errno = neon_plat_errno_from_win32(GetLastError());
    free(w);
    return r;
}

// `MOVEFILE_REPLACE_EXISTING` because POSIX `rename` replaces, and a seam whose two sides
// disagree about that would make every caller platform-aware. `MOVEFILE_COPY_ALLOWED` so a
// move across volumes works, which POSIX `rename` does not do -- the seam's contract is the
// weaker of the two, so a caller must not rely on cross-volume moves being atomic.
static inline int neon_plat_rename(const char* from, const char* to) {
    wchar_t* a = neon_plat_widen(from);
    if (a == NULL) return -1;
    wchar_t* b = neon_plat_widen(to);
    if (b == NULL) {
        free(a);
        return -1;
    }
    int r = MoveFileExW(a, b, MOVEFILE_REPLACE_EXISTING | MOVEFILE_COPY_ALLOWED) ? 0 : -1;
    if (r != 0) errno = neon_plat_errno_from_win32(GetLastError());
    free(a);
    free(b);
    return r;
}

// 1 directory, 0 not, -1 with `errno`.
static inline int neon_plat_is_dir(const char* path) {
    wchar_t* w = neon_plat_widen(path);
    if (w == NULL) return -1;
    DWORD a = GetFileAttributesW(w);
    free(w);
    if (a == INVALID_FILE_ATTRIBUTES) {
        errno = neon_plat_errno_from_win32(GetLastError());
        return -1;
    }
    return (a & FILE_ATTRIBUTE_DIRECTORY) ? 1 : 0;
}

static inline int neon_plat_size(const char* path, int64_t* out) {
    wchar_t* w = neon_plat_widen(path);
    if (w == NULL) return -1;
    WIN32_FILE_ATTRIBUTE_DATA d;
    int ok = GetFileAttributesExW(w, GetFileExInfoStandard, &d) ? 0 : -1;
    free(w);
    if (ok != 0) {
        errno = neon_plat_errno_from_win32(GetLastError());
        return -1;
    }
    *out = ((int64_t)d.nFileSizeHigh << 32) | (int64_t)d.nFileSizeLow;
    return 0;
}

// Size, kind and modification time in one attribute query. `is_file` is "not a directory",
// which is the closest the attribute set gets to POSIX's "regular file". `mtime_ms` converts
// the `FILETIME` (100ns ticks since 1601) to Unix milliseconds through the same 1601->1970
// constant `neon_plat_unix_millis` uses.
static inline int neon_plat_stat(const char* path, int64_t* size, int64_t* is_dir,
                                 int64_t* is_file, int64_t* mtime_ms) {
    wchar_t* w = neon_plat_widen(path);
    if (w == NULL) return -1;
    WIN32_FILE_ATTRIBUTE_DATA d;
    int ok = GetFileAttributesExW(w, GetFileExInfoStandard, &d) ? 0 : -1;
    free(w);
    if (ok != 0) {
        errno = neon_plat_errno_from_win32(GetLastError());
        return -1;
    }
    *size = ((int64_t)d.nFileSizeHigh << 32) | (int64_t)d.nFileSizeLow;
    *is_dir = (d.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) ? 1 : 0;
    *is_file = *is_dir ? 0 : 1;
    uint64_t t = ((uint64_t)d.ftLastWriteTime.dwHighDateTime << 32) | d.ftLastWriteTime.dwLowDateTime;
    *mtime_ms = (int64_t)((t - 116444736000000000ULL) / 10000ULL);
    return 0;
}

// The narrow `_getcwd` matches POSIX `getcwd`'s shape closely enough (an `int` length in
// place of `size_t`), and it decodes into the ANSI code page -- acceptable here because the
// result is handed straight back through `neon_os_lossy`, which repairs any non-UTF-8 byte.
static inline char* neon_plat_getcwd(char* buf, size_t n) {
    return _getcwd(buf, (int)n);
}

static inline int neon_plat_chdir(const char* path) {
    wchar_t* w = neon_plat_widen(path);
    if (w == NULL) return -1;
    int r = _wchdir(w);
    free(w);
    return r;
}

static inline int64_t neon_plat_getpid(void) {
    return (int64_t)_getpid();
}

// `GetComputerNameExA` rather than winsock `gethostname`, which would need `WSAStartup`
// first. The DNS host name is the same short name a POSIX `gethostname` returns.
static inline int neon_plat_hostname(char* buf, size_t n) {
    DWORD size = (DWORD)n;
    if (!GetComputerNameExA(ComputerNameDnsHostname, buf, &size)) {
        errno = neon_plat_errno_from_win32(GetLastError());
        return -1;
    }
    return 0;
}

static inline char** neon_plat_environ(void) {
    return _environ;
}

NEON_NORETURN static inline void neon_plat_exit_now(int code) {
    _exit(code);
}

// QueryPerformanceCounter: the monotonic clock Windows actually promises. Scaled to
// nanoseconds through the queried frequency, both cached — they never change.
static inline int64_t neon_plat_monotonic_ns(void) {
    static LARGE_INTEGER freq;
    if (freq.QuadPart == 0) {
        QueryPerformanceFrequency(&freq);
    }
    LARGE_INTEGER t;
    QueryPerformanceCounter(&t);
    return (int64_t)((double)t.QuadPart * (1000000000.0 / (double)freq.QuadPart));
}

static inline int64_t neon_plat_unix_millis(void) {
    // FILETIME epoch is 1601; the offset to 1970 in 100ns units is a constant.
    FILETIME ft;
    GetSystemTimeAsFileTime(&ft);
    uint64_t t = ((uint64_t)ft.dwHighDateTime << 32) | ft.dwLowDateTime;
    return (int64_t)((t - 116444736000000000ULL) / 10000ULL);
}

static inline void neon_plat_sleep_ms(int64_t ms) {
    if (ms > 0) {
        Sleep((DWORD)ms);
    }
}

static inline void neon_plat_ignore_sigpipe(void) {
}

// `rand_s` reaches the OS CSPRNG through the CRT without pulling in bcrypt.
static inline int64_t neon_plat_entropy(void) {
    unsigned int hi = 0, lo = 0;
    rand_s(&hi);
    rand_s(&lo);
    return (int64_t)(((uint64_t)hi << 32) | lo);
}

#else

#include <signal.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

#include <fcntl.h>
#include <stdio.h> // remove, rename
#include <sys/stat.h>
#include <sys/types.h>

typedef struct iovec neon_iovec;

static inline int neon_plat_open(const char* path, int mode) {
    int flags = mode == NEON_OPEN_READ ? O_RDONLY
                : mode == NEON_OPEN_WRITE
                    ? (O_WRONLY | O_CREAT | O_TRUNC)
                    : (O_WRONLY | O_CREAT | O_APPEND);
    return open(path, flags, 0666);
}

static inline int neon_plat_close(int fd) {
    return close(fd);
}

static inline neon_ssize neon_plat_read(int fd, void* buf, size_t n) {
    return read(fd, buf, n);
}

static inline neon_ssize neon_plat_writev(int fd, neon_iovec* v, int n) {
    return writev(fd, v, n);
}

static inline int neon_plat_exists(const char* path) {
    return access(path, F_OK);
}

static inline int neon_plat_remove(const char* path) {
    return remove(path);
}

static inline int neon_plat_mkdir(const char* path) {
    return mkdir(path, 0777);
}

static inline int neon_plat_rename(const char* from, const char* to) {
    return rename(from, to);
}

// 1 directory, 0 not, -1 with `errno`.
static inline int neon_plat_is_dir(const char* path) {
    struct stat st;
    if (stat(path, &st) != 0) return -1;
    return S_ISDIR(st.st_mode) ? 1 : 0;
}

static inline int neon_plat_size(const char* path, int64_t* out) {
    struct stat st;
    if (stat(path, &st) != 0) return -1;
    *out = (int64_t)st.st_size;
    return 0;
}

// Size, kind and modification time in one `stat`. `is_file` is "regular file", so a
// directory, symlink target that is a directory, socket or device answers 0 to both
// questions -- a caller asking "is it a plain file" gets a straight no rather than a
// surprise. `mtime_ms` is `st_mtime` (whole seconds) scaled to milliseconds, which is the
// resolution POSIX guarantees everywhere; the sub-second `st_mtim` is not portable enough
// to promise.
static inline int neon_plat_stat(const char* path, int64_t* size, int64_t* is_dir,
                                 int64_t* is_file, int64_t* mtime_ms) {
    struct stat st;
    if (stat(path, &st) != 0) return -1;
    *size = (int64_t)st.st_size;
    *is_dir = S_ISDIR(st.st_mode) ? 1 : 0;
    *is_file = S_ISREG(st.st_mode) ? 1 : 0;
    *mtime_ms = (int64_t)st.st_mtime * 1000;
    return 0;
}

// The process's current directory into `buf`; NULL with `errno` (ERANGE when `buf` is too
// small, which the caller answers by growing and retrying). Same shape as POSIX `getcwd`.
static inline char* neon_plat_getcwd(char* buf, size_t n) {
    return getcwd(buf, n);
}

static inline int neon_plat_chdir(const char* path) {
    return chdir(path);
}

static inline int64_t neon_plat_getpid(void) {
    return (int64_t)getpid();
}

// The host name into `buf`, always NUL-terminated even when the kernel truncates (POSIX
// leaves termination unspecified on truncation, so this pins it). 0, or -1 with `errno`.
static inline int neon_plat_hostname(char* buf, size_t n) {
    if (n == 0) {
        errno = EINVAL;
        return -1;
    }
    if (gethostname(buf, n) != 0) {
        return -1;
    }
    buf[n - 1] = '\0';
    return 0;
}

// The environment block: a NULL-terminated array of `KEY=VALUE` strings. `std::os` walks
// it and splits each on the first `=`.
static inline char** neon_plat_environ(void) {
    extern char** environ;
    return environ;
}

// Nothing to do: a POSIX stream never translates.
static inline void neon_plat_stdio_binary(void) {
}

NEON_NORETURN static inline void neon_plat_exit_now(int code) {
    _exit(code);
}

static inline void neon_plat_ignore_sigpipe(void) {
    signal(SIGPIPE, SIG_IGN);
}

static inline int64_t neon_plat_monotonic_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + (int64_t)ts.tv_nsec;
}

static inline int64_t neon_plat_unix_millis(void) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return (int64_t)ts.tv_sec * 1000 + (int64_t)ts.tv_nsec / 1000000;
}

static inline void neon_plat_sleep_ms(int64_t ms) {
    if (ms <= 0) {
        return;
    }
    struct timespec ts = {(time_t)(ms / 1000), (long)(ms % 1000) * 1000000};
    // EINTR resumes with the remainder, so a signal does not shorten the sleep.
    while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
    }
}

// `getentropy` over reading /dev/urandom: no descriptor to manage, and it is in every
// libc this runtime meets (glibc 2.25+, musl, the BSDs, macOS).
static inline int64_t neon_plat_entropy(void) {
    uint64_t v = 0;
    if (getentropy(&v, sizeof v) != 0) {
        // The one fallback that needs no unavailable machinery: the clock. Weak, and
        // says so — a failed getentropy on a supported platform is already strange.
        v = (uint64_t)neon_plat_monotonic_ns() ^ ((uint64_t)neon_plat_unix_millis() << 17);
    }
    return (int64_t)v;
}

#endif

#endif
