#ifndef NEON_CHANNEL_H
#define NEON_CHANNEL_H

// Channels and Task/await — fibers talking to fibers (docs/design/fibers.md), built on the
// scheduler's park/wake (neon/fiber.h).
//
// Two layers live here. The `neon_chan` pair below is a `void*` mechanism the C tests drive.
// The LANGUAGE channel (`neon_channel` + `neon_channel_ref`) carries VALUES: witness-sized
// slots, deep-copied on send — into a parked receiver's arena directly, or staged on the
// slab and restaged by `recv`. A handle is two objects, a shared body and a per-holder
// arena-resident ref; see the long comment in src/fiber_chan.c for why.

#include <stdbool.h>
#include <stddef.h>

#include "neon/core.h" // neon_witness, for the language channel

// ---- channels ----

typedef struct neon_chan neon_chan;

// An unbounded, buffered channel. Sends never block; a receive blocks until a value arrives
// or the channel is closed and drained.
neon_chan* neon_chan_new(void);

// Send `v`. If a receiver is parked it is handed `v` directly and woken; otherwise `v` is
// buffered. Never blocks (the buffer grows). Sending on a closed channel traps — closing is
// the sender's statement that it is done.
void neon_chan_send(neon_chan* ch, void* v);

// Receive into `*out`. Returns true with a value, or — only once the channel is both closed
// and empty — false. Blocks (parks the current fiber) while the channel is open and empty.
bool neon_chan_recv(neon_chan* ch, void** out);

// Close the channel: wake every parked receiver (each gets a false return), and make later
// receives drain the buffer and then return false. Idempotent.
void neon_chan_close(neon_chan* ch);

// Free a channel. It must have no parked receivers (close and let them drain first).
void neon_chan_free(neon_chan* ch);

// ---- the language channel (`std::channel`'s Channel[T], src/fiber_chan.c) ----
//
// Carries VALUES in slots of the element witness's size, deep-copied to the shared heap on
// send so they owe nothing to the sender's arena. The struct is shared-heap allocated and
// refcounted (its first field is a neon_header); every op consumes the handle reference it
// is passed, per the runtime-wide native convention.

typedef struct neon_channel neon_channel;      // the shared body: ring, waiters, lock
typedef struct neon_channel_ref neon_channel_ref; // the per-holder handle (see fiber_chan.c)
typedef struct neon_list neon_list;               // select_recv's List[Channel[T]] argument

// A fresh open channel for elements described by `w`. The caller owns the returned handle.
neon_channel_ref* neon_channel_new(const neon_witness* w);

// A bounded channel: sends park once `n` values are buffered (backpressure). n >= 1.
neon_channel_ref* neon_channel_new_bounded(const neon_witness* w, int64_t n);

// Whether the channel has been closed. Consumes the handle reference.
bool neon_channel_is_closed(neon_channel_ref* ch);

// recv with a deadline: false on closed-and-drained OR timeout (probe is_closed to tell).
// Consumes the handle reference.
bool neon_channel_recv_timeout(neon_channel_ref* ch, void* out, int64_t millis);

// Deep-copy the value at `v` into the channel (straight into a parked receiver's slot when
// one waits), release the original, wake the receiver. False when the channel is (or
// becomes, for a parked bounded send) closed — the copy never ran on that path, so a
// resource's baton stayed with the caller; std::channel turns it into ClosedError. Always
// consumes the `ch` reference and the value.
bool neon_channel_send(neon_channel_ref* ch, const void* v);

// Receive into `out`: true with a value (owned by the caller), false only once the channel
// is closed AND drained. Parks the calling fiber while open and empty. Consumes the `ch`
// reference (after any park).
bool neon_channel_recv(neon_channel_ref* ch, void* out);

// Close: wake every parked receiver empty-handed; later receives drain then return false.
// Idempotent. Consumes the `ch` reference.
void neon_channel_close(neon_channel_ref* ch);

// select_recv over a `List[Channel[T]]`: block until one channel yields a value or is
// closed-and-drained, take from exactly that one, leave the rest untouched. On return
// `*out_index` is the winning list index; true means a value landed in `out` (owned by the
// caller), false means that channel was closed (the null outcome). A ready value beats a
// closed channel, ties go to the lowest index. Parks the calling fiber while every channel is
// open and empty. Consumes the `list` reference (and, through it, every channel ref it
// holds). Traps on an empty list. See the long comment in src/fiber_chan.c for the
// claim/lock-order machinery.
bool neon_channel_select_recv(neon_list* list, void* out, int64_t* out_index);

// The witness `copy` for a channel-handle slot: retain + pointer copy (a channel is a
// shared-heap identity). Used by emitted witness tables when a Channel crosses fibers.
void neon_wcopy_channel(const void* src, void* dst);

// ---- the language task (`std::task`'s Task[T], src/fiber_chan.c) ----
//
// A fiber whose result crosses back: the body's value is deep-copied to the shared heap at
// completion (the channel discipline), and `await` moves it out to the awaiter — one-shot.
// The handle is shared-heap refcounted with the result slot inline.

typedef struct neon_task_lang neon_task_lang; // the shared body: result slot, awaiter
typedef struct neon_task_ref neon_task_ref;   // the per-holder handle

// The per-repr call shim, emitted by codegen: call the zero-arg body closure and write its
// result (whose C type only codegen knows) to `out`.
typedef void (*neon_task_shim)(neon_closure f, void* out);

// Spawn `body` as a task fiber; the caller owns the returned handle. The body must be a
// named function or capture-free lambda (the spawn rule).
neon_task_ref* neon_task_lang_spawn(neon_closure body, const neon_witness* w,
                                    neon_task_shim shim);

// Block until the task completes and move its result into `out` (owned by the caller).
// One-shot: a second await traps, and awaiting a task whose body CRASHED traps too — the
// failure propagates along await edges. Consumes the handle reference.
void neon_task_lang_await(neon_task_ref* t, void* out);

// The witness `copy` for a task-handle slot: retain + pointer copy (a shared identity,
// like a channel). Used by emitted witness tables when a Task crosses fibers.
void neon_wcopy_task(const void* src, void* dst);

// ---- Task[T] / await (the C-level `void*` form the runtime tests drive) ----

typedef struct neon_task neon_task;

// A task body: run for its result, which await hands back. The result must outlive the task's
// arena (a scalar, or a shared-heap value) — see the file header.
typedef void* (*neon_task_fn)(void* arg);

// Spawn `fn(arg)` as a fiber under the current scheduler and return a handle to await its
// result. Must be called from within a fiber (there must be a scheduler).
neon_task* neon_task_spawn(neon_task_fn fn, void* arg);

// Block until the task has finished and return its result. If it has already finished, returns
// at once. One awaiter per task.
void* neon_task_await(neon_task* t);

// Free a finished task's handle. Awaiting first is the caller's responsibility.
void neon_task_free(neon_task* t);

#endif
