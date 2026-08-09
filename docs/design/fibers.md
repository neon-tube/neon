# Design: fibers — isolated-arena green threads, refcount-safe by construction

**Status:** design locked, implementation in progress on the `fibers` branch, built in
independently-testable slices (see the build order at the end). No fiber runtime is on
`main` yet. This document is the decided design; where it and a future module doc disagree,
the module doc is nearer the code and this file is the bug.

This is **fibers only.** A general-purpose CPU **executor pool** (`par_map`) and
**send-multiprocess** are pinned and named at the end just as seams. The one thread
component in scope is the internal **file-offload pool**, which `std::fs` under fibers
requires on the readiness IO backends.

## The constraint everything answers to

Every heap object carries a non-atomic `uint32_t rc`. Two OS threads touching one object's
`rc` concurrently is a data race and a use-after-free. So the whole design honours one rule:

> **No heap object's refcount is ever touched by two OS threads at once.**

The chosen way to honour it is **isolation**: each fiber has its own heap (arena), and a
value lives in exactly one arena. Cross-fiber sharing of a reference is structurally
impossible, so the non-atomic `rc` is safe by construction — not by a lock, not by
serialisation, not by an atomic. (The one exception, the size-heuristic shared heap for big
values, is addressed below and pays atomic `rc` only where it is free to.)

## What a fiber is

A **stackful, cooperatively-scheduled green thread**: its own stack, its own arena,
suspended and resumed by swapping the stack pointer. The defining property:

> **A fiber suspends by swapping its whole stack, so any ordinary `fn` can block-and-yield
> transparently, and NO function signature changes.** No `async`, no colouring, no `await`.
> `fs::read(p) -> str` is the same function on a fiber and on `main`.

