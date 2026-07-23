# Design: coroutines — one machine, two surfaces (generators & async)

**Status:** design sketch, nothing implemented. No `gen`/`yield`/`async`/`await` token exists
in `compiler/src/` today. This is a proposal to be argued with. It grew out of the stdlib
JSON work (a DOM parser wants a streaming tokenizer, a tokenizer wants coroutines) and the
observation that coroutines fall almost entirely out of machinery neon already has for
closures and refcounting.

The through-line: **there is one mechanism — a stackless state-machine coroutine — and two
surfaces over it.** Iterators (`gen fn` / `yield` / `for`) and async (`async fn` / `gen::` /
a scheduler) are the same lowered object seen from two ends, exactly the way Python (`yield`
vs `async def`), Rust (`Iterator` vs `Future`) and C# (`IEnumerable` vs `Task`) split them.
Users meet the two familiar faces; only an effect-library author meets the raw coroutine.

Companion reading: `errors.md` (the `throws`/`try` tagged-result ABI, reused verbatim),
`resources.md` (once-only cleanup, which keeps cloning sound), `dispatch.md` (why the generic
iterator protocol is blocked), `opacity.md` (sealing), `ir.md` (the SSA/closure ABI).

---

## 0. Why coroutines are cheap in neon specifically

Three properties of the existing implementation make them cheap here where they are expensive
elsewhere. The target is a **stackless compiler state-machine transform** (like Rust/C#
`yield`), *not* stackful coroutines: no second stack, no `makecontext`. A coroutine body with
N suspension points lowers to a heap state record plus a `resume` function, and both are
ordinary refcounted values.

1. **The frame is already a value neon knows how to allocate and count.** A lifted lambda is
   `Func { env: Some(Repr), .. }` (`ir/ssa.rs:52-53`): its first parameter is a boxed tuple of
   captures. A coroutine's suspended state is the same thing plus a resume-point discriminant
   and the locals live across a yield. `Op::MakeClosure` / `Op::CallClosure` / `Op::MakeRecord`
   / `Op::Field` (`ir/ssa.rs:153-161`) already build and read exactly these.

2. **`resume` mutates the frame in place exactly when it is uniquely owned.** The
   sole-ownership pass (`ir/unique.rs`) already rewrites a consume-and-produce loop into
   in-place writes once it has established `rc == 1` (`neon_list_ensure_unique`,
   `ir/unique.rs:652`). A linearly-driven coroutine is the same shape;
   `partial::SET_FIELD_INPLACE` (`ir/refcount.rs:475-480`) is the record-level in-place write
   it needs.

3. **A re-entered frame is already a borrow.** A closure's environment parameter is
   *borrowed*, not consumed — "the closure owns it and may be called again"
   (`ir/refcount.rs:42`, `195`). A coroutine resumed repeatedly wants precisely this.

The one genuinely new thing is that a coroutine's carried state must survive a *return to the
caller and re-entry*, so it cannot live in SSA block parameters the way a loop's carried state
does. It must be reified into heap record fields. That reification *is* the transform.

---

## 1. The one mechanism

### 1.1 Shape of the lowered form

Source:

    gen fn count_up(n: i64) {
        let i = 0;
        while i < n {
            gen::yield(i);        // suspension point S1
            i = i + 1;
        }
    }

Lowers to two artifacts. **A state record** — compiler-internal, never spellable in source:

    record CountUp$state {
        resume_point: i64,   // 0 = start, 1 = after S1, 2 = done
        n: i64,              // live across S1
        i: i64,              // live across S1
    }

`Repr::Record` (`ir/repr.rs:41`), boxed and refcounted like any record. Only fields **live
across at least one yield** are stored. And **a resume function** whose entry block is a
`Term::Switch` on the discriminant (`ir/ssa.rs:218`, `SwitchKey::Int`):

    entry(state):
        switch state.resume_point { 0 => start, 1 => after_S1, default => done }
    start:      set_field_inplace(state, "i", 0); jump header
    header:     cond = state.i < state.n; branch cond then=emit else=finish
    emit:       set_field_inplace(state, "resume_point", 1); ret Yielded { value: state.i }
    after_S1:   set_field_inplace(state, "i", state.i + 1); jump header
    finish:     set_field_inplace(state, "resume_point", 2); ret Returned { value: () }

Every piece is an op that exists today. This is the classic re-entrant switch (Duff's device /
LLVM coroutine splitting).

