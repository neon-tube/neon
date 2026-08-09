#ifndef NEON_CHANNEL_H
#define NEON_CHANNEL_H

// Channels and Task/await — fibers talking to fibers (docs/design/fibers.md, slice 5), built
// on the scheduler's park/wake (neon/fiber.h). This is the C mechanism: payloads are `void*`.
// HOW a language value crosses a channel — small values copied into the receiver's arena,
// big values shared on a refcounted heap, Resources moved — is codegen's job (the send site
// emits the copy), because only codegen knows the value's type and size. In particular a
// value that points into the SENDER's arena must be copied out before it is sent, and a
// Task's result must not point into the task's arena, which is dropped when the task is
// reaped. The tests here use scalars cast to `void*`, which live in no arena.

#include <stdbool.h>
#include <stddef.h>

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

// ---- Task[T] / await ----

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
