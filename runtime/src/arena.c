// The per-fiber arena allocator. See `neon/arena.h` and `docs/design/fibers.md`.
//
// The shape mirrors the global slab (`lifecycle.c`) — same 16-byte size classes, the class
// stashed in the header's `flags` — but as an INSTANCE you create and drop, and with a bump
// pointer for fresh space rather than carving a whole chunk into one class up front. That
// matters here: a per-fiber arena is small (4 KB first chunk), so pre-carving would waste
// most of it; bump-any-size-then-reclaim-by-class keeps a tiny arena dense.
//
// Why memory plateaus (the property the design leans on): a freed slot goes back to its
// class list and is reused, so a new chunk is taken only when every existing slot of the
// needed class is live. Arena footprint is therefore the sum over classes of each class's
// high-water mark of simultaneously-live objects — it stops growing once each class peaks,
// and is bounded by the fiber's peak per-class working set. Pure bump (no reclaim) would
// charge TOTAL allocation and leak a long-lived fiber; size classes charge PEAK LIVE and
// cannot.

#include "libneon_rt.h"

#include <stdlib.h>

// Same class scheme as the slab: 16-byte grain, 32 classes, 512-byte ceiling. A block is at
// least 16 bytes, which holds the `next` pointer a free-list slot needs, and 16 is twice the
// header's 8-byte alignment, so every block is `neon_header`-aligned.
#define NEON_ARENA_GRAIN 16
#define NEON_ARENA_CLASSES 32
#define NEON_ARENA_MAX (NEON_ARENA_GRAIN * NEON_ARENA_CLASSES)

// The first chunk is small — an idle fiber should not cost 64 KB — and the arena grows by
// more chunks of the same size. 4 KB is one page.
#define NEON_ARENA_CHUNK 4096

typedef struct neon_arena_chunk {
    struct neon_arena_chunk* next;
    // usable bytes follow, aligned: the header of a chunk is one pointer, and the bump
    // region starts after it, kept 16-aligned by the padded size below.
} neon_arena_chunk;

// The chunk header rounded up to the grain, so the bump region past it stays 16-aligned.
#define NEON_ARENA_CHUNK_HDR NEON_ARENA_GRAIN

// A big allocation, threaded on its own list so drop can free it. The payload's
// `neon_header` sits right after this node, so the object pointer handed out is `node + 1`.
// `total` is the whole malloc size (node + header + payload), kept so a free can decrement
// the footprint by exactly what the alloc added.
typedef struct neon_arena_big {
    struct neon_arena_big* next;
    size_t total;
} neon_arena_big;

struct neon_arena {
    void* free_list[NEON_ARENA_CLASSES]; // reclaimed slots, per class
    char* bump;                          // next free byte in the current chunk
    char* bump_end;                      // end of the current chunk's bump region
    neon_arena_chunk* chunks;            // every chunk, for drop
    neon_arena_big* bigs;                // every big allocation, for drop
    size_t footprint;                    // bytes held from the OS (chunks + bigs)
};

static size_t neon_arena_class_of(size_t total) {
    return (total + NEON_ARENA_GRAIN - 1) / NEON_ARENA_GRAIN - 1;
}

static size_t neon_arena_class_size(size_t cls) {
    return (cls + 1) * NEON_ARENA_GRAIN;
}

neon_arena* neon_arena_create(void) {
    neon_arena* a = calloc(1, sizeof(neon_arena));
    if (a == NULL) {
        neon_trap("out of memory");
    }
    // free_list all NULL, bump == bump_end == NULL (forces a chunk on first alloc), chunks
    // and bigs NULL, footprint 0 — all from calloc.
    return a;
}

// Add a fresh chunk and point the bump region at it. Called when the current chunk cannot
// satisfy a request.
static void neon_arena_grow(neon_arena* a) {
    neon_arena_chunk* chunk = malloc(NEON_ARENA_CHUNK);
    if (chunk == NULL) {
        neon_trap("out of memory");
    }
    chunk->next = a->chunks;
    a->chunks = chunk;
    a->footprint += NEON_ARENA_CHUNK;
    a->bump = (char*)chunk + NEON_ARENA_CHUNK_HDR;
    a->bump_end = (char*)chunk + NEON_ARENA_CHUNK;
}

void* neon_arena_alloc(neon_arena* a, size_t bytes, void (*drop)(void*)) {
    size_t total = sizeof(neon_header) + bytes;
    neon_header* h;

    if (total > NEON_ARENA_MAX) {
        // Big objects are the shared heap's job in the full runtime; a standalone arena
        // still handles one so it is self-contained. Tracked on the big list for drop.
        size_t whole = sizeof(neon_arena_big) + total;
        neon_arena_big* node = malloc(whole);
        if (node == NULL) {
            neon_trap("out of memory");
        }
        node->next = a->bigs;
        node->total = whole;
        a->bigs = node;
        a->footprint += whole;
        h = (neon_header*)(node + 1);
        h->flags = NEON_ALLOC_ARENA | NEON_ALLOC_BIG;
    } else {
        size_t cls = neon_arena_class_of(total);
        size_t bsz = neon_arena_class_size(cls);
        void* blk = a->free_list[cls];
        if (blk != NULL) {
            a->free_list[cls] = *(void**)blk; // pop the reclaimed slot
        } else {
            // Compare remaining bytes rather than forming `bump + bsz`: on the first
            // allocation `bump` is NULL, and even within a chunk `bump + bsz` can land more
            // than one past the end — both undefined pointer arithmetic. `bump_end - bump`
            // is a defined subtraction (both point into the one live chunk, or the guard
            // short-circuits before touching a NULL `bump`).
            if (a->bump == NULL || (size_t)(a->bump_end - a->bump) < bsz) {
                neon_arena_grow(a);
            }
            blk = a->bump;
            a->bump += bsz;
        }
        h = (neon_header*)blk;
        h->flags = NEON_ALLOC_ARENA | (uint32_t)((cls + 1) << NEON_ALLOC_CLASS_SHIFT);
    }

    h->rc = 1;
    h->drop = drop;
    return h;
}

void neon_arena_free(neon_arena* a, void* p) {
    neon_header* h = (neon_header*)p;
    if (h->flags & NEON_ALLOC_BIG) {
        // Unlink from the big list and free. Rare, and big lists are short, so the linear
        // scan is fine; the full runtime routes big values through the shared heap instead.
        neon_arena_big* node = (neon_arena_big*)h - 1;
        neon_arena_big** link = &a->bigs;
        while (*link != NULL && *link != node) {
            link = &(*link)->next;
        }
        if (*link == node) {
            *link = node->next;
            a->footprint -= node->total;
            free(node);
        }
        return;
    }
    size_t cls = ((h->flags & NEON_ALLOC_CLASS_MASK) >> NEON_ALLOC_CLASS_SHIFT) - 1;
    *(void**)p = a->free_list[cls];
    a->free_list[cls] = p;
}

void neon_arena_drop(neon_arena* a) {
    neon_arena_chunk* c = a->chunks;
    while (c != NULL) {
        neon_arena_chunk* next = c->next;
        free(c);
        c = next;
    }
    neon_arena_big* b = a->bigs;
    while (b != NULL) {
        neon_arena_big* next = b->next;
        free(b);
        b = next;
    }
    free(a);
}

size_t neon_arena_footprint(const neon_arena* a) {
    return a->footprint;
}