### 1.2 Live-variable analysis across yields

The load-bearing analysis: **at each yield, which values are live-out?** Those are the fields
the state record carries. neon already computes backward liveness (`ir/refcount.rs:388-443`),
correctly collapsing projections to their owning root. Two honest gaps: it returns
*block-boundary* sets, so blocks must be split at each yield to make a yield a boundary; and
that split is a real fix, not free.

### 1.3 Where the transform runs

**During or immediately after lowering, before `ir::unique` and `ir::refcount`.** Those passes
must see the final shape with `set_field_inplace` and the borrow of the state param already
present, or they will insert a retain that manufactures the exact second reference the whole
design avoids. This is the same reasoning that puts `ir::unique` before refcount today. It is
also why the state machine is **emitted as IR and lowered to C by the existing backend, never
hand-written as backend C**: if the frame were C text, the passes that make coroutines *cheap*
(in-place resume, COW fork, §5) would never see it. See §8.

---

## 2. Driving a coroutine

### 2.1 The type

    Generator[Y, R, C]   ≡  Repr::Closure { params: [C], throws: E, ret: Step[Y, R] }

- **`Y` (Yields)** — produced at each suspension.
- **`R` (Returns)** — produced once, at the end.
- **`C` (Receives)** — consumed at each resumption; the type `gen::yield` evaluates to.

`Repr::Closure { params, throws, ret }` already exists (`ir/repr.rs:82`) — a function pointer
plus a boxed environment, and the environment *is* the state record. So `MakeClosure` /
`CallClosure` build and drive a coroutine with zero new IR.

The **return annotation names `Returns`**, and — like every function — an omitted return type
is `()`, not `never`. `Yields` and `Receives` are inferred from the body. Calling a `gen fn`
does *not* run it; it `MakeClosure`s the initial frame (discriminant 0, params stored) and
returns it. The body runs on first advance.

### 2.2 `yield`, `start`, `resume`, `next`

    fn yield [Y, C](value: Y)                       -> C        // body side; intrinsic
    fn start [Y, R, C](g: Generator[Y, R, C])       throws E -> Step[Y, R]
    fn resume[Y, R, C](g: Generator[Y, R, C], c: C) throws E -> Step[Y, R]
    fn next  [Y, R]   (g: Generator[Y, R, ()])      throws E -> Y | :done

    record Yielded[Y]  { value: Y }
    record Returned[R] { value: R }
    type Step[Y, R] = Yielded[Y] | Returned[R]

The coroutine is a **mutable frame value**: `g` is a stable handle, and `resume(g, c)` mutates
it in place under `rc == 1` (advancing it), returning the next `Step`. There is no separate
"resuming handle" type and no continuation smuggled out through the step — you keep driving the
same `g`.

- **`start(g)`** advances an unstarted frame to its first yield. It takes **no `C`**: on first
  entry nothing is parked to receive one. Starting a frame you *retained* (`rc > 1`) **clones**
  it via COW (§5), so an unstarted `Generator` is a **reusable template** — each `start` spins
  an independent run, and it is free when you don't retain it.
