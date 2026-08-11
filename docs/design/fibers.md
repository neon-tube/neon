# Fibers — isolated-arena green threads, refcount-safe by construction

**Status: built.** The runtime, the language surface, M:N scheduling, and an io_uring backend
are implemented and tested on the `fibers` branch. This document describes what the system
**is**; the last section is an honest list of what it is **not** yet. Where this file and a
module comment disagree, the module comment is nearer the code and this file is the bug.

Verified continuously by: 190 C unit tests under ASan/UBSan/LSan and 315 backend trials (a
corpus program compiled, linked against the sanitized runtime, run, and diffed), **each run
against both IO engines**; 20 of those corpus programs are fiber programs.

## The model, as a user meets it

A fiber is a lightweight thread; spawn thousands, the runtime schedules them across cores.
Each fiber has its own memory — values crossing between fibers are deep copies, so there
are no data races and no locks to write. Channels are how fibers talk: `recv` blocks the
fiber and returns `null` once the channel is closed and drained; `send` throws a catchable
`ClosedError`, and a send that threw did not send. Resources (files, sockets) are the one
thing that does not copy: each has exactly **one owner fiber**, and ownership moves in
exactly three visible ways — a `move` lambda, a channel send, a task return. After giving a
resource away, the old binding is a stale name that throws `NotOwnerError`; a crashed fiber's
resources are cleaned up in its teardown, and a resource abandoned inside a dropped channel
is closed by the channel. Every mistake is a named, catchable error.

## The constraint everything answers to

Every heap object carries a non-atomic `uint32_t rc`. Two OS threads touching one object's
`rc` concurrently is a data race and a use-after-free. So the whole design honours one rule:

> **No ordinary heap object's refcount is ever touched by two OS threads at once.**

The chosen way to honour it is **isolation**: each fiber has its own heap (arena), and an
ordinary value lives in exactly one arena. Cross-fiber sharing of a reference is structurally
impossible, so the non-atomic `rc` is safe by construction — not by a lock, not by
serialisation, not by an atomic.

The exceptions are enumerable and small: the *bodies* of channels and tasks, which are shared
by design and count atomically through their own entry points (see **Handles**). Nothing else
crosses, so `neon_retain`/`neon_release` never learned about threads at all.

## What a fiber is

A **stackful, cooperatively-scheduled green thread**: its own stack, its own arena, suspended
and resumed by swapping the stack pointer. The defining property:

> **A fiber suspends by swapping its whole stack, so any ordinary `fn` can block-and-yield
> transparently, and NO function signature changes.** No `async`, no colouring, no `await`
> keyword on IO. `fs::read(p) -> str` is the same function on a fiber and on `main`.

Colouring exists only because *stackless* `async` compiles suspension into a compile-time
state machine, forcing "can suspend" into the type. A stackful fiber keeps a real stack; its
locals live on it; the swap preserves them; nothing colours. Java's Loom is the industrial
proof: `InputStream.read()` did not change signature.

The swap gadget is ours: ~15 instructions of x86-64 SysV assembly
(`src/fiber_swap_x86_64_sysv.S`) saving the callee-saved registers and the FP control words,
storing the suspended `rsp`, loading the target's. Not vendored — a cooperative context
switch is small enough to own, audit, and comment. Every switch is bracketed with
AddressSanitizer's fiber annotations (`__sanitizer_start/finish_switch_fiber`); without them
ASan's fake-stack tracking corrupts across a raw stack swap.

## The heap: per-fiber arenas

Each fiber allocates from its own arena (`src/arena.c`):

- **Bump-pointer for fresh allocation**, with the arena pointer in an `initial-exec`
  `_Thread_local`.
- **Size-class free lists for reclaim** — a count reaching zero pushes the slot to its class
  list; a later allocation pops it before bumping. This is what makes memory **plateau
  instead of grow**: arena size is the sum over classes of each class's high-water mark of
  simultaneously-live objects. Pure bump would charge *total* allocation and leak a
  long-lived fiber; size classes charge *peak live* and cannot.
- **Walkable** — a freed slot keeps its size class (the free-list link rides in the `drop`
  field instead) and sets a FREED bit, so the teardown walk can step slot to slot.
- **Dropped whole at death**, after the teardown walk below.