Colouring exists only because *stackless* `async` compiles suspension into a compile-time
state machine, forcing "can suspend" into the type. A stackful fiber keeps a real stack;
its locals live on it; the swap preserves them; nothing colours. Choosing the stack-swap
mechanism *is* choosing to have no colours. (Java's Loom is the industrial proof:
`InputStream.read()` didn't change signature; the JDK parks the virtual thread.)

## The heap: per-fiber arenas

Each fiber allocates from its own arena. The arena is:

- **Bump-pointer for fresh allocation** — a register-pinned pointer, so allocation is a
  pointer add (faster than the global slab's free-list-pop-plus-class-math). See "the
  register" below.
- **Size-class free lists for reclaim** — a refcount reaching zero pushes the slot to its
  class list; a later allocation pops that list before bumping. This is what makes memory
  **plateau instead of leak**: arena size = the sum over classes of each class's high-water
  mark of simultaneously-live objects, which stops growing once each class peaks and is
  bounded by the fiber's *peak per-class working set*. Pure bump (no reclaim) charges
  *total* allocation and would leak a long-lived fiber; size classes charge *peak live* and
  cannot. No fiber can fragment into an unbounded leak.
- **Dropped whole at death** — normal exit and crash both end by dropping the entire arena
  in one operation, the "generational nursery bulk-frees the dead young objects" win without
  a moving collector.

**This is the GC, and it needs no scan and no compaction.** Immutability ⟹ no cycles ⟹
refcounting is *complete* (everything reaches zero, nothing leaks), so there is no tracing
backstop to build. The arena-drop gives the bulk-free that copying collectors compact-to-
reclaim for. What a compacting collector additionally gives — defragmentation — is unneeded:
short fibers die before fragmenting, uniform-allocation fibers (the accept loop) barely
fragment, and size classes bound the residual to a plateau. So Neon gets complete
reclamation, bulk young-object free, zero leaks, **stable pointers** (no relocation, no
barriers, trivial FFI) and deterministic destruction — the things tracing/copying/
generational collectors are built for — with none of the machinery.

*Smarter later (earned by proof, never default):* a fiber the compiler proves
bounded-lifetime can use **pure bump, no reclaim** (total is known small, so skipping the
free list is safe and faster). Safe-by-default (size-classed, plateaus), optimise-where-
proven (pure bump). That is the Neon posture: the fast-but-unsafe path exists only where the
compiler guarantees it is safe.

## The register

The current fiber's arena — specifically its bump/free-list state — is pinned in a
dedicated register (or an initial-exec `_Thread_local`), updated by the swap gadget on a
fiber switch (free — the gadget already saves and restores callee-saved registers). So
allocation reads the arena with no lookup and no branch. This is Go's `mcache`-via-`g`
register, exactly. The current *fiber* (for park/trap/spawn) is reached the same way; a
`_Thread_local` here is a single segment-relative load, not `pthread_getspecific`.

(An earlier draft derived the current fiber from a size-aligned stack via `rsp`-masking.
That is unsound for non-fiber threads — a file-offload pool worker on an ordinary pthread
stack would read arbitrary bytes as a fiber pointer. `_Thread_local` is the sound choice,
and it also frees us from fixed-size stacks, leaving growable stacks open as a later
density optimisation.)

## Crash semantics: a trap is fatal to the fiber, and leaves no mess

Today a trap is `_exit(101)` — the whole process, cleaned up by the OS. Making a fiber a
fault domain means being your own OS for that domain: close its handles, free its memory.
The design does exactly that, cleanly, because **Neon's traps are preventive.** A bounds
check traps *before* the bad access; overflow *before* the wrong value; division *before*
the fault. So the heap is *consistent* at the trap point — the trap fired to keep it that
way — and an immutable cleanup closure over intact immutable values can run safely.

Teardown — exit *and* crash, the same operation — is a **walk, not a registry.** Resources
and big shared values do NOT live in the arena; they live in the process/shared heap
(a Resource must survive its creating fiber, since it can be *moved* to another fiber). The
arena only holds *references* to them. So:

1. **Walk the arena's live objects, releasing their OUTGOING references.** A Resource
   reference released to its last holder runs the cleanup (close the fd); a shared-value
   reference is decremented (freed if last). Intra-arena references need nothing — those
   objects are all dying together. The walk is generic (repr-directed, like the drop
   functions), so Resources and shared values are handled uniformly, with no per-object
   tracking and no cross-fiber re-homing. Cost is **O(live working set)** — bounded by the
   arena's plateau — paid once, at death, never during execution. Each cleanup is
   **guarded**: one that itself traps is abandoned, logged, and the walk continues (C++'s
   "don't throw from a destructor mid-unwind"). Cleanup runs blocking and prompt — the fiber
   is dead, it cannot park.
2. **Bulk-free the arena's memory** — the arena-local objects, in one operation.
3. **Swap back to the scheduler** — the park operation, flagged dead. The fiber's stack
   returns to the pool.

The walk needs only **arena walkability**: each slot's header already carries its type (the
drop pointer) to find outgoing references and its size to step to the next, plus a
freed-slot flag to skip reclaimed slots. No registry, no per-store cost, no re-homing — the
walk is the teardown. This also unifies exit and crash: during life, refcounting reclaims
intermediate garbage into the arena's free lists; only the *terminal* release at death is
replaced by the walk (which is the bulk-drop win — skip per-object terminal releases).

No memory leaked, no handle left open, and every other fiber untouched. For an HTTP server
this is tokio's `catch_unwind`-per-task behaviour reached with **no unwinding** — the fiber
does not `try` its own bounds error; it dies and the system moves on.

**Non-fiber traps `_exit` unchanged.** `neon_trap` is only reachable from generated Neon
code, which only ever runs on a fiber, so in practice the isolate path is universal for
program code; the scheduler and IO engine are hand-written C that report errors as C return
codes and never call `neon_trap`. So the branch is: on a fiber → isolate; otherwise → today's
`_exit`. Isolation is purely additive.

**Two guarded exceptions** to "run cleanup inline":
- **OOM** (`neon_alloc` failed): the heap is consistent but memory is scarce, and a cleanup
  that allocates could nested-trap — hence the per-cleanup guard, and cleanups are
  `close()`-shaped and should not allocate.
- **Stack overflow**: a SIGSEGV on an exhausted stack, where the walk cannot run (async-signal
  unsafe). A minimal `sigaltstack` handler swaps to the scheduler, which runs the arena walk
  on its own healthy stack, then bulk-frees. The walk is the same one exit and crash use, so
  isolation makes even this corner tractable — no special path.

## Sendability: move-only Resources, copy everything else

A value crossing a channel is copied into the receiver's arena. The rules:

- **Resources are move-only, gated by a runtime `rc == 1` check** at the send/spawn boundary
  (traps if shared). No compile-time linearity, no colouring — "is this sendable" is a
  structural property (does the value contain a Resource?), computed from the repr like the
  drop-walk, and the uniqueness is a local single-threaded `rc == 1` comparison. A value
  containing a Resource inherits move-only (contagion, structural). Since a Resource lives in
  the process/shared heap (not the arena), a move just transfers the reference; the teardown
  walk of whichever fiber owns it at the time cleans it — no per-fiber resource list.
- **Everything else copies.** In an immutable language, duplicating a shared node is
  *semantically invisible* — two identical immutable copies are indistinguishable from the
  original (`==` is structural, nothing mutates). So a deep copy is always *correct*; it can
  only be bigger or slower, never wrong. Internal sharing (a DAG) is duplicated by default;
  preserving it is a measured-later optimisation (Erlang's opt-in `copy_shared`), not a
  correctness decision.
- The **size-heuristic shared heap** carries the big-value case: values above a size
  threshold are allocated on a shared refcounted heap *from birth*, so a send of them is a
  refcount bump, not a byte-copy. The threshold that avoids the copy is the same threshold
  that makes atomic `rc` affordable (big values, few rc ops), so the shared heap pays atomic
  `rc` only where it costs nothing; arenas stay non-atomic for the small, frequently-counted
  values. A shared-heap value can never contain a Resource (move-only ⟹ single-referenced ⟹
  never shared), so the shared heap is Resource-free by construction. This is Erlang's
  binary-heap split, generalised to any large value because Neon's values are uniform.

## The surface

```neon
// std::fiber
fn runtime(body: () -> null)                  // lazy entry: start the scheduler, run body as fiber 0
fn yield()                                     // cooperatively step aside

opaque record Channel[T] { .. }
fn channel[T](capacity: i64) -> Channel[T]     // 0 = rendezvous
fn send[T](c: Channel[T], v: T)                // copies into the receiver's arena (moves a Resource)
fn recv[T](c: Channel[T]) -> T | closed        // parks the caller
fn close[T](c: Channel[T])

opaque record Task[T] { .. }                   // a fiber + the one-shot channel of its result
fn spawn[T](body: () -> T) -> Task[T]          // start a fiber; its return sends on the channel
fn await[T](t: Task[T]) -> T                   // recv the result; parks the caller
```

**Lazy entry, explicit.** `fiber::runtime(body)` starts the scheduler and runs `body` as
fiber 0 from the stack pool — the OS `main` thread becomes the scheduler driver. A program
that never calls it (a plain CLI, a compiler) pays *nothing*: no scheduler, no engine, no
arenas. `runtime`, not on-first-spawn, avoids adopting `main`'s non-pool stack as a fiber.

**Join is a channel.** `Task[T]`/`await` is a thin library over a one-shot channel; the
result crosses arenas on the same copy/move path as any send. Structured concurrency (a
`scope` that awaits its tasks and surfaces the first error) is buildable on top.

**Cancellation is cooperative.** A fiber carries a cancellation token it `select`s on or
checks, notices, and *returns normally* — running its own cleanups via the ordinary path,
no injected async kill. Hard `kill(fiber)` needs true async unwind and is deferred (it is a
supervision feature, and supervision is pushed — see below).

**Deadlock is a panic.** If every fiber is parked and nothing can wake one (run queue empty,
no IO op in flight, no timer, no pool task), the scheduler panics, as Go does for "all
goroutines asleep."

## Memory in the real world

Per parked fiber, physical, back-of-envelope: **stack ~4–8 KB** (one page floor of a
guard-paged, demand-paged reservation) + **arena ~1–4 KB** (plateaued at working set) +
control ~200 B ≈ **6–9 KB**, stack-dominated. So: a plain program pays **zero** (lazy
runtime); 1K–100K fibers is 6 MB–900 MB (fine); 1M fibers is ~6–9 GB, **Go-class**, behind
Erlang's ~2.7 KB. The density lever is the **stack, not the arena** — the allocator is the
cheap part. Erlang-class density is a later **growable/copyable stack** optimisation (start
small, grow-by-copy), which the `_Thread_local` current-fiber decision left open (no
fixed-alignment requirement).

## Parallelism: M:N is the design; M=1 is the first milestone

The design is **M:N in one process** — one runtime, M OS-thread schedulers, N fibers
(`1:M:N`). Parallelism reaches multi-core WITHOUT two threads on one arena: work-stealing
migrates a whole fiber (arena and all) to another OS thread, and single-owner-at-all-times
holds (a fiber is in a run queue *or* running, never both), so arenas stay non-atomic. The
shared big-value heap's `rc` becomes atomic under M>1 — but only there, and only for big,
rarely-counted values (the size heuristic makes that free). Cross-thread sends route through
a synchronised message queue, never a direct arena write.

**The build reaches M:N via M=1 first**, and that is a milestone, not a downgrade. A single
scheduler is *deterministic*, which is the only way this codebase validates the novel parts
(the swap, the arena, crash-isolation, channels) — the oracle and the models need
reproducibility. So the mechanics are proven at M=1, then M>1 is turned on as an **additive**
slice: the run queue becomes a concurrent work-stealing deque, fibers migrate, the shared
heap's `rc` switches to atomic, and same-thread sends become cross-thread queues. The fiber,
the arena, the IO engine, and the channel *semantics* do not change. So every slice is built
**M:N-ready even while running M=1** — the run queue behind an interface, `rc` ops always
through `retain`/`release` (never open-coded `rc++`), sends through a hand-off abstraction —
so flipping M>1 rearchitects nothing.

Separately, **send-multiprocess** is its own capability, not the parallelism mechanism: OS
processes, hardware-isolated fault domains, over the `std::process` pipes; sends serialise.
`fork` vs spawn-and-serialise is its own unmade ruling. Isolated arenas were chosen so both
M:N and multiprocess stay addable without redoing the memory model.

## The swap gadget

Context switch is a small per-ABI assembly routine (save callee-saved regs, exchange `rsp`,
restore). **Vendor `boost.context`'s `fcontext`** rather than hand-roll the per-ABI stubs
(x86-64 SysV, x86-64 Windows with its TIB fields, aarch64) — the swap is not where the
correctness budget should go. **Sanitizer fiber annotations are mandatory**:
`__sanitizer_start_switch_fiber` / `__sanitizer_finish_switch_fiber` bracket every switch, or
ASan-over-corpus floods with false positives — and ASan-over-corpus is how this codebase
proves memory.

## Preemption

Cooperative, plus **compiler-inserted safepoints at loop back-edges and function entry** —
a one-instruction poll (`if (self->preempt) yield()`) so a CPU-bound fiber yields within a
bounded instruction count, and the *same* safepoint checks the cancellation token. Neon's
structural advantage over signal-based preemption. A long-running *native* has no safepoints
(it is C), so the invariant for native authors is: **a native must be short, chunk-and-yield,
or offload to the pool** — the sibling of "all blocking goes through the engine."

## Build order (the `fibers` branch)

Each slice lands green, tested, before the next — the repo's discipline applied to a big
feature.

1. **Arena allocator** — bump + size-class free lists + drop, standalone, C-tested + CBMC.
2. **Swap gadget** — vendored `fcontext`, a two-fiber cooperative swap, sanitizer
   annotations.
3. **Scheduler + `fiber::runtime` + register-pinned arena** — run queue, lifecycle, lazy
   entry, `neon_alloc` routes to the current arena.
4. **Trap isolation** — the teardown walk (release outgoing refs + bulk-free), `sigaltstack`
   for overflow,
   deadlock-panic.
5. **Channels + `Task[T]`** — copy-on-send, move-only Resources, size-heuristic shared heap.
6. **IO engine + file-offload pool + safepoints** — completion engine (io_uring/epoll first,
   IOCP later), transparent blocking, timers, preemption.