- **`resume(g, c)`** feeds `c` to the parked `yield` and advances. `resume` after `Return`
  throws `GeneratorDone` (a catchable error, consistent with `resources.md`'s "use-after-close
  is an error, not a trap"); a nonsense `start` of an already-running frame is the one
  unrecoverable case and panics.
- **`next(g)`** is the `C = ()` iteration convenience: it advances and returns the yielded value
  directly, or `:done` at `Return`. (If `Y` itself can be `:done`, drop to `Step`.)

`gen::yield` is the only one that cannot be a library function — it captures the continuation
and rewrites the frame, so it is an intrinsic the transform recognises. The rest are ordinary
`CallClosure`s.

### 2.3 Consuming, in practice

Narrowing in `while`/`if` conditions (which neon already does for a bare local) is what keeps
the caller clean — you never write a `match` block:

    let g = fib();
    let step = gen::start(g);
    while step is Yielded {                 // narrows step to Yielded in the body
        io::println(to_string(step.value));
        step = gen::resume(g, ());
    }
    // here step is Returned; step.value is the R

or, for pure iteration, `next` collapses `Step` to the value itself:

    let g = fib();
    let x = gen::next(g);                   // i64 | :done
    while x is i64 { io::println(to_string(x)); x = gen::next(g); }

### 2.4 Fibonacci

    use std::io;

    gen fn fib() {                          // Returns (); Yields inferred i64; Receives ()
        let a = 0;
        let b = 1;
        while true {
            gen::yield(a);
            let sum = a + b;
            a = b;
            b = sum;
        }
    }

    fn main() {
        let g = fib();
        let x = gen::next(g);
        let n = 0;
        while n < 10 and x is i64 {
            io::println(to_string(x));
            x = gen::next(g);
            n = n + 1;
        }
    }

Lazy, O(1) memory: the whole state is `{ a, b, resume_point }`, mutated in place per pull. An
infinite stream in a fixed-size value.

---

## 3. The gen / async divide

`gen fn` and `async fn` are the same lowered coroutine, presented as two surfaces. **Do not
unify them at the surface** — the confusion is the shared machine, and the fix is to keep the
two faces users already know.

| | **iterators** | **async** |
|---|---|---|
| declare | `gen fn` | `async fn` |
| suspend | `gen::yield(v)` — produce a value | `gen::io::read(..)` — an awaitable op |
| what leaves the frame | a `Yields` value | a `Trap`, to the scheduler |
| consume | `for x in g` / `gen::next` | a scheduler (`run`) |
| object type | `Generator[Y, R, C]` | `AsyncResult[R]` (§4) |

The one deliberate carryover is the **`gen::` prefix as the await marker.** In an `async fn`,
`gen::io::read(f)` is a suspension point (it yields a `Trap`); the prefix is visible exactly
where control can leave the task, the same way neon already makes you write `try` at every
throwing call. `await` is spelled `gen::`.

`async fn f() -> R` is a `gen fn` whose `Yields`/`Receives` are fixed to the scheduler's
`Trap` / `TrapResume` vocabulary — i.e. it is an `AsyncResult[R]`. Iterators never touch that
vocabulary; async never calls raw `gen::yield`.

---

## 4. Async

### 4.1 `AsyncResult` and the trap ABI

    type AsyncResult[R] = Generator[Trap, R, TrapResume]

This fixes `Yields = Trap` and `Receives = TrapResume`, leaving only `Returns` free — async has
a **fixed vocabulary**: one request language the scheduler understands, one answer language it
replies in.

    record Open { path: str }
    record Read { file: Fd }
    type Trap = Open | Read | Write | Spawn | Park | ... | Custom      // Custom: §7

    record Opened   { file: Fd }
    record ReadDone { bytes: Bytes }
    record Failed   { error: IoError }         // any trap may answer with a failure
    type TrapResume = Opened | ReadDone | Failed | ... | CustomAns

The scheduler is the `resume`-driver; a trap yield is an await point:

    fn run[R](task: AsyncResult[R]) -> R {
        let step = gen::start(task);
        while step is Yielded {
            step = gen::resume(task, perform(step.value));   // do the I/O, feed the answer back
        }
        step.value                                            // narrowed to Returned
    }

`resume`'s full result, once `throws` is folded in, is the tagged-result of `errors.md` §2.1
unchanged — no new result shape.

### 4.2 The monomorphism, hidden (curio)

`TrapResume` is monomorphic, but the answer *depends on the trap* — naming "the answer type of
this trap" needs associated types neon does not have (`dispatch.md`). So the correspondence is
not static. It is not visible either: each user-facing op is a thin wrapper — one `match`, one
trap — that narrows the answer, exactly as curio's kernel-trap wrappers do.

    internal mod gen::fs {
        gen fn open(path: str) throws IoError -> Fd {
            let ans = gen::yield(Open { path: path });
            match ans {
                is Opened => return ans.file,
                is Failed => throw ans.error,
                _         => panic("fs kernel answered Open out of contract"),
            }
        }
    }

    async fn read_file(path: str) throws IoError -> str {
        let file    = try gen::fs::open(path);       // gen:: suspends; try discharges the error
        let content = try gen::fs::read_all(file);
        return content;
    }

Two properties earn this its place: the `panic` arm is the *entire* leak, contained to the
wrapper module; and the wrapper's public signature (`open : str throws IoError -> Fd`) is
*already* the one associated types would give it — so if neon ever grows them, the `panic` arms
become dead code and are deleted, with **no change to any signature or user code.** Note also
that `gen::fs::open` inside `read_file` **auto-delegates** (drives the sub-coroutine, forwards
its `Trap`s up, binds its `Return`); `try` is orthogonal, discharging only the error channel.

### 4.3 Don't block the loop

The `gen::` variant of an I/O op yields, so the scheduler can park the task and run others. The
plain blocking variant (`io::read`) still compiles and runs correctly inside an `async fn` — it
just doesn't yield, so the loop stalls until it returns. That is the whole rule, and it is
Python/curio's: **don't block the loop.** The compiler does not forbid the blocking call; the
stalled loop is the teacher. This is a *performance* discipline (a blocking call gives correct
results, only serialized), not a correctness gate. A *soft* lint — "blocking native in an
`async fn`; did you mean `gen::io::read`?" — is available from the effect bit §5 already
computes, but it is optional polish, not part of the model.

---

## 5. Cloning, effects, and why purity is not a gate

A coroutine frame is an ordinary immutable value with COW, so it is a **persistent data
structure**: you can hold two versions at once, fork either, and neither disturbs the other.

    let ahead = gen::clone(g);              // rc bump, no copy yet
    let peek  = gen::resume(ahead, x);      // ahead diverges here -> COW pays one bounded frame copy
    // inspect peek, then drop `ahead`: rc -> 0, freed. g never moved.

`gen::clone` and a bare alias are identical at runtime (both bump the refcount; the copy is
deferred to the first divergent `resume` under `rc > 1`, bounded by the live set, never the
input, which is a shared immutable `str`). `clone` is the intent-revealing spelling. This gives
**lookahead, backtracking, and multi-shot continuations for free**: clone, try, keep the
winner, drop the losers — and "start a template twice" (§2.2) is the same COW at the start
boundary.

**The decision on effects.** Cloning a coroutine that performs side effects **duplicates those
effects**, and we accept that:

- It is **not unsafe.** Pass-by-value keeps values and resources sound however many times you
  clone. A cloned frame shares its `Resource` by refcount, and once-only cleanup runs at the
  last drop (`resources.md`) — cloning a coroutine mid-way through an open file does **not**
  double-close it. Only an explicit `println` / `send` *in the body* repeats.
- It is only **surprising**, and only under duplication. Drive a coroutine once and its effects
  happen once, exactly as written. The residual "impurity" that matters is interaction with the
  world outside the value model (writes; non-deterministic reads) — and those aren't values, so
  COW has nothing to copy. You are accepting "you can't un-print," a property of the world, not
  a hole in the language.

Therefore: **no purity gate, no effect colouring, no `gen::` ban.** `clone` / peek /
re-`start` / multi-shot are always legal on any coroutine. The purity analysis in
`ir/effects.rs` stays exactly what it is — a pessimistic two-state (`pure`/`effectful`) fixpoint
**for the optimiser only** (dead-code elimination in `ir/opt.rs`, gated so a `@pure`-less native
is never deleted). It never becomes a user-facing check; its pessimism, free for a missed DCE,
would be false-positive noise as an error.

The reify-effects technique survives as **advice, not law**: if you want to *speculate* over
effects (fork a computation that does I/O without running the I/O twice), yield the effects as
`Trap`s instead of performing them — which is what an `AsyncResult` already does, so async
tasks are fork-safe for free. That is a technique you reach for, not a rule the compiler
enforces. An optional soft lint ("cloning a coroutine that performs effects; each branch runs
them") is the most the compiler should ever say.

---

## 6. Interaction with `throws`

`resume` is itself a throwing function, and reuses the `errors.md` ABI verbatim: it returns a
positional tagged result `Union([Step[Y,R], E])`, the ok arm being the `Yielded | Returned`
union. No new shape, no new accessor.

`try` across a suspension point composes as plain CFG, *because* the transform runs after
lowering (§1.3): a `yield` inside a `try` body straddles the handler, but once lowered the
handler is an ordinary block reachable by an edge, and the state-machine split preserves edges.
The only obligations — values live into a handler across a yield get persisted like any
yield-live value; a yield can't land on the compiler-generated error edge; throwing calls
before a yield are not re-run on resume — all fall out of the liveness union and the split, not
special cases. `throws` on a `gen fn` obeys the ordinary rules (absent clause is `never`, so a
non-throwing coroutine's error arm is `Repr::Never` and short-circuits away). The effect
lattice does not model `throw` (it is a terminator, not an op); that is correct here, since a
throwing coroutine is still fork-safe — re-deriving a thrown error on a forked path is
harmless.

---

## 7. Extending the async vocabulary

**Tier 1 — new operations, by composition (free, the common case).** Most "I need a new trap"
is a new operation composed from existing ones, as in curio (`Queue`, `Semaphore`, sockets are
all userland coroutines over park/reschedule). Given a park/wake primitive, a third party
writes any synchronization primitive as a `gen fn` with no new trap and no scheduler change.

**Tier 2 — a new primitive, via one escape-hatch trap.** `Trap` is a closed union, sealed by
its alias — and it should be, so the kernel's `perform` stays exhaustive and total. A genuinely
new primitive goes through `Custom { req: any }` / `CustomAns { ans: any }` and a handler chain:

    fn perform(trap: Trap, chain: HandlerChain) -> TrapResume {
        match trap {
            is Open   => ...,
            is Custom => CustomAns { ans: chain::dispatch(trap.req) },   // delegate outward
            ...
        }
    }

A library ships private request/answer records, a registered handler that narrows the `any`
and interprets it (returning `:not_mine` to pass to the next handler), and a typed wrapper that
hides both. The core union never changes; the `any` is contained to the extending module; the
wrapper's public API is fully typed; and an **unclaimed `Custom` request `throw`s** (a
catchable `TrapUnhandled`), it does not panic. If neon later grows a typed `TrapHandler`
protocol, the `any` door and the panic arms are replaced with no change to wrapper signatures.

---

## 8. Implementation split

| layer | where | why |
|---|---|---|
| `gen fn` → state-machine transform | **compiler pass, emits IR → C** | must be visible to `unique`/`refcount` for in-place resume + fork |
| frame `ensure_unique` / clone | **C runtime** (`libneon_rt`) | low-level refcount+memcpy, sibling of `neon_list_ensure_unique` |
| scheduler, `perform` routing, handler chain, trap wrappers | **Neon** (stdlib) | policy, typed, and the Tier-2 extension surface must be user-writable |
| syscall leaves (epoll/read/timer) | **C natives** behind `@native` | raw kernel calls |

The load-bearing row is the first: emit the state machine as IR, **never** as backend C text,
or the passes that make coroutines cheap never run. Everything below is the ordinary
`runtime/`-is-C, `stdlib/`-is-Neon boundary.

---

## 9. Hard parts and open questions

None of this is implemented.

1. **Record-level sole-ownership.** `ir::unique` establishes `rc == 1` only for `List` header
   params. Both in-place `resume` (§1) and COW fork/`clone` (§5) need the same for a boxed
   *record* — a `neon_ensure_unique` on a record plus a `chain`-style analysis over frames.
   `partial::SET_FIELD_INPLACE` exists; the establishing analysis does not. This is the single
   primitive the whole design rests on; prototype it against a hand-written frame first.

2. **Per-program-point liveness.** `ir/refcount.rs::liveness` gives block-boundary sets; split
   blocks at each yield to get the live set *at* the yield, then reuse the existing dataflow.
   Small, not free.

3. **Generic `for` is two features away.** `for` is hardcoded to `List` (`lower_for`,
   `ir/lower.rs:2595-2601`). A generic `Iterator` protocol needs associated types (to name a
   stateful iterator's element) *and* union-receiver dispatch (`Resolution::Switch`, stubbed at
   `ir/lower.rs:1975`). Ship a **structural `Gen` desugar** first — one hardcoded shape, exactly
   as `List` is special-cased today — and unify later.

4. **Per-trap answer typing (§4.2).** Hidden in the wrapper layer, not solved. The sound
   version wants associated resume types; the wrappers are designed so landing that feature is a
   deletion, not a rewrite.

5. **Finalisation on early drop — the sharpest one.** A coroutine abandoned before `Return`
   whose frame holds a live `Resource` across a suspension cleans up only when the frame's `rc`
   hits zero — non-deterministic timing, in tension with the deterministic-cleanup story async
   most needs. Async makes it acute: a *cancelled task is the common case*, not the exception.
   A cancellation path that runs pending `using` cleanup deterministically on drop is the open
   question most likely to force a change to the model.

6. **Recursion and unbounded frames.** Recursive coroutines lean on `Repr::BoxedRec` /
   `Recursive` (`ir/repr.rs:99-106`, already supported). The `clone`-cost bound (§5) relies on a
   bounded yield-live set — true for parsers, not for an unbounded accumulator. State it as a
   precondition, not a guarantee.

7. **Handler-chain ordering (§7).** Registration order with first-claim-wins is the simplest
   defensible rule; pin it before anyone relies on the alternative. Unclaimed → `throw`.
