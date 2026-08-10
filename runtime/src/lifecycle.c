#include "libneon_rt.h"

#include "internal.h"
#include "platform.h"

#include <stdlib.h>

// The current fiber's arena (declared in internal.h). Defined here, in the always-compiled
// lifecycle unit, so every build links it even where the fiber sources are absent; it stays
// NULL until the scheduler sets it, and NULL means "use the global slab" below. Not defined
// under NEON_CBMC — the models take the plain-malloc path and never build the fiber code.
#ifndef NEON_CBMC
_Thread_local NEON_TLS_IE neon_arena* neon_current_arena = NULL;
_Thread_local NEON_TLS_IE neon_arena* neon_teardown_arena = NULL;
#endif

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
#ifndef NEON_CBMC
    // Teardown mode (see internal.h): an internal reference in a dying arena is neither
    // counted down nor dropped — the bulk-free reclaims it, and the no-op is what makes
    // the walk's forced drops exactly-once. The cost outside teardown: nothing for a
    // non-arena object (`flags` is already loaded for the immortal test), one predictable
    // initial-exec TLS load for an arena one.
    if ((h->flags & NEON_ALLOC_ARENA) && neon_teardown_arena != NULL) {
        return;
    }
#endif
    if (--h->rc == 0) {
        h->drop(h);
    }
}

// ---- the allocator ----
//
// Small heap objects come from a segregated-free-list slab, not straight from `malloc`.
// The measured reason: on binary-trees ~60% of the run was glibc's bin machinery
// (`_int_malloc`/`_int_free`), on 67M short-lived Nodes. A slab makes allocation a
// free-list pop and freeing a push — no size search, no coalescing — which is the whole
// cost when the objects are tiny and same-sized, exactly the shape a refcounted language
// produces. It also recycles a just-freed block straight back, so the drop-tree-i /
// build-tree-i+1 loop reuses cache-hot slots: FBIP reuse arriving by the runtime door,
// where a compiler reuse-token pass cannot reach because alloc and free are in different
// functions.
//
// The class is recovered on free from the header's `flags` (NEON_ALLOC_*), so there is no
// per-object overhead and no aligned-slab pointer arithmetic; a block too big for any
// class (>512 bytes, a long string say) goes straight to `malloc` and is marked
// NEON_ALLOC_BIG. Slabs are never returned to the OS — the runtime has no GC and the free
// list recycles within the process — so there is a destructor purely to keep LeakSanitizer
// honest at exit.
//
// Under CBMC (`NEON_CBMC`, passed by runtime/models/CMakeLists.txt) this collapses to plain `malloc`/`free`: the models verify the
// refcount CONTRACT `neon_alloc` provides (a fresh header, rc 1) and the drop semantics,
// which the slab does not change, and a global-free-list allocator is exactly the
// stateful, unbounded-loop shape a model checker is worst at. The slab's own integrity —
// right class, no overlap, no double-serve — is covered instead by ASan/LSan over the
// whole corpus, which is the tool that actually catches a slab bug.

#ifdef NEON_CBMC

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

#else

// 16-byte granularity up to 512 bytes: 32 classes. 16 is the minimum because a freed
// block holds a `next` pointer, and the step matches the header's own 8-byte alignment
// doubled, so every block is suitably aligned for a `neon_header`.
#define NEON_SLAB_GRAIN 16
#define NEON_SLAB_CLASSES 32
#define NEON_SLAB_MAX (NEON_SLAB_GRAIN * NEON_SLAB_CLASSES)
#define NEON_SLAB_CHUNK (64 * 1024) // one malloc carves this many bytes into blocks

static void* neon_free_list[NEON_SLAB_CLASSES];
static void* neon_slab_chunks; // linked list of raw chunks, freed only at exit

static size_t neon_class_of(size_t total) {
    return (total + NEON_SLAB_GRAIN - 1) / NEON_SLAB_GRAIN - 1;
}

static size_t neon_class_size(size_t cls) {
    return (cls + 1) * NEON_SLAB_GRAIN;
}

