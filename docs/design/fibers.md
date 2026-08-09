# Design: fibers — stackful, uncoloured, and refcount-safe by construction

**Status:** design, nothing implemented. No `spawn`/`Channel`/scheduler exists in
`compiler/src/` or `runtime/src/` today. This is the document to argue with before a line
is written. This document is **fibers, and only fibers.** Two further designs — a
general-purpose **executor pool** (CPU-parallel `par_map`) and **send-multiprocess**
(OS-process parallelism) — are pinned and out of scope; they are named at the end only to
mark the seams they will attach to. The one piece of thread machinery that IS in scope
here is a small internal **file-offload pool**, because `std::fs` under fibers depends on
it on the readiness IO backends (epoll/kqueue/select) — see the IO-engine section.

This file is the reasoning. Where it and a future module doc disagree, the module doc is
nearer the code and this file is the bug.

## The one constraint everything answers to

Every heap object carries a non-atomic `uint32_t rc` (`runtime/include/neon/core.h`),
incremented with a plain `h->rc++`. This is not an accident to be fixed later — it is a
deliberate, measured choice that the slab allocator and the 16-byte header were tuned
around. The moment two OS threads can reach the same object's `rc` concurrently, that
`h->rc++` is a data race and a use-after-free.

So the whole concurrency design is organised around one rule:

> **No heap object's refcount is ever touched by two OS threads at once.**

Everything below is a different structural way of honouring that rule. Fibers honour it by
*serialising*: many fibers, one OS thread per scheduler, one running at a time. The other
two designs honour it by *isolation* (pools: private heaps; processes: private address
spaces). None of them make the refcount atomic, because that is a tax on every program to
buy something most programs do not use.

## What a fiber is

A fiber is a **stackful, cooperatively-scheduled green thread**: its own stack, its own
call chain, suspended and resumed by swapping the stack pointer. Millions are cheap. They
are Erlang's processes and Go's goroutines and Java's virtual threads — the same idea from
the same premise (immutability, per-context heaps, no shared mutable state), which is why
Neon fits the model rather than fighting it.

The defining property, and the reason to build it stackful rather than as a state machine:

> **A fiber suspends by swapping its whole stack, so any ordinary `fn` can block-and-yield
> transparently, and NO function signature ever changes.** There is no `async` keyword, no
> function colouring, no `await`. `fs::read(p) -> str` is the same function whether it runs
> on a fiber or on `main`.

Colouring exists only because *stackless* `async` compiles a suspension into a compile-time
state machine, so "can suspend" has to appear in the type and propagates up every caller.
A stackful fiber keeps a real stack; its locals live on it; the switch preserves them; so
suspension needs nothing in the type. Choosing the stack-swap mechanism *is* choosing to
have no colours.

## The mechanism: the swap gadget

Context switching is a small per-architecture assembly routine — save the callee-saved
registers, exchange `rsp`, restore. On x86-64 SysV:

```asm
; void neon_fiber_swap(void** save_rsp, void* resume_rsp)
neon_fiber_swap:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov  [rdi], rsp        ; park the caller onto its own stack
    mov  rsp, rsi          ; adopt the resumed fiber's stack
    pop  r15
    pop  r14
    pop  r13
    pop  r12
    pop  rbx
    pop  rbp
    ret                    ; returns *into* the resumed context
```

That is the entire switch — a few nanoseconds. It is well-trodden (boost.context, libco,
Go's `gogo`, every fiber library), and it is the easy part. Three things ride on it that
are **not** easy, and are the actual project:

1. a completion-based IO engine (IOCP/io_uring/epoll/kqueue behind one interface), or
   blocking freezes the world;
2. a stack-lifetime and stack-size story;
3. preemption, or a CPU-bound fiber starves the rest.

Each has a section below. First, the two properties that make the whole thing sound.

### Portability of the gadget

The gadget is per-architecture and per-ABI: x86-64 SysV, x86-64 Windows (which also
requires updating the TIB's stack base/limit fields, or the OS faults on the swapped
stack), and aarch64 (Apple silicon, and the arm64 Linux/Windows targets). That is 3–4
hand-written stubs, each a foot-gun. **Vendor `boost.context`'s `fcontext` (or an
equivalently small, audited library) rather than hand-roll all of them** — the swap is not
where Neon should be spending its correctness budget, and the corner cases (Windows TIB,
signal stacks) are exactly the ones a library already got right.

### Sanitizer discipline is mandatory, not optional

Neon's correctness rests on ASan/UBSan/LSan over the whole corpus (`backend_run.rs`). A
raw `rsp` swap makes ASan believe the stack was smashed and floods every concurrent test
with false positives. The swap gadget **must** bracket the switch with
`__sanitizer_start_switch_fiber` / `__sanitizer_finish_switch_fiber`. This is a known,
required pattern, not a nicety; it is written once, in the gadget, under the sanitizer
build.