**This is the GC, and it needs no scan and no compaction.** Immutability ⟹ no cycles ⟹
refcounting is *complete*; the arena-drop gives the bulk-free that copying collectors compact
to reclaim. Defragmentation is unneeded: short fibers die before fragmenting, and size
classes bound the residual to a plateau. So Neon gets complete reclamation, bulk young-object
free, zero leaks, **stable pointers** (no relocation, no barriers, trivial FFI) and
deterministic destruction, with none of the machinery.

**Measured (2026-08-10):** in-fiber arena allocation is *indistinguishable* from the global
slab — 23–26 ms both, on a 4M-node binary-trees pattern with safepoints on. The design
originally called a dedicated arena register "critical"; measurement says it is not. What
matters, and what isolation buys, is never paying atomics and never paying general-dynamic
TLS (see the TLS rule in `src/internal.h`). A pinned register would save the last ~1% at the
cost of burning a register in all generated code and coupling build flags across the archive
seam; it waits for a profile that names the load, and none has.

## Handles: a channel is two objects

A channel or task has a shared **body** — ring, waiter lists, lock, result slot — that
genuinely lives across fibers and threads. The Neon value `Channel[T]` is *not* that body: it
is a per-holder **ref**, an ordinary object in the holder's own arena owning exactly one
reference to the body.

The split makes one bug unrepresentable. With a bare pointer, a handle held only in a crashed
fiber's **locals** — never stored in any arena object — was invisible to the teardown walk,
and the body (with everything buffered in it) leaked. Locals die with the abandoned stack and
there are no stack maps to find them; conservative stack scanning is sound for a *tracing*
collector (over-finding merely retains) but **unsound for a refcount release**, where a stale
word that looks like a pointer would double-free. Making the handle arena-resident solves it
structurally: the walk finds every live ref by construction and its drop releases the body.

It also confines concurrency. A ref never crosses threads — a handle crossing fibers is a
*copy*, which mints a fresh ref in the destination's heap — so a ref has a plain,
thread-local count and codegen emits the generic retain/release for handles. Atomic counting
lives in one file, where a ref's birth and death touch the body. Same shape as `neon_str`'s
data + owner.

## Crash semantics: a trap kills the fiber and leaves no mess

A trap outside a fiber is `_exit(101)`, unchanged. Inside a fiber it kills **just that
fiber**, because Neon's traps are *preventive*: a bounds check traps before the bad access,
overflow before the wrong value. The heap is consistent at the trap point — that is what the
trap is for — so cleanup can safely run.

**The mechanism is the swap gadget, not `setjmp`/`longjmp`.** A crashing fiber switches back
to the scheduler exactly as a finishing one does, flagged crashed. A cross-stack `longjmp`
would trip glibc's `__longjmp_chk` and side-step the ASan annotations; killing a fiber *is* a
context switch, and we already have a correct one.

**Teardown is a walk, in two passes**, and it runs for crash and normal exit alike:

1. **Seal** every live object in the arena with the `IMMORTAL` flag. From then on any release
   aimed at it is a no-op through the guard `neon_release` has always had.
2. **Force-drop** each one (`rc = 0`, then `h->drop(h)`). A Resource runs its cleanup; a
   container releases what it holds — and because siblings are sealed, only *outgoing*
   references really count down. Exactly-once falls out of the seal.
3. **Bulk-free** the arena.

The seal is why this costs the hot path nothing. The first implementation instead taught
`neon_release` a teardown-mode test — one branch with a TLS load — and that cost **30% on
binary-trees**. Reusing a guard that already existed costs zero. Soundness rests on isolation
(no other fiber can reference this arena's objects) and on sendability (a shared object never
references an arena one, so external release cascades cannot re-enter).

**Stack overflow** is the one case that is a real fault: fiber stacks are `mmap`'d with a
`PROT_NONE` guard page, and on Linux a SIGSEGV handler on a `sigaltstack` **rewrites the
interrupted `ucontext`** to resume on a healthy recovery stack, which then performs the
ordinary kill. (A handler that switched away and never returned would leave the altstack
marked in-use and the signal blocked.) Under ASan the handler is left off so ASan's own
overflow detection stands; the isolation path is validated by
`tests/manual/fiber_overflow.c`.

