#include "libneon_rt.h"

#include <stdio.h>

// ---- io ----

void neon_io_println(neon_str s) {
    fwrite(neon_str_data(&s), 1, neon_str_len(&s), stdout);
    fputc('\n', stdout);
    neon_str_release(s); // consumes s
}

void neon_io_print(neon_str s) {
    fwrite(neon_str_data(&s), 1, neon_str_len(&s), stdout);
    neon_str_release(s); // consumes s
}

// stderr is unbuffered by default, so a diagnostic written here appears even if the
// program traps before stdout is flushed -- which is when a diagnostic matters most.
void neon_io_eprintln(neon_str s) {
    fwrite(neon_str_data(&s), 1, neon_str_len(&s), stderr);
    fputc('\n', stderr);
    neon_str_release(s); // consumes s
}