## Why non-atomic refcounts survive: one OS thread per scheduler

A **scheduler** owns one OS thread and one heap, and runs its fibers **one at a time**. A
fiber runs until it *voluntarily* yields (at a channel operation, an IO park, or a
compiler-inserted safepoint); then the scheduler swaps to the next runnable fiber. Because
only one fiber on a scheduler runs at any instant, no two of them ever execute `h->rc++`
concurrently. The non-atomic refcount is correct with **zero changes**, and the slab and
the 16-byte header are untouched.

This is M:1 within a scheduler. Parallelism across cores is a *later* design (executor
pools and multiprocess), and it reaches parallelism specifically WITHOUT putting two
threads on one heap — see the last section. A fiber never migrates OS threads, which also
kills the staleness class of bug described under "current fiber" below.

## Finding the current fiber: from `rsp`, not from a thread-local

A blocking native — `fs::read`, `process::wait`, `io::read_line` — has to know whether it
is running on a fiber (park on would-block) or not (block normally). The naive answer is a
thread-local `current_fiber`, and it is both slower than it needs to be (if done with
`pthread_getspecific`) and carries a staleness hazard (a cached value across a migration).

The better answer reuses the slab's own trick: **derive the current fiber from the stack
pointer.** Give each fiber a size-aligned stack and place its control block at the base.

```c
static inline neon_fiber* neon_current_fiber(void) {
    uintptr_t sp = (uintptr_t)__builtin_frame_address(0);
    return *(neon_fiber**)(sp & ~(NEON_FIBER_STACK_SIZE - 1));
}
```

No thread-local. The current fiber is a *pure function of `rsp`*, and `rsp` is current by
definition — the swap already changed which stack we are on, so there is nothing to update
on a switch and nothing to get stale. The scheduler's own OS stack is not size-aligned this
way, so the mask lands on a sentinel: "not on a fiber, block normally," the null case for
free and with no branch to maintain.

The string attached: this needs **fixed-size fiber stacks** (the mask needs a constant
size). That is almost certainly the right first answer anyway — see stacks, below — but it
is a real commitment, and it is the reason the stack decision and the current-fiber
decision are one decision.

## Transparent blocking: the native decides, the signature does not

`std::fs`, `std::io` and `std::process` do not change. Their *natives* gain a fiber-aware
path, chosen dynamically:

```c
neon_str neon_io_read_all(int64_t fd) {
    if (neon_current_fiber() == NULL) {
        return blocking_read(fd);          // main-before-any-spawn, or a pool worker: just block
    }
    // On a fiber: hand the read to the IO engine and park. `submit` returns only when the
    // op has completed — resumed by the scheduler draining the completion queue. The engine
    // is completion-native on IOCP/io_uring and adapts readiness (arm epoll, then read) or
    // pool-offloads a regular file underneath, invisibly.
    neon_io_result r = neon_io_submit(NEON_OP_READ, fd, buf, cap);
    if (r.err < 0) {
        return -r.err;                     // through the existing IoError channel, unchanged
    }
    return ...;                            // r.n bytes are already in buf
}
```