**Deadlock is a trap, never a hang.** When no seat can run anything, nothing external is in
flight, and fibers are still live, the last seat to go idle says so — after re-checking every
seat's queues, because a wake already pushed into an injection queue is work no counter
reflects until that seat looks.

## Sendability: what crosses, and how

A value crossing a channel, a spawn argument, a capture, or a task result is **deep-copied**,
through a `copy` operation on the value witness that codegen emits per repr:

- **scalars** — the bitcopy is the copy (a NULL `copy` slot);
- **str, list, map, boxed `any`** — runtime helpers that rebuild recursively;
- **records, tuples, unions, boxed records** — generated walkers (bitcopy, then overwrite the
  heap parts; unions switch on the tag; boxed records get per-type functions, forward-declared
  so mutually recursive shapes terminate);
- **channels and tasks** — identity: retain the body, mint a fresh ref (see **Handles**);
- **resources** — identity **plus ownership**: mint a fresh ref, and at a transfer edge the
  baton moves with it (see **Resources: owned identities** below);
- **closures** — a **trapping** copy when they capture, never NULL. Loudness over corruption.

Deep copy is always *correct* in an immutable language: two identical immutable values are
indistinguishable, so duplicating is semantically invisible. It can only be bigger or slower,
never wrong.

Where the copy *lands* depends on the lifetime shape, and this is what keeps atomics out of
the generic path:

- **Rendezvous** (a receiver is already parked): the copy goes **straight into the receiver's
  arena**. A parked receiver's arena is exclusively the sender's for the handoff — parkedness
  is the lock, even under M:N. One hop, plain counts.
- **Staging** (a buffered send, a spawn argument, an env copy, a task result): the plain slab.
  Ownership is *sequential* — producer creates, container owns, exactly one consumer takes
  over, with the channel lock as the visibility barrier — so plain counts are sound. `recv`
  and `await` then **restage** into the consumer's own arena and release the staging copy.
  Two hops for a buffered value: the price of a generic refcount path with no atomics in it.

A **capturing** lambda crosses too: its environment is deep-copied through the *env-copy
table*, which maps an environment's `drop` function (the runtime's only handle on its shape)
to a generated copy. The drop-pointer-as-type-id trick, in its second use after teardown.

## Resources: owned identities and the baton

A resource is the one value that does not copy — there is only one file descriptor — and
for a while that made it second-class: it could not cross fibers at all, and `tcp::serve`
had to launder descriptors through integers to hand connections out. What is built instead
makes the resource an **identity with an owner**, and the server loop a thing any user
writes:

```neon
while true {
    let c = try tcp::accept(l);
    fiber::spawn(move () => handler(c));
}
```

**The shape.** `Resource[T, E]` is the channel split again (see **Handles**): a shared
**body** — payload, cleanup closure, armed flag, and an `owner` fiber — plus per-holder
arena-resident **refs**, of which exactly one is the **owning ref**. Crossing any boundary
mints a ref, uniformly; the identity travels freely. The **baton** — the right to use the
resource and the duty to clean it up — moves only at three edges, each visible in source:

1. a **`move` lambda** (`fiber::spawn(move () => handler(c))`) — the new closure-head
   keyword, whose env copy is a transfer;
2. a **channel send** — with the guarantee that a send which throws `ClosedError` did NOT
   send, so the baton stayed;
3. a **task return** — `await`'s restage claims it for the awaiter.

A transfer demotes the source ref, marks the minted ref owning, and leaves the body
**unowned**; the far side's first use claims it. Every write to `owner` happens on the
fiber holding the unique owning ref, with visibility riding the same publish edge the value
crossed on — so ownership costs **zero atomics**, like everything else here. The body's own
count is concurrent exactly at ref birth/death, the channel-body discipline.

**Failure is a name, not a trap.** Using a resource from a fiber that does not own it
throws `NotOwnerError`; using one whose cleanup ran throws `ReleasedError`; both are
catchable. `release`/`take` by a non-owner are no-ops by the double-release precedent — a
moved-away handle owes no cleanup from here — which keeps `release` throwing only `E` and
preserves `Resource[T, never]`'s no-`try` release. The compiler rejects the mistake it can
see statically: a plain (non-`move`) literal lambda capturing a resource at a
spawn/task/runtime site is a compile error suggesting `move`, and a `move` lambda with no
resource to move is an error too. What the compiler cannot see (a resource behind `any`, a
closure arriving through a variable) arrives as a non-owning ref and throws at first use —
the runtime backstop.

