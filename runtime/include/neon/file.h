#ifndef NEON_FILE_H
#define NEON_FILE_H

// Files: descriptors, and only descriptors.
//
// The *handle* is `opaque record File` on the Neon side, holding a
// `Resource[i64, IoError]` -- so refcounted cleanup, the armed flag and use-after-close
// detection all come from `neon_resource` (see `neon/resource.h`) rather than being
// open-coded here.
//
// Failure is a value (`-errno`); the one call that returns data as well uses an
// out-parameter, which codegen turns into a tuple.

#include <stdbool.h>
#include <stdint.h>

#include "neon/core.h"
#include "neon/list.h" // neon_io_writev takes the parts as a list

int64_t neon_io_open(neon_str path, int64_t mode);      // consumes path; fd or -errno
int64_t neon_io_close(int64_t fd);                      // 0 or -errno
neon_str neon_io_read_all(int64_t fd, int64_t* err);    // *err: 0 or -errno
int64_t neon_io_writev(int64_t fd, neon_list* parts);   // consumes parts; 0 or -errno
int64_t neon_io_remove(neon_str path);                  // consumes path; 0 or -errno
bool neon_io_exists(neon_str path);                     // consumes path
neon_str neon_io_strerror(int64_t code);                // pure: a code, not hidden state

// Directories and metadata. POSIX-backed; the Windows builds return `-ENOSYS`, and
// `std::fs` refuses to call them there with a `TODO(..)` compile error.
int64_t neon_io_mkdir(neon_str path);                   // consumes path; 0 or -errno
int64_t neon_io_rename(neon_str from, neon_str to);     // consumes both; 0 or -errno
int64_t neon_io_is_dir(neon_str path);                  // 1 dir, 0 not, or -errno
int64_t neon_io_size(neon_str path);                    // bytes, or -errno
// Size, kind and mtime in one call. Status is the return (0 or -errno); size, is_dir,
// is_file and mtime-in-millis come back through the out-parameters. Consumes path.
int64_t neon_io_stat(neon_str path, int64_t* size, int64_t* is_dir, int64_t* is_file,
                     int64_t* mtime_ms);
// Entry NAMES, NUL-separated, without `.` and `..`. *err: 0 or -errno.
neon_str neon_io_read_dir(neon_str path, int64_t* err);

#endif
