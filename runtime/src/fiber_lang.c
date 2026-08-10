// The `std::fiber` native surface — the seam between a Neon closure and the C scheduler
// (src/fiber_sched.c). A Neon `() -> ()` value arrives as a 16-byte `neon_closure` by value,
// owned (the convention every native shares; `neon_resource_new` is the precedent): the
// trampoline calls it on the new fiber's stack and releases its environment after.
//
// The closure crosses a stack boundary, so it rides in a malloc'd cell — the spawner's frame
// may be long gone when the spawned fiber first runs. Plain malloc/free, not the neon heap:
// the cell is runtime plumbing, not a language value, and it must not land in any fiber's
// arena (the spawned fiber frees it from its own context).
//
// THE ARENA RULE (why spawn checks the env): a capturing closure's environment is allocated
// wherever the closure was BUILT. Built on the root context (the `fiber::runtime` call site)
// that is the process slab, which any fiber may safely release into — the slab is routed by
// the header flag, not by who is running. Built INSIDE a fiber it is that fiber's arena, and
// the spawned child releasing it would free into the child's arena — the cross-fiber
// sendability problem, whose answer (copy-on-send) is the channels slice. Until then spawn
// refuses the case loudly rather than corrupting quietly. Named functions and capture-free
// lambdas have a NULL environment and are always safe — `fiber::spawn(worker)` is the
// canonical form.

#include "libneon_rt.h"

#include <stdlib.h>
#include <string.h> // memcpy for the scalar spawn_with argument

// The cell a closure rides in from the spawn site to the fiber's first run.
typedef struct {
    neon_closure body;
} neon_fiber_cell;

// Every language-spawned fiber runs this: unwrap the cell, call the closure (unit-returning,
// zero-arg: `void (*)(neon_header*)` per the closure ABI), release the owned environment.
static void neon_fiber_run_closure(void* arg) {
    neon_fiber_cell* cell = (neon_fiber_cell*)arg;
    neon_closure body = cell->body;
    free(cell);
    ((void (*)(neon_header*))body.fn)(body.env);
    neon_release(body.env); // consume the closure; NULL is a no-op
}

static neon_fiber_cell* neon_fiber_cell_new(neon_closure body) {
    neon_fiber_cell* cell = malloc(sizeof(neon_fiber_cell));
    if (cell == NULL) {
        neon_trap("out of memory");
    }
    cell->body = body;
    return cell;
}

// The env-copy table's weak defaults: every fiber PROGRAM emits the strong definition
// (codegen's emit_env_copies); these exist so the C test binary — runtime linked without a
// generated program — still links. Weak is fine here: the fiber sources are ELF-only
// (x86-64 SysV gate), where weak symbols are a given.
__attribute__((weak)) const neon_env_copy_entry* neon_env_copy_entries = 0;
__attribute__((weak)) size_t neon_env_copy_count = 0;

neon_header* neon_env_copy_to_shared(neon_header* env) {
    for (size_t i = 0; i < neon_env_copy_count; i++) {
        if (neon_env_copy_entries[i].drop == env->drop) {
            void* saved = neon_send_routing_begin();
            neon_header* c = neon_env_copy_entries[i].copy(env);
            neon_send_routing_end(saved);
            neon_release(env); // the reference the spawn consumed, in the caller's context
            return c;
        }
    }
    neon_trap("fiber spawn: closure environment has no registered copy (compiler bug)");
}

// A capturing closure crossing to a new fiber: an arena-resident environment is deep-copied
// to the shared heap (captures and all, the sendability rules applying to each — a captured
// resource or closure traps with the unsendable message); a slab or NULL environment is
// already safe and passes through.
static neon_closure neon_fiber_body_cross(neon_closure body) {
    if (body.env != NULL && (body.env->flags & NEON_ALLOC_ARENA)) {
        body.env = neon_env_copy_to_shared(body.env);
    }
    return body;
}

// `fiber::runtime(body)` — the lazy entry. Called on the root context (nesting traps in the
// scheduler), so `body`'s environment is slab-allocated and safe to hand to the first fiber.
void neon_fiber_lang_runtime(neon_closure body) {
    neon_fiber_runtime(neon_fiber_run_closure, neon_fiber_cell_new(body));
}

// `fiber::runtime_threads(n, body)` — the M:N entry: n scheduler seats, fibers pinned by
// spawn's round-robin. n <= 1 is exactly fiber::runtime.
void neon_fiber_lang_runtime_threads(int64_t threads, neon_closure body) {
    neon_fiber_runtime_threads(threads, neon_fiber_run_closure, neon_fiber_cell_new(body));
}

// `fiber::spawn(body)` — enqueue a fiber under the running scheduler.
void neon_fiber_lang_spawn(neon_closure body) {
    neon_fiber_spawn(neon_fiber_run_closure, neon_fiber_cell_new(neon_fiber_body_cross(body)));
}