**Cleanup is deterministic and exactly-once.** It rides the owning ref: last use under
ARC (which means a `get` that is a binding's final use runs cleanup *inside* the call,
after the payload is retained out — the lock-guard ordering `using` documents), scope end,
explicit `release`, or crash teardown — the arena walk finds the owning ref by
construction, the crash-locals argument a third time. A resource sent but never received
sits in the channel with its baton, and the channel's drop closes it: dead letters cannot
leak. The body lives on the shared heap so it can outlive its creator; its payload is
copied there at construction, which is also where an unsendable payload or a capturing
cleanup's environment fails — at `new`, the one line where the author is present.

**`move` is self-scoping.** Values always copy; the keyword affects only resources, so
`move () => ...` capturing a resource, a list, and a string moves one baton and copies the
rest. Under the hood a `move` lambda's env forks its identity in the env-copy table (the
table is keyed by drop pointer, and the transfer bracket lives inside the move env's copy),
which is why the keyword costs nothing when unused.

```neon
// std::fiber
fn runtime(body: () -> ())                        // lazy entry; returns when the tree is done
fn runtime_threads(threads: i64, body: () -> ())  // the same, on N scheduler seats
fn spawn(body: () -> ())                          // capturing lambdas welcome
fn spawn_with[T](body: (T) -> (), arg: T)         // hand data over explicitly
fn yield()

// std::channel
opaque record Channel[T] { .. }
fn new[T]() -> Channel[T]                         // unbounded: sends never block
fn bounded[T](n: i64) -> Channel[T]               // backpressure: a full send parks
fn send[T](ch: Channel[T], v: T)
fn recv[T](ch: Channel[T]) -> T | null            // null == closed and drained
fn recv_timeout[T](ch: Channel[T], millis: i64) -> T | null
fn select_recv[T](channels: List[Channel[T]]) -> (i64, T | null) // index fired + value/null
fn close[T](ch: Channel[T])
fn is_closed[T](ch: Channel[T]) -> bool

// std::task
opaque record Task[T] { .. }
fn spawn[T](body: () -> T) -> Task[T]
fn await[T](t: Task[T]) -> T                      // one-shot; traps if the task crashed

// std::sys
fn has_fibers() -> bool
fn io_engine() -> IoEngine                        // :io_uring | :epoll | :none
```

Everything above is `@native` + `@runtime` **stdlib declarations**. Adding fibers to the
language needed no typechecker change: a Neon closure is already a 16-byte `neon_closure`
passed by value, a named function used as a closure already gets an adapter thunk, and
`@runtime("neon_channel_ref")` is how a stdlib type names a runtime-backed representation.
Codegen assists only where a generic value crosses the ABI (witnesses, per-repr call shims,
the env-copy table, the `T | null` that `recv` returns).

**Lazy entry.** A program that never calls `fiber::runtime` pays *nothing*: no scheduler, no
engine, no arenas, and — because safepoints are emitted only in fiber programs — C output
byte-identical to a pre-fiber build.

**Failure propagates along await edges.** A task whose body traps marks the task failed and
wakes its awaiter, whose `await` then traps — killing that fiber cleanly, which cascades to
*its* awaiter. Fibers that await nothing are unaffected.

## Parallelism: M:N, by affinity

`fiber::runtime_threads(n, body)` runs `n` scheduler seats. Each seat is exactly the M=1
engine; **fibers are pinned** to a home seat at spawn (round-robin), and every later wake
routes to that home. So a fiber's arena is only ever touched by its own thread, and the whole
isolation argument survives multithreading untouched.

The mesh's moving parts: a per-seat lock guarding `{injection queue, sleepers}` (the local run
queue stays owner-only and lock-free), an `eventfd` doorbell per seat registered in whatever
engine that seat idles in, global atomic live/external/waiting counts, shutdown by whoever
reaps the last fiber, and per-channel/per-task mutexes.

Two races worth remembering, both found by hammering rather than reasoning:

