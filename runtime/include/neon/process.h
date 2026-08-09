#ifndef NEON_PROCESS_H
#define NEON_PROCESS_H

// Child processes. Failure is `-errno` (a child's own exec failure included — see the
// report-pipe note in process.c); stream modes are 0 inherit / 1 piped / 2 null,
// matching `std::process`'s atoms. Windows: everything answers -ENOSYS today.

#include <stdint.h>

#include "neon/core.h"
#include "neon/list.h"

// (pid, in_fd, out_fd, err_fd); fds are -1 unless that stream was piped. Consumes
// program/cwd/args/envs. `envs` are "KEY=VALUE" strings EXTENDING the inherited
// environment; "" for `cwd` inherits.
int64_t neon_proc_spawn(neon_str program, neon_list* args, neon_list* envs, neon_str cwd,
                        int64_t in_mode, int64_t out_mode, int64_t err_mode, int64_t* in_fd,
                        int64_t* out_fd, int64_t* err_fd);

// Spawn all-piped, pump stdin/stdout/stderr with poll (deadlock-free), reap. Returns
// the exit code, or -errno when the spawn failed; *out/*err_s always set.
int64_t neon_proc_run(neon_str program, neon_list* args, neon_list* envs, neon_str cwd,
                      neon_str input, neon_str* out, neon_str* err_s);

int64_t neon_proc_wait(int64_t pid); // exit code, 128+signal, or -errno
int64_t neon_proc_kill(int64_t pid); // SIGKILL; 0 or -errno

// One read, up to `max` bytes (clamped to 1 MiB); "" with *err == 0 is end-of-stream.
neon_str neon_proc_read_some(int64_t fd, int64_t max, int64_t* err);

#endif
