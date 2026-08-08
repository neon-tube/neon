#ifndef NEON_OS_H
#define NEON_OS_H

// The process interface — arguments, environment, exit — and the stdin reads behind
// `std::io`. The reads live here rather than in `neon/io.h` because they are in `os.c`,
// sharing its lossy UTF-8 boundary (every byte string crossing from the OS into a `str`
// goes through one converter), and the header-per-translation-unit rule wins over
// grouping by stream direction.

#include <stdbool.h>
#include <stdint.h>

#include "neon/core.h"
#include "neon/portability.h" // NEON_NORETURN

// Stashes the pointers the C `main` received. Not called by emitted code directly —
// `neon_rt_init` takes (argc, argv) and forwards here, so arguments are part of runtime
// initialization and cannot be forgotten separately.
void neon_os_set_args(int argc, char** argv);

int64_t neon_os_argc(void);
neon_str neon_os_arg(int64_t i); // traps out of range; `std::os` loops argc, not users

bool neon_os_has_env(neon_str name);     // consumes name
neon_str neon_os_env_get(neon_str name); // consumes name; "" when unset — guard with has_env

NEON_NORETURN void neon_os_exit(int64_t code);

neon_str neon_io_read_line_raw(void); // includes the trailing newline; "" only at EOF
neon_str neon_io_read_all_stdin(void);

#endif