`main` runs *on a fiber* from the start (like Go's goroutine 1), so in practice the `NULL`
branch is only ever taken by a file-offload pool worker — the code path stays uniform, and
the *signature* of `fs::read` is unchanged in every context.

## The IO engine is completion-based (a proactor), because IOCP demands it

The single most consequential architectural choice, forced by the platform targets:
**IOCP (Windows) and io_uring (Linux ≥5.1) are COMPLETION models** — you submit "read this
into this buffer" and the OS hands back the *finished* operation — while
**epoll/kqueue/select are READINESS models** — the OS tells you a read *won't block* and
you do it yourself. These are not two flavours of one interface; they are opposite
control flows.

A runtime that wants both cannot pick readiness as its interface: emulating completion on
readiness is easy (arm interest, wake, do the syscall), but emulating readiness on IOCP is
a disaster (zero-byte reads, or falling back to `select` on Windows and discarding
everything IOCP is for). So the interface is **completion**, and readiness backends are
adapted up to it. This is exactly the choice libuv (Node) and Boost.Asio made, and for the
same reason.

The fiber-facing operation is therefore always "submit and park, resume with the result":

```c
// One completion op: fiber submits, parks, and resumes here with `res` filled in.
neon_io_result neon_io_submit(neon_io_op op, int64_t fd, void* buf, size_t len, ...);
```

Under the hood, one small vtable, four backends, chosen at build/runtime:

| backend            | model      | sockets/pipes | regular files       | when              |
|--------------------|------------|---------------|---------------------|-------------------|
| **IOCP**           | completion | native        | **native** (OVERLAPPED) | Windows       |
| **io_uring**       | completion | native        | **native** (async disk) | Linux ≥5.1    |
| **epoll**          | readiness  | adapted       | *pool offload*      | Linux fallback    |
| **kqueue**         | readiness  | adapted       | *pool offload*      | BSD/macOS (later) |
| **select**         | readiness  | adapted       | *pool offload*      | universal floor   |

Two payoffs of picking completion as the interface fall straight out of that table:

- **On the completion backends, regular-file IO is async natively.** IOCP with an
  OVERLAPPED handle and io_uring both do disk reads asynchronously — the very thing epoll
  cannot. So on Windows and modern Linux there is **no thread pool in the file path at
  all**; `fs::read` submits to the engine and parks like any socket.
- **The thread pool becomes a compatibility shim, not a core component.** It exists only to
  give the *readiness* backends (epoll, kqueue, select) an answer for unpollable regular
  files — `epoll` reports a file "always ready" while the `read` still blocks on the disk,
  so on those backends a file op is handed to a **dedicated blocking OS thread** (Go's
  `entersyscall`) and the fiber parks until it finishes. On the completion backends the
  pool is simply unused for files.

The pool worker runs with `neon_current_fiber() == NULL` (its stack is not a fiber stack),
so the same native takes its plain-blocking branch there and the mechanism composes. This
**internal file-offload pool is part of the fiber runtime** — small, fixed, and required
for `std::fs` on readiness backends. It is NOT the general-purpose CPU executor pool
(`par_map` and friends): that, and OS-process parallelism, are deliberately **pinned** and
out of scope here (see the closing section). Fibers are the whole of this document.

## Cancellation is part of the completion contract

A completion model has to answer "the parked fiber no longer wants this op" — its channel
closed, a timeout fired, the scheduler is shutting down. Readiness backends get this for
free (just do not perform the syscall), but completion backends have an op in flight in the
kernel: IOCP has `CancelIoEx`, io_uring has `IORING_OP_ASYNC_CANCEL`, and the buffer must
stay pinned until the cancellation itself completes (the kernel may still write to it up to
that point). So a submitted op owns its buffer until the engine reports the op *or its
cancellation* done — the parked fiber, whose live stack holds the buffer, is exactly that
owner, so the lifetime is correct by construction, but the cancellation handshake is real
surface the engine vtable must carry.

## Stacks: fixed size, guard page, and the honest limit

Neon recurses — binary-trees' `make`/`check` is depth-18 recursion, and a user program can
go deeper. So fiber stacks cannot be assumed tiny.

The proposal: **fixed-size stacks (e.g. 256 KiB) from `mmap`, size-aligned (for the
`rsp` trick), each with a guard page below it.** A stack overflow hits the guard page and
traps with a clear message rather than silently corrupting a neighbour — the same "a fault
is a trap, not undefined behaviour" stance the rest of the runtime takes. Stacks are pooled
and reused (an allocation of a fiber is a pop from a free list of stacks, a slab by another
name), never unmapped until exit.

Rejected, and why:

- **Growable/segmented stacks** (Go pre-1.4) break the size-aligned `rsp` trick, have the
  "hot split" pathology, and are a large amount of runtime machinery. Not for v1.
- **Tiny stacks that grow by copying** (Go 1.4+) require the compiler to emit stack-limit
  checks at every function entry and to relocate pointers into the stack on a move — a deep
  compiler commitment. A candidate for later if 256 KiB × millions proves too heavy, but
  not the place to start.

The honest limit of fixed stacks: a million fibers at 256 KiB reserved is 256 GiB of
*address space* (fine — it is untouched, so it costs no physical memory until used), but a
million fibers each using 200 KiB of stack is real memory. This is the same limit Loom
accepts; it is a documented ceiling, not a bug.

## Preemption: the compiler is the asset

Cooperative scheduling — a fiber yields only at channel ops and IO parks — is simple and
correct, but a CPU-bound fiber that does neither starves every other fiber on its
scheduler. Two ways out:

- **Signal-based** (Go 1.14+): the scheduler sends a signal, the handler forces a yield.
  Portable-ish, subtle, and hostile to the sanitizer discipline above.
- **Compiler-inserted safepoints**: the backend already inserts refcounts and bounds checks
  at known sites. Add a one-instruction poll — `if (self->preempt) neon_fiber_yield();` —
  at **loop back-edges and function entry**. A CPU-bound fiber hits a back-edge on every
  iteration and yields within a bounded number of instructions of being asked to. This is
  deterministic, needs no signals, and is something an interpreted runtime cannot do as
  cleanly. It is Neon's structural advantage, and it is the version to build.

The cost is a poll per back-edge. It is a predicted, almost-always-not-taken branch; on the
brainfuck-style tight loops it is measurable and would want to be as cheap as the bounds
check the same loops already carry. Whether it is on by default or opt-in per build is a
decision for when it is built, informed by measuring it.

## The surface: `spawn` and `Channel[T]`

Fibers communicate the way the memory model demands: by **sending values through
channels**, never by sharing mutable state (there is no mutable state to share). The
surface is small.

```neon
// std::fiber
fn spawn(body: () -> null) -> Fiber          // start a fiber; it runs the closure
fn yield()                                   // cooperatively step aside

opaque record Channel[T] { .. }
fn channel[T](capacity: i64) -> Channel[T]   // 0 = rendezvous (send blocks for a recv)
fn send[T](c: Channel[T], v: T)              // parks the fiber if the buffer is full
fn recv[T](c: Channel[T]) -> T | closed      // parks the fiber if the buffer is empty
fn close[T](c: Channel[T])                   // recv drains, then returns `closed`
```

Two questions this surface leaves open, both flagged for the argument:

- **`select` over multiple channels.** Needed for any non-trivial fiber (wait on either of
  two inputs, or on an input-or-timeout). It is real design — the fairness rule, the
  registration/deregistration on the wait queues — and might be v2. But a runtime without
  it forces every multi-source fiber into a busy-poll, so it is not comfortably deferrable.
- **What a `body` may capture.** A spawned closure captures values from its parent's heap.
  On one scheduler with one heap, that is safe (still one thread), but it is the seam where
  a future multi-scheduler design would need the capture MOVED, not shared — see below.

### The send that costs nothing

A channel send hands a value from one fiber to another. On a single-scheduler heap, that is
a pointer copy and a retain — both are on the same heap, both correct. But the value being
sent is very often *uniquely owned* at the send site (the sender is done with it), and
`ir::unique` — the analysis built for in-place mutation — is exactly the analysis that
proves `rc == 1`. When it does, the send **moves** the reference instead of retaining it:
zero copy, zero refcount traffic, the sender's obligation transferred across the channel.

The uniqueness pass we already own is the enabling analysis for cheap message passing. That
is the sign the model fits the language: the machinery is already here, built for another
reason, and it turns out to be what this needs.

## What this design deliberately is NOT

- **Not shared-memory threading.** Two fibers never touch one object concurrently. That is
  the whole point.
- **Not parallel by itself.** One scheduler is one core. Fibers are *concurrency* —
  millions of cheap suspendable tasks, transparent async IO — not *parallelism*.
  Parallelism is the next two designs, which reach it by isolation, not by sharing.
- **Not colored.** No `async`/`await`, no signature ever changes, by construction of the
  stackful mechanism.

## The decisions to settle before building

1. **Fixed-size stacks?** Yes is assumed throughout (it is what the `rsp` trick needs), but
   it is the load-bearing commitment and it should be made on purpose. Size, and the
   overflow-traps-on-a-guard-page behaviour.
2. **`select` in v1 or v2?** It is uncomfortable to defer and real to build.
3. **Preemption on by default?** Depends on the measured cost of the back-edge poll.
4. **Does `main` run on a fiber from the start?** Recommended yes (kills the null branch,
   makes the runtime uniform), but it means the runtime stands up a scheduler before
   `nl_main`, which every non-fiber program then also pays for at startup. Measure it.

## Pinned: the two parallelism designs, and the seams they attach to

Fibers give *concurrency* while preserving the memory model exactly. *Parallelism* — using
more than one core — is a separate concern, deliberately **pinned** here. Both future
routes reach it WITHOUT two threads on one heap, and both are named now only so this design
leaves the right seams:

- **Executor pool (CPU-parallel).** A bounded set of OS threads running *leaf* Neon work —
  `list::par_map` and kin. The refcount rule is kept by isolation: a pool task gets its own
  arena, its inputs are *moved* in (`ir::unique` proves `rc == 1`) or read-only-and-pinned
  so it touches no shared `rc`, and its result is moved out. **Seam:** the same move-on-
  unique boundary a channel send already uses. Distinct from this document's internal
  file-offload pool, which runs no Neon code and touches no heap.
- **Send-multiprocess.** Real OS processes running Neon code, isolated address spaces, no
  shared heap at all, over the `std::process` pipe machinery already built. Full
  parallelism, full isolation, at the cost that every send marshals. **Seam:** whatever a
  channel `send` transfers must, for a cross-process channel, be *serializable* — which
  ties this to `docs/design/serialization.md` and makes "sendable across a process" and
  "sendable across a machine" one concept. Whether a Neon process is a `fork` (fast, COW,
  POSIX-only, thread-hostile) or spawn-and-serialize (portable, marshal-on-send) is the
  ruling that gates it, and it is not made here.

Neither is built or committed. This document stands on its own: fibers, an IO engine, and
the file-offload pool they require.