- **Admission must be idempotent.** A deadline firing and a channel delivery racing for the
  same parked fiber would enqueue it twice and corrupt the intrusive queue. Every admission
  path now claims an atomic `queued` bit; the loser's wake dissolves.
- **The park/wake race is unlosable *because* of pinning.** Only the home seat resumes a
  fiber, and only after the park's switch completes — so publish-then-unlock-then-park is
  safe.

## IO: a completion engine, with a readiness fallback

Two seams, and everything above them is engine-agnostic: the **operation hooks**
(`neon_fiber_blocking_read`/`_writev`, armed only inside a runtime) and the **pump's idle
wait**.

- **io_uring** (`src/fiber_uring.c`) — raw, no liburing: two syscalls and three `mmap`s
  against `<linux/io_uring.h>`. One ring per seat (single-issuer, hence lock-free). READ and
  WRITEV are submitted as operations, so **regular files need no worker threads at all**;
  POLL_ADD serves readiness (pidfd waits, the doorbell); the idle wait is one
  `io_uring_enter(GETEVENTS)` carrying a TIMEOUT SQE for the nearest sleep deadline — the
  wait, the deadline, and the reap in a single call. The ring-index barriers are written as
  explicit `__atomic` orderings (release on SQ tail, acquire on CQ tail, release on CQ head)
  so the code is correct on aarch64, not accidentally-correct on x86's TSO.
- **epoll + an offload pool** — the fallback. Readiness for descriptors; regular files, which
  epoll cannot wait on, go to a two-thread pool whose workers run *raw syscalls only* (never a
  neon object, a refcount, or an arena — that is what makes a thread pool sound here) and
  ring an eventfd the scheduler watches.

Selected by feature detection at seat open: `ENOSYS` (old kernel), `EPERM` (seccomp), or
`NEON_IO=epoll` picks the fallback. **Both engines run the entire test suite**, because a
backend nobody exercises is a backend that rots.

Transparently fiber-aware today: `time::sleep` (parks on a deadline list), the `fs`/`io`
read and write paths (hooked at the syscall level, so everything above them came along for
free), and `process::wait` (parks on a pidfd).

## Preemption

Cooperative scheduling starves on a fiber that never yields, so codegen emits
`neon_fiber_safepoint()` at every **loop back-edge** — in fiber programs only. The safepoint
is a flag check that yields when a **per-seat** CPU-time timer (`CLOCK_THREAD_CPUTIME_ID` +
`SIGEV_THREAD_ID`, 10 ms) has ticked. Per-seat matters under M:N: a process-wide timer
delivers to one arbitrary thread, so only that thread's hot loops would ever be preempted.

This is the JVM's design (safepoint polls at back-edges, flag armed externally). Back-edge
placement specifically avoids Go's pre-1.14 hole, where a tight loop containing no calls was
unpreemptible. Measured cost: **~1%** on a loop whose body is a single `bxor`, unmeasurable on
anything realistic, and zero in non-fiber programs.

## Memory in the real world

Per parked fiber: **stack ~4–8 KB** (a guard-paged, demand-paged reservation) + **arena
~1–4 KB** (plateaued at working set) + control ~200 B ≈ **6–9 KB**, stack-dominated. A plain
program pays **zero**. 1K–100K fibers is 6 MB–900 MB; 1M fibers is ~6–9 GB — Go-class, behind
Erlang's ~2.7 KB. The density lever is the **stack, not the arena**.

---

## What is not built

Everything below is deliberate scope, not oversight. Each is independent of the others.

### Semantics and API

- **Send-select and heterogeneous select.** Receive-select is built:
  `channel::select_recv[T](List[Channel[T]]) -> (i64, T | null)` blocks until one channel
  yields a value or closes, takes from exactly that one, and leaves the rest untouched — a
  waiter linked into every channel's receiver list, a single atomic claim electing the one
  winner, canonical (address-ordered) locking against deadlock, and a lazy unlink walk on
  wake. What remains: a **send-select** (parking a value on whichever channel drains first),
  a **heterogeneous** select over channels of different element types (which needs union
  machinery to say which type came back), and `select_recv_timeout` (the deadline park the
  runtime already has for `recv_timeout`).
