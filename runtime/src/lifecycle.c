#include "libneon_rt.h"

#include "internal.h"
#include "platform.h"

#include <stdlib.h>

#ifndef NEON_CBMC
#include <pthread.h> // the slab's chunk registry lock (refill-time only)
#endif

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

// The retain/release bodies are MEASURED SURFACE and admit no additions: an extra branch
// with a TLS load cost 30% on binary-trees (the teardown experiment), an inlined atomic
// arm 44%, and even an out-of-line cold call reachable from the guard arm 33% — a call
// anywhere in the inlined body forces spill setup around the whole drop recursion. The
// M:N answer is therefore NOT a dynamic shared-bit here: copy-on-send lands values in the
// RECEIVER'S ARENA (the original design), so ordinary objects are never cross-thread and
// this path never changes; only the enumerable HANDLE types (channels, tasks) need atomic
// counts, through their own entry points at repr-known sites. See docs/design/fibers.md.
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

// The HANDLE pair: channels and tasks are the only objects whose count is touched by more
// than one live holder across scheduler threads, and every site that retains or releases
// one knows the repr at emission time — codegen emits these calls for handle reprs, the
// runtime's own handle sites call them directly, and the generic pair above never learns
// atomics exist (it measured 33-44% when taught; see the comment there).
void neon_retain_shared(neon_header* h) {
    if (h == NULL) {
        return;
    }
    __atomic_fetch_add(&h->rc, 1, __ATOMIC_RELAXED);
}

void neon_release_shared(neon_header* h) {
    if (h == NULL) {
        return;
    }
    // Release ordering so the dropping thread sees every write made under the references
    // it reclaims; acquire pairs on the zero transition.
    if (__atomic_sub_fetch(&h->rc, 1, __ATOMIC_ACQ_REL) == 0) {
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

// PER-THREAD, not static, and it is what makes the slab safe under M:N with zero locks:
// each worker allocates from its own lists, and a slot freed on a different thread than it
// was carved on simply migrates — pushed onto the freeing thread's list, reused there. The
// memory stays valid (chunks are never returned to the OS before exit), ownership of the
// slot just moves. Single-threaded programs see exactly the old behavior through one
// initial-exec TLS indirection. Chunks are additionally threaded onto a LOCKED global
// registry — refill-time only, never per-allocation — so the exit teardown (below, LSan's
// sake) can free every thread's chunks from wherever the destructor runs.
static _Thread_local NEON_TLS_IE void* neon_free_list[NEON_SLAB_CLASSES];
static void* neon_slab_chunks; // global registry of every thread's chunks, for exit teardown
static pthread_mutex_t neon_slab_registry_mu = PTHREAD_MUTEX_INITIALIZER;

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
    pthread_mutex_lock(&neon_slab_registry_mu);
    *(void**)chunk = neon_slab_chunks;
    neon_slab_chunks = chunk;
    pthread_mutex_unlock(&neon_slab_registry_mu);

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

// The slab proper, shared by the plain path and the shared-heap (sentinel) path; `extra`
// carries the SHARED flag for the latter.
static inline void* neon_slab_alloc(size_t bytes, void (*drop)(void*), uint32_t extra) {
    size_t total = sizeof(neon_header) + bytes;
    neon_header* h;
    if (total > NEON_SLAB_MAX) {
        h = malloc(total);
        if (h == NULL) {
            neon_trap("out of memory");
        }
        h->flags = NEON_ALLOC_BIG | extra;
    } else {
        size_t cls = neon_class_of(total);
        void* blk = neon_free_list[cls];
        if (blk == NULL) {
            blk = neon_slab_refill(cls);
        } else {
            neon_free_list[cls] = *(void**)blk;
        }
        h = (neon_header*)blk;
        h->flags = (uint32_t)((cls + 1) << NEON_ALLOC_CLASS_SHIFT) | extra;
    }
    h->rc = 1;
    h->drop = drop;
    return h;
}

NEON_NOINLINE void* neon_alloc(size_t bytes, void (*drop)(void*)) {
    // Inside a fiber, allocation comes from that fiber's own arena (isolation: the object
    // lives, and will be freed, in exactly one arena — see docs/design/fibers.md). Under
    // the SEND ROUTING the arena is the shared sentinel and the object lands on the slab
    // flagged SHARED (atomic rc — it may cross scheduler threads). Off-fiber — the root
    // context, and every program with no fibers at all — the arena is NULL and the slab
    // path runs unchanged, one predictable branch the cost.
    neon_arena* arena = neon_current_arena;
    if (arena != NULL) {
        if (arena == NEON_SHARED_ARENA) {
            return neon_slab_alloc(bytes, drop, NEON_ALLOC_SHARED);
        }
        return neon_arena_alloc(arena, bytes, drop);
    }
    return neon_slab_alloc(bytes, drop, 0);
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
    // Plain slab, NO shared flag: staging copies (a buffered send, a spawn argument, an env
    // copy, a task result) have SEQUENTIAL ownership — the producer creates, the container
    // owns, exactly one consumer takes over — so their counts are never touched by two
    // threads at once and plain rc is sound even under M:N (the channel lock is the
    // visibility barrier). Only HANDLES need atomic counts; they allocate through
    // neon_shared_routing_begin below.
    neon_current_arena = NULL;
    return saved;
}

void* neon_shared_routing_begin(void) {
    neon_arena* saved = neon_current_arena;
    neon_current_arena = NEON_SHARED_ARENA; // slab + SHARED flag: concurrent-rc objects
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