// Carve a fresh chunk into blocks of class `cls`, thread all but the first onto the free
// list, and return the first for the caller to hand out. The chunk's own first word links
// it into `neon_slab_chunks` for teardown; blocks start after that.
static void* neon_slab_refill(size_t cls) {
    char* chunk = malloc(NEON_SLAB_CHUNK);
    if (chunk == NULL) {
        neon_trap("out of memory");
    }
    *(void**)chunk = neon_slab_chunks;
    neon_slab_chunks = chunk;

    size_t bsz = neon_class_size(cls);
    char* base = chunk + NEON_SLAB_GRAIN; // past the chunk link, still bsz-aligned
    size_t n = (NEON_SLAB_CHUNK - NEON_SLAB_GRAIN) / bsz;
    for (size_t i = 1; i < n; i++) {
        void* blk = base + i * bsz;
        *(void**)blk = neon_free_list[cls];
        neon_free_list[cls] = blk;
    }
    return base;
}

NEON_NOINLINE void* neon_alloc(size_t bytes, void (*drop)(void*)) {
    // Inside a fiber, allocation comes from that fiber's own arena (isolation: the object
    // lives, and will be freed, in exactly one arena — see docs/design/fibers.md). Off-fiber
    // — the root context, and every program with no fibers at all — this is NULL and the
    // slab path below runs unchanged, one predictable branch the cost.
    neon_arena* arena = neon_current_arena;
    if (arena != NULL) {
        return neon_arena_alloc(arena, bytes, drop);
    }

    size_t total = sizeof(neon_header) + bytes;
    neon_header* h;
    if (total > NEON_SLAB_MAX) {
        h = malloc(total);
        if (h == NULL) {
            neon_trap("out of memory");
        }
        h->flags = NEON_ALLOC_BIG;
    } else {
        size_t cls = neon_class_of(total);
        void* blk = neon_free_list[cls];
        if (blk == NULL) {
            blk = neon_slab_refill(cls);
        } else {
            neon_free_list[cls] = *(void**)blk;
        }
        h = (neon_header*)blk;
        h->flags = (uint32_t)((cls + 1) << NEON_ALLOC_CLASS_SHIFT);
    }
    h->rc = 1;
    h->drop = drop;
    return h;
}

NEON_NOINLINE void neon_free(void* p) {
    neon_header* h = (neon_header*)p;
    // An arena block goes back to the arena it came from. Under isolation that arena is the
    // current one: a fiber only ever frees its own objects, and its objects live in its own
    // (current) arena, so `neon_current_arena` is the owner. Objects that outlive their fiber
    // or cross to another (shared/big values) are a different, flagged path — slice 5.
    if (h->flags & NEON_ALLOC_ARENA) {
        // In teardown the walk runs on the scheduler's context (current arena NULL) but
        // forced drops still free their own objects — route those to the dying arena.
        neon_arena* owner = neon_teardown_arena != NULL ? neon_teardown_arena : neon_current_arena;
        neon_arena_free(owner, p);
        return;
    }
    if (h->flags & NEON_ALLOC_BIG) {
        free(h);
        return;
    }
    size_t cls = ((h->flags & NEON_ALLOC_CLASS_MASK) >> NEON_ALLOC_CLASS_SHIFT) - 1;
    *(void**)p = neon_free_list[cls];
    neon_free_list[cls] = p;
}

// Return every slab chunk to the OS at normal exit. Not for correctness — the process is
// ending — but so LeakSanitizer's end-of-run check does not flag the retained chunks. On
// a trap/`_exit` path this does not run, and neither does the leak check, so the two stay
// consistent. GNU C only; MSVC does not run LSan, where the leak would be harmless anyway.
#if defined(__GNUC__)
__attribute__((destructor)) static void neon_slab_teardown(void) {
    void* c = neon_slab_chunks;
    while (c != NULL) {
        void* next = *(void**)c;
        free(c);
        c = next;
    }
}
#endif

#endif

// ---- sendability helpers (see neon/core.h's witness `copy` contract) ----

void neon_wcopy_unsendable(const void* src, void* dst) {
    (void)src;
    (void)dst;
    neon_trap("this value cannot be sent between fibers (closures and resources do not cross)");
}

#ifndef NEON_CBMC
void* neon_send_routing_begin(void) {
    neon_arena* saved = neon_current_arena;
    neon_current_arena = NULL; // every allocation until end lands in the shared slab
    return saved;
}

void neon_send_routing_end(void* saved) {
    neon_current_arena = (neon_arena*)saved;
}
#else
// The models never build the fiber sources and have no current arena to route around.
void* neon_send_routing_begin(void) { return NULL; }
void neon_send_routing_end(void* saved) { (void)saved; }
#endif