- **Cancellation.** A fiber carrying a token it checks, returning normally so its own cleanups
  run through the ordinary path. The safepoint machinery is exactly the place to poll one.
  Hard `kill(fiber)` is a separate, harder question and stays deferred.
- **Structured concurrency / supervision.** A `scope` that awaits its children and surfaces
  the first failure. `Task` failure already propagates along await edges, which is the
  primitive it would be built from.
- **Timers beyond `sleep`.** No interval/ticker, no timeout combinator over an arbitrary
  operation (`recv_timeout` is the one timed op).
- **Fiber-local storage.** Not designed. Arenas make it cheap if it is ever wanted.
- **One awaiter per task.** A second `await` traps. Multi-await would need the result to be
  copied rather than restaged out once.
- **Fiber-local resources.** A resource whose baton must never leave its creating fiber
  (a future mutex guard, a transaction). Deliberately unshipped until the first such type
  exists; the design notes weigh a construction-time flag against a second nominal type.
- **Split reader/writer.** Single ownership forbids one fiber reading a socket while
  another writes it. The escape hatch, if ever needed, is an explicit `tcp::split` doing an
  OS-level `dup` — opt-in sharing, loudly named.

### Performance, all profile-gated

None of these has a benchmark asking for it; that is the bar for starting one.

- **Size-heuristic shared heap.** Big values still deep-copy on send. The design's plan —
  allocate above a threshold on a shared refcounted heap from birth, so a send is a refcount
  bump — is unimplemented, and the threshold that avoids the copy is the same threshold that
  makes atomic `rc` affordable.
- **Sharing-preserving copy.** A DAG sent through a channel is duplicated. Erlang's opt-in
  `copy_shared` is the model.
- **Register-pinned arena.** Measured unnecessary (see **The heap**).
- **Work-stealing.** Fibers are pinned, so a seat with an empty queue idles while another has
  a backlog. Stealing would move a fiber's arena to another thread, which the isolation
  argument currently forbids — it needs a real answer (steal-on-park, or arena hand-off), not
  just a deque.
- **Growable stacks.** The path to Erlang-class fiber density.
- **Ring tuning.** 256 SQEs per seat, two offload workers, 10 ms quantum: all fixed constants
  that no measurement has yet challenged.

### Platform

- **aarch64 and Win64 swap gadgets.** ~15 instructions each; fibers are x86-64 SysV only, and
  on every other target the sources are not compiled and `std::sys::has_fibers()` answers
  false.
- **IOCP (Windows) and kqueue (BSD/macOS)** backends behind the same two seams.

### Verification

- **Scheduler models cover three invariants** (ring FIFO across a wrapping growth,
  rendezvous handover, admission-bit idempotence); the deadline race and the language
  channel are documented as intractable under CBMC in the models' SCOPE sections. The
  park/wake race under genuine concurrency remains unmodelled.
- **No fiber benchmark.** `bench/` has no concurrency program, so there is no standing number
  for spawn cost, channel throughput, or mesh scaling — only correctness tests.

---

## How it was built

Slices, each landing green before the next, on the `fibers` branch:

1. **Arena allocator** — bump + size-class free lists + drop. C-tested, plus a CBMC model.
2. **Swap gadget** — our own x86-64 SysV switch, ASan fiber annotations mandatory.
3. **Scheduler** — run queue, lazy `fiber::runtime`, `neon_alloc` routed to the current arena.
4. **Trap isolation** — walkability + the teardown walk; crash isolation via the gadget;
   guard-page stacks and overflow recovery.
5. **Channels + `Task[T]`** — park/wake, buffered and rendezvous paths.
6. **IO seam + safepoints.**

Then the language surface (`std::fiber`, `std::channel`, `std::task`), the completeness set
(teardown wired, capturing spawn, sendable tasks and failure propagation, fiber-aware
`sleep`, bounded channels, the offload pool), M:N, io_uring, and the handle split.

The recurring lesson, paid for three times: **`neon_retain`/`neon_release` admit no
additions.** An inlined slab body cost 26%, a teardown-mode branch 30%, an atomic arm 44%,
and even an out-of-line cold call reachable from the guard 33%. Every design decision that
looks indirect — the seal, the receiver-arena copy, handles as two objects — exists because
that pair had to stay exactly as it was. binary-trees is 0.30 s, unchanged from before fibers
existed.
