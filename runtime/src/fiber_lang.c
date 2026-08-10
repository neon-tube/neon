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

// `fiber::runtime(body)` — the lazy entry. Called on the root context (nesting traps in the
// scheduler), so `body`'s environment is slab-allocated and safe to hand to the first fiber.
void neon_fiber_lang_runtime(neon_closure body) {
    neon_fiber_runtime(neon_fiber_run_closure, neon_fiber_cell_new(body));
}

// `fiber::spawn(body)` — enqueue a fiber under the running scheduler.
void neon_fiber_lang_spawn(neon_closure body) {
    if (body.env != NULL && (body.env->flags & NEON_ALLOC_ARENA)) {
        // See THE ARENA RULE above: this closure captures values that live in the spawning
        // fiber's arena, and nothing yet copies them out. Refuse loudly.
        neon_trap(
            "fiber::spawn: the body captures values from the spawning fiber, which cannot "
            "cross fibers yet — pass a named function or a capture-free lambda");
    }
    neon_fiber_spawn(neon_fiber_run_closure, neon_fiber_cell_new(body));
}

// ---- fiber::spawn_with(body, arg) ----
//
// The way DATA reaches a spawned fiber today: a capture-free body plus one explicit
// argument, deep-copied to the shared heap at the spawn site (the witness's `copy`, under
// shared routing) so it owes nothing to the spawner's arena. The `shim` is emitted by
// codegen per argument repr — only it knows the C type the closure call needs — with the
// shape `shim(closure, arg_bytes)`: load the value, call the closure with it by value.
//
// Ownership: the closure and the original argument are both consumed here (the original is
// released after the copy, in the spawner's context); the copied argument is consumed by
// the body call itself, exactly as any argument is.

typedef void (*neon_spawn_shim)(neon_closure f, const void* arg);

typedef struct {
    neon_closure body;
    neon_spawn_shim shim;
    const neon_witness* w;
    // the copied argument's bytes follow, w->size of them
} neon_spawn_cell;

static void neon_fiber_run_spawn_with(void* arg) {
    neon_spawn_cell* cell = (neon_spawn_cell*)arg;
    neon_closure body = cell->body;
    neon_spawn_shim shim = cell->shim;
    // The argument bytes ride in the cell; hand them to the shim, which passes the value to
    // the body BY VALUE — the call consumes it, so the cell is free to go.
    shim(body, (const void*)(cell + 1));
    free(cell);
    neon_release(body.env);
}

void neon_fiber_lang_spawn_with(neon_closure body, const void* arg, const neon_witness* w,
                                neon_spawn_shim shim) {
    if (body.env != NULL && (body.env->flags & NEON_ALLOC_ARENA)) {
        neon_trap(
            "fiber::spawn_with: the body captures values from the spawning fiber, which "
            "cannot cross fibers yet — pass a named function or a capture-free lambda, and "
            "hand data over as the argument");
    }
    neon_spawn_cell* cell = malloc(sizeof(neon_spawn_cell) + w->size);
    if (cell == NULL) {
        neon_trap("out of memory");
    }
    cell->body = body;
    cell->shim = shim;
    cell->w = w;
    void* saved = neon_send_routing_begin();
    if (w->copy) {
        w->copy(arg, (void*)(cell + 1));
    } else {
        memcpy((void*)(cell + 1), arg, w->size);
    }
    neon_send_routing_end(saved);
    // Consume the original argument in the spawner's own context.
    if (w->release) {
        w->release((void*)arg);
    }
    neon_fiber_spawn(neon_fiber_run_spawn_with, cell);
}
