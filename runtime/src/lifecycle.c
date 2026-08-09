#include "libneon_rt.h"

#include "platform.h"

#include <stdlib.h>

// ---- lifecycle ----

void neon_rt_init(int argc, char** argv) {
    // Arguments are initialization state, not a separate registration step: taking them
    // here makes "initialized but argless" unrepresentable. A caller with no command
    // line — the C test suites — passes (0, NULL) and `os::args()` reads as empty.
    neon_os_set_args(argc, argv);
    // Writing a closed pipe must be an ERROR, not sudden death: `std::process` hands
    // out pipe ends, a child can exit any time, and the default SIGPIPE would kill this
    // process for writing to one. Ignored, `write` returns EPIPE and the error flows
    // through the same IoError channel as everything else. A no-op on Windows, which
    // has no SIGPIPE to ignore.
    neon_plat_ignore_sigpipe();
    // The standard streams, unmodified. A Windows CRT opens them in text mode, so every
    // `\n` `println` writes would leave the process as `\r\n` -- one more byte than the
    // program produced, which changes what a pipe reads and what a golden file matches.
    // The rest of the runtime writes bytes (`neon_io_writev` goes to a descriptor opened
    // `_O_BINARY`), and stdout should not be the one stream that disagrees. A no-op
    // everywhere else.
    neon_plat_stdio_binary();
}

void neon_retain(neon_header* h) {
    if (h == NULL || (h->flags & NEON_IMMORTAL)) {
        return;
    }
    h->rc++;
}

void neon_release(neon_header* h) {
    if (h == NULL || (h->flags & NEON_IMMORTAL)) {
        return;
    }
    if (--h->rc == 0) {
        h->drop(h);
    }
}

void* neon_alloc(size_t bytes, void (*drop)(void*)) {
    neon_header* h = malloc(sizeof(neon_header) + bytes);
    if (h == NULL) {
        neon_trap("out of memory");
    }
    h->rc = 1;
    h->flags = 0;
    h->drop = drop;
    return h;
}

void neon_free(void* p) {
    free(p);
}
