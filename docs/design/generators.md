# Design: generators (`gen fn` / `yield`)

**Status:** design sketch, nothing implemented. No `gen`/`yield` token exists anywhere in
`compiler/src/` today (verified by grep). This is a proposal to be argued with, not a plan
of record. It grew out of the stdlib JSON work: a DOM parser wants a streaming tokenizer,
a streaming tokenizer wants coroutines, and coroutines turn out to fall almost entirely
out of machinery neon already has for closures and refcounting. The through-line of this
file is that **a generator is neon's existing "immutable value, mutate-under-unique" model
applied to a control-flow frame** — nothing about it is new except where explicitly flagged.

The target is a **stackless compiler state-machine transform**, like Rust iterators or C#
`yield` — *not* stackful coroutines. There is no second stack, no `makecontext`, no
segmented/growable stack. A `gen fn` body with N suspension points lowers to a heap state
record plus a `resume` function, and both are ordinary refcounted values.

Companion reading: `dispatch.md` (why the iterator-protocol story is blocked), `errors.md`
(the tagged-result ABI that `throws` reuses), `ir.md` (the SSA and closure ABI this builds
on), and `stdlib.md` §"no laziness" (the design decision this feature reopens).

---

## 0. Why this fits neon specifically

Three properties of the existing implementation make generators cheap here where they are
expensive elsewhere:

1. **A generator's frame is already a value neon knows how to allocate and count.** A lifted
   lambda is `Func { env: Some(Repr), .. }` (`ir/ssa.rs:52-53`): its first parameter is a
   boxed tuple of captures, passed as a `neon_header*`. A generator's suspended state is the
   same thing with two additions — a resume-point discriminant and the locals that are live
   across a yield. `Op::MakeClosure`/`Op::CallClosure`/`Op::MakeRecord`/`Op::Field`
   (`ir/ssa.rs:153-161`) already build and read exactly these.

2. **`resume` mutates the frame in place exactly when it is uniquely owned.** The
   sole-ownership pass (`ir/unique.rs`) already rewrites a loop that consumes-and-produces a
   `List` into in-place writes once it has *established* `rc == 1` on entry via
   `neon_list_ensure_unique` (`ir/unique.rs:652`, "clones at most once, a pointer test when
   the list already stands alone"). A linearly-consumed generator — one `for` loop draining
   it — is the same shape: `resume` reads the old field values, computes new ones, writes
   back, and under `rc == 1` those writes are in place. `partial::SET_FIELD_INPLACE` (cited
   as a borrow in `ir/refcount.rs::operand_uses`, lines 475-480) is the record-level
   in-place write this needs.

3. **The refcount pass already treats a re-entered frame as a borrow.** A closure's
   environment parameter is *borrowed*, not consumed — "the closure owns it and may be
   called again" (`ir/refcount.rs:42`, `env_param` at line 195), and `CallClosure` borrows
   its callee. A generator that is resumed repeatedly wants precisely this: `resume` borrows
   the state record, it does not consume it.

The one thing that is genuinely new is that a generator's carried state must survive a
*return to the caller and re-entry*, so it cannot live in SSA block parameters the way a
`loop`'s carried state does today (`ir/ssa.rs:5-7`, and `lower_for` at
`ir/lower.rs:2612-2625` threads carried vars as `block_param`s). Block params are ephemeral
within one activation. Generator state must be reified into heap record fields. That
reification *is* the transform.

---

## 1. The state-machine transform

### 1.1 Shape of the lowered form

Source:

    gen fn count_up(n: i64) -> i64 {
        let i = 0;
        while i < n {
            yield i;          // suspension point S1
            i = i + 1;
        }
    }

Lowers to two artifacts.

**A state record** — a compiler-internal nominal record, never spellable in source:

    // repr only; no source syntax
    record CountUp$state {
        resume_point: i64,    // discriminant: 0 = start, 1 = after S1, 2 = done
        n: i64,               // param, live across S1
        i: i64,               // local, live across S1
    }

Its `Repr` is `Repr::Record { name: Some("CountUp$state"), fields: [...] }`
(`ir/repr.rs:41`), boxed and refcounted like any record. Only fields **live across at least
one yield** are stored; a local dead before the next suspension stays an ordinary SSA value
inside `resume` and never touches the record.

**A resume function** — `fn resume(state) -> Yield(i64) | Done(unit)`, whose entry block is
a `Term::Switch` on the discriminant (`ir/ssa.rs:218`, `SwitchKey::Int` at
`ir/ssa.rs:225-231`):

    entry(state):
        rp = Field { base: state, field: "resume_point" }
        switch rp {
            0 => start,
            1 => after_S1,
            default => done_trap,
        }

    start:
        // i = 0; the while header
        set_field_inplace(state, "i", 0)
        jump header

    header:
        i = Field { base: state, field: "i" }
        n = Field { base: state, field: "n" }
        cond = i < n
        branch cond then=emit_S1 else=finish

    emit_S1:
        // persist live set, set resume point, return Yield(i)
        set_field_inplace(state, "resume_point", 1)
        // i and n already in the record
        y = MakeRecord { name: Some("Yield"), fields: [("0", i)] }
        ret Some(y)

    after_S1:
        // resumed: i = i + 1, loop
        i = Field { base: state, field: "i" }
        set_field_inplace(state, "i", i + 1)
        jump header

    finish:
        set_field_inplace(state, "resume_point", 2)
        ret Some( Done )

This is the classic re-entrant switch (Duff's device / LLVM coroutine splitting), and every
piece of it is an op that exists today: `Term::Switch`, `Op::Field`, the in-place field set,
`Op::MakeRecord`, `Term::Ret`. The transform mints new values on a finished function via
`Func::new_value` (`ir/ssa.rs:253-257`) — the same escape hatch `ir::unique` already uses.

### 1.2 Live-variable analysis across yields

The load-bearing analysis: **at each yield point, which values are live-out?** Those are
exactly the fields the state record must carry. This is a backward liveness query, and neon
already computes backward liveness — `ir/refcount.rs::liveness` (lines 388-443) is a
standard iterative backward dataflow producing per-block `live_in`/`live_out`.

Two gaps to close honestly:

- **Granularity.** `refcount::liveness` returns *block-boundary* sets only; `insert_fn`
  redoes the intra-block backward walk itself for per-instruction liveness
  (`ir/refcount.rs:379-382`). A yield is an instruction, not a block boundary, so the
  transform needs per-program-point liveness *at the yield*. The fix is small — split every
  block at each `yield` before running liveness, so each yield becomes a boundary — but it
  is a fix, not something free.

- **Root collapsing.** `refcount::liveness` tracks *owners*, collapsing views
  (`Field`/`Elem`/`Cast`/`UnwrapOk`/`UnwrapErr`) to their root via `root_base`
  (`ir/refcount.rs:158`). That is exactly right for us: a projection live across a yield is
  re-derived from its persisted owner on resume, so only owners land in the record. The
  existing pass hands us the correct set for free.

The set of yield-live owners, unioned across all N yields, is the record's field set. A
value live across yield *A* but not *B* is still a field (records are flat), but it is dead
storage on the *B* path — an acceptable cost, and the same slack a union-of-locals frame has
in every state-machine compiler.

### 1.3 Self-referential and nested state

**Delegation (`yield*`-style).** A `gen fn` that drains another generator holds the inner
generator's state record as one of *its own* fields. Nesting depth in the record tree = the
static nesting depth of `gen fn`s, which is finite and known. Cloning cost for backtracking
(§4) is bounded by this depth.

**Recursion.** A `gen fn` that resumes an instance of itself (a recursive-descent parser
generator over a recursive grammar) produces a state record whose type refers to itself.
neon's repr system already handles this: `Repr::BoxedRec(u32)` is "a pointer to a heap
record whose cycle closes by value" and `Repr::Recursive(TyId)` is the μ back-edge
(`ir/repr.rs:99-106`). The recursive state record is a boxed-record cycle like any
recursive `record`, and `Program::recursive`/`Program::boxed` (`ir/ssa.rs:24-31`) are the
side tables the backend already resolves such back-edges through. The *depth* of a recursive
generator's live record chain is the recursion depth at the suspension point — bounded by
input structure, not input length, for a parser.

### 1.4 Where the transform runs in the pipeline

It must run **during or immediately after lowering, before `ir::unique` and
`ir::refcount`**. Two reasons:

- The handler-stack interaction (§2) is cleanest if yields are lowered *with*
  `self.handlers` in scope (they become CFG edges to real handler blocks), then the split is
  a CFG-to-CFG rewrite. Running post-lowering means try/catch is already wired.
- `ir::unique` (between optimiser and refcount, `ir/unique.rs:83-89`) and `ir::refcount`
  (last IR pass, `ir/refcount.rs:54`) must see the *final* shape with `set_field_inplace`
  and `resume`'s borrow of the state param already present, or they will insert a retain
  that manufactures the exact second reference the whole design is trying to avoid — the
  same reasoning that puts `ir::unique` before refcount today.

---

## 2. Interaction with `throws` — the fiddliest bit

### 2.1 `resume` is itself a throwing function

A throwing function in neon returns a **positional tagged result** `Union([ret, err])` —
variant 0 the value, variant 1 the error, built raw (not normalised) so `Op::IsErr` /
`Op::UnwrapOk` / `Op::UnwrapErr` can address the arms by index (`ir/ssa.rs:60-74`,
`Func::result_repr`; `errors.md` §"What a throw carries"). `throws` is part of the calling
convention, not a payload the function names.

The clean answer is to **make `resume` a throwing function and reuse this ABI verbatim.**
`resume` returns `Yield[Y] | Done[R]` and *throws* `E`. Its actual result repr is then

    Union([ Union([Yield[Y], Done[R]]), E ])
          |  \_______ the ok half _______/    \_ err half _/

— the existing two-arm tagged result, where the ok arm happens to itself be a two-arm
`Yield | Done` union discriminated by `Op::IsVariant` (`ir/ssa.rs:176-188`). No new result
shape, no new accessor: a consumer does `IsErr` → on error, the generator threw; else
`UnwrapOk` → `IsVariant "Yield"` → keep going or stop. `Term::Throw` (`ir/ssa.rs:214-215`)
inside the body works unchanged — it returns the error arm of `resume`'s result.

`throws` on a `gen fn` obeys the same rules as any function (`errors.md` §`throws`): the
clause takes any type, an absent clause is `never` (so `resume`'s err half is `Repr::Never`
and `wrap_throwing_repr` short-circuits it away, `ir/lower.rs:2213-2215`), and it is
covariant.

### 2.2 `try` / `try?` / `try!` across a suspension point

The subtle part. `try` is lowered with a **lowering-time stack of handler blocks**,
`self.handlers` (`ir/lower.rs:2135`). A `try { .. } catch (e)` pushes a handler block; a
throwing call inside branches to the top handler on error (`wrap_throwing_repr`,
`ir/lower.rs:2220-2236`); `try` / `try?` / `try!` are `TryForm::Propagate` / `Soften` /
`Assert` (`ir/lower.rs:2167-2185`). Crucially, **once lowered, all of this is just CFG** —
handler blocks, `Term::Branch` edges, a join block with a parameter.

So the composition rule falls out if the transform runs *after* lowering (§1.4):

> A `yield` inside a `try` body straddles the handler. After lowering, the handler is an
> ordinary block reachable by an edge; the yield's continuation block is downstream, still
> inside the try region, and any *subsequent* throwing call in that region still has its
> error edge wired to the same shared handler block. The state-machine split preserves edges,
> so the handler is still reachable from the resume label. **Nothing per-resume needs
> re-establishing.**

The only genuine obligations the transform must honour:

1. **Values live into a handler block, across a yield, must be persisted** like any other
   yield-live value (§1.2). The handler's own error parameter arrives on the throwing edge
   (`err_param`, `ir/lower.rs:2133`) and needs no persistence; but a variable the *catch
   body* reads that was defined before the yield does. This is not special-cased — it falls
   out of the liveness union if the handler block is included in the CFG walk, which it is.

2. **A yield may not land on the error edge between a throwing call and its handler.** There
   is no user code there (`wrap_throwing_repr` generates it), so no `yield` can be placed
   there — the invariant holds by construction, not by a check.

3. **On resume, the throwing calls *before* the yield are not re-run.** The resume label is
   the yield's continuation, downstream of those calls; they already executed and their
   effects already happened. Correct for a *pure* generator by definition; for an effectful
   one this is the intended semantics (resume continues, it does not restart).

`try?` (`Soften`, → `null` to the join) and `try!` (`Assert`, → `neon_panic` +
`Term::Unreachable`) lower to blocks before the split too, so they compose identically. A
`try!` that traps inside a generator terminates the whole program, which is `try!`'s
contract everywhere.

**Open question.** The effect analysis (`ir/effects.rs`) does *not* model throwing at all —
`throw` is a terminator, not an `Op`, so it has no `op_is_effectful` arm (the survey
confirmed this; `ir/effects.rs:191-192`). That is fine for the optimiser but means the
"is this generator pure?" question that gates fork safety (§4) sees `throws` as invisible.
A generator that throws is still pure in the effect lattice's sense (no IO, no mutation) —
which is actually correct for fork safety: re-deriving a thrown error on a forked path is
harmless. Worth stating so nobody "fixes" it.

---

## 3. Type and the `for` / iterator protocol

### 3.1 The type of a generator

Propose `Gen[Y, R]` (and `Gen[Y, R, E]` when it throws, or fold `E` into the surface syntax
via a `throws` clause on the `gen fn`). But the *representation* need not be a new repr kind:
a generator is a **nullary throwing closure returning `Yield | Done`**.

    Gen[Y, R]  ≡  Repr::Closure { params: [], throws: Never,  ret: Union([Yield[Y], Done[R]]) }
    Gen[Y,R,E] ≡  Repr::Closure { params: [], throws: E,      ret: Union([Yield[Y], Done[R]]) }

`Repr::Closure { params, throws, ret }` already exists (`ir/repr.rs:82`) and is exactly a
function pointer plus a boxed environment — and the environment *is* our state record. This
means `MakeClosure`/`CallClosure` (`ir/ssa.rs:153-156`) build and drive a generator with
zero new IR. `g.resume()` is `CallClosure { callee: g, args: [] }`; the mutable env is the
suspended frame. The closure ABI's borrow-of-env (`ir/refcount.rs:195`) gives repeated
resume for free.

This also settles construction: calling a `gen fn` does *not* run its body; it
`MakeClosure`s the initial state record (discriminant 0, params stored) and returns it. The
body runs on first `resume`.

### 3.2 What `for` iterates today, and the honest blocker

Today `for` is **hardcoded to `List`**. `lower_for` (`ir/lower.rs:2595-2601`) reads
`neon_list_len` and indexes with `Op::Index`; there is no `Iterator`/`Iterable` protocol
anywhere, and `range` returns an eager `List[i64]` (`prelude.neon:41-49`). `stdlib.md`
(lines 128-138) records the decision *against* laziness explicitly, having rejected
`Iter[T] = () -> (T, Iter[T])` as "strictly worse than eager plus `rc == 1` in-place reuse".
Generators reopen that decision (§5) because a *stackless* gen fn does not pay the
per-element closure box that rejection was about.

Making `for` generic over user generators **and** native collections wants a protocol:

    protocol Iterator for T {
        fn next(it: T) throws E -> Elem[T] | :done
    }

and it does not work yet, for two independent reasons both documented elsewhere:

- **No associated types.** There is no way to spell "the element type of `T`". `Mappable`
  (`stdlib/std/collections/list.neon`) gets away with element genericity because element `T`
  is a *method* type parameter recovered from the *container argument* — `map[T,U](c: C[T],
  ..)`. A stateful `next(it: T)` has no element-bearing argument to recover the element type
  from; it would have to *return* it, which needs an associated type or a second type
  parameter on the protocol subject. Neither exists. `dispatch.md` §"Generic impls" and
  §"Bounded impls" confirm the neighbouring features are also unbuilt.

- **Union-receiver dispatch is a stub.** Even with the protocol, `for x in it` where
  `it: I where I: Iterator` resolves through `Resolution::Bound` — implemented for a
  concrete head, but the moment `I` is instantiated at a *union* it reaches the same
  unimplemented path as `Resolution::Switch`, which lowers to a string constant
  (`ir/lower.rs:1975`, `Resolution::Switch(_) => self.unhandled_note("dispatch switch", ..)`;
  `dispatch.md` §"Step 6 produces a resolution lowering cannot lower"). A union of two
  concrete iterator types would print `<todo: dispatch switch>` and exit 0.

**Near-term recommendation: structural desugar, exactly like `List` today.** `for x in g`
where `g : Gen[Y, R]` desugars *structurally* — keyed on the `Gen`/closure repr, no protocol
dispatch — to a loop that calls `resume` until `Done`:

    loop:
        step = CallClosure { callee: g, args: [] }   // or wrap_throwing if it throws
        branch (IsVariant step "Done") then=exit else=body
    body:
        x = UnwrapVariant(step, "Yield", 0)
        <user body>
        jump loop

This is the same posture as `lower_for`'s List special-case: one hardcoded shape per
iterable kind. A pragmatic unification is to make `range` and collection iteration *return
generators* — `for x in xs` desugars to `for x in list_iter(xs)` where `list_iter` is a
`gen fn` — collapsing everything onto the single `Gen` structural path. The tension: the
List fast path is index-based and benefits from in-place reuse, while a gen-based path adds
one `resume` call per element. Keep the List fast path, add the `Gen` path, and defer the
protocol unification until associated types **and** `Resolution::Switch` land. Do not pretend
the generic-`for` story is close; it is two unbuilt features away.

---

## 4. Fork-on-alias: backtracking for free

### 4.1 The claim, and why the model already supports it

> Aliasing a half-consumed generator forks its suspended state copy-on-write, yielding two
> independent continuations. A speculative parse backtracks by holding a second reference
> across the trial and paying one bounded copy only on the path that actually diverges.

This is not a new mechanism — it is `neon_list_ensure_unique` generalised from `List` to the
state record. A generator's frame is a refcounted, **non-atomic** (`ir/refcount.rs`
"immutable, acyclic, non-atomic" model, no atomics discussed) boxed record. Holding a second
reference across a trial makes `rc == 2`. The next `resume` that *mutates* the frame must
first ensure uniqueness; under `rc > 1` that clones the record (COW), producing two
independent continuations that resume from the same suspension point. Under `rc == 1` — the
committed path, no speculation live — the write is in place and costs nothing.

    let g0 = parse_value(input);     // suspended before the first token
    let trial = g0;                  // rc == 2, NO copy yet — just a refcount bump
    // speculatively try to parse a Number:
    match resume(trial) {            // trial's first mutating resume: rc==2 -> COW clones
        Yield(Number(n)) => ...      //   trial is now a fresh rc==1 frame; g0 untouched
        _ => resume(g0),             // fell through: g0 is the pristine rc==1 original
    }

The `let trial = g0` is free (a `Retain`, `ir/ssa.rs:195`). The clone is deferred to
whichever of `g0`/`trial` first mutates, and happens **at most once** — the first mutator
clones and leaves with `rc == 1`, the other keeps the original at `rc == 1`. Pure COW. This
is the identical deferral `ir/unique.rs` performs for lists; the only missing piece is a
`neon_ensure_unique` that clones a boxed *record* rather than a list buffer, plus extending
the sole-ownership *chain analysis* (`ir/unique.rs::chain`, lines 369-428, currently keyed on
`Repr::List` header params, line 338) to record-typed frames. `partial::SET_FIELD_INPLACE`
already provides the in-place write for the committed path.

### 4.2 What gets copied on a fork, and the bound

Cloning a frame copies **its own fields**: the discriminant plus the yield-live locals at the
current suspension point (§1.2). For a recursive-descent parser those live locals are the
half-built AST node, a loop index or two, and — via delegation (§1.3) — a *reference* to the
inner generator's frame. The copy is **shallow per level**: cloning the outer frame bumps
the inner frame's `rc`, and the inner frame is itself cloned only if and when the fork
actually resumes *into* it and mutates it. So a fork clones **O(nesting depth) records,
lazily, along the resumed spine** — not O(total suspended state), and emphatically not
O(input size).

The parse **input is never copied.** It is a shared `str`/slice (`Repr::Str` /
`docs/design/ir.md:241` `data/len/owner`), refcounted and never mutated; a fork bumps its
count. Backtracking copies *parser frames*, not *text*. That is the entire win: an
alternation `A | B` in a grammar tries `A` on a forked frame over the shared input, and on
failure resumes `B` from the pristine original, having paid one bounded frame-chain clone.

### 4.3 Footgun or feature: implicit vs explicit `.fork()`

Implicit COW is **sound and free for a pure generator** — the parser case, pure over an
immutable input. Value semantics are preserved: you cannot *observe* the shared mutation,
because the first mutator forks away from it. This is the same guarantee neon's whole
immutable-value model gives for lists, extended to frames.

The footgun is an **effectful** generator (one that does IO per element, `println` per
token). Forking it and resuming both halves runs the side effects twice, which value
semantics cannot hide. neon's effect lattice can *see* this: it is binary pure/effectful
(`ir/effects.rs:1-8`), and a generator whose body reaches an effectful native is effectful.
Recommendation, given neon has no linear types to lean on:

- **Pure generator:** implicit COW fork on alias. Free, sound, invisible. Ship it.
- **Effectful generator:** a **diagnostic**, not a silent fork — "this generator is
  effectful; aliasing it re-runs its effects on resume; write `.fork()` to say you mean it."
  The effect bit is already computed (`effects::analyze` returns `HashMap<String, bool>`), so
  the lint is a lookup, not new analysis. `.fork()` then makes the double-execution a thing
  the author wrote, not a thing that happened.

Cost summary, stated plainly: the committed path pays **nothing** (`rc == 1`, in-place
resume). The speculative path pays **one O(depth) lazy frame-chain clone**, only on the
prefix that actually diverges, only when it first mutates. This is strictly the "pay once, on
the speculative path only" claim, and it holds against the real refcount model.

**Caveat worth flagging:** the O(depth) bound assumes the yield-live set at any suspension
point is bounded (parser frames are). A generator whose live set grows without bound (an
accumulator that never releases) has unbounded fork cost — fork-on-alias is cheap for
*shaped* state, not arbitrary state. And non-termination is already an effect
(`effects::may_diverge`, back-edge/call-graph-cycle detection, `ir/effects.rs`), so an
infinite generator is expected and fine — but forking one mid-stream still only copies the
current finite frame, which is the point.

---

## 5. Payoffs beyond JSON

### 5.1 Lazy `map`/`filter` with no intermediate lists

`Mappable` for `List` builds a fresh `List[U]` per stage (`stdlib/std/collections/list.neon`,
`map` pushes into `out`, `filter` into another `out`). A pipeline `xs.map(f).filter(g)`
allocates **two** intermediate lists. Generator `map`/`filter` are `gen fn`s wrapping the
source generator; they **fuse into one pass** with constant extra memory and no intermediate
list.

Crucially this is the case `stdlib.md` §"no laziness" *rejected the old design over*: an
`Iter[T] = () -> (T, Iter[T])` closure-stream "boxes a closure per element". A stackless
`gen fn` allocates **one state record for the whole pipeline**, not one per element — so it
beats *both* eager-with-intermediates (fewer allocations) *and* the closure-stream it
rejected (no per-element box). That is the argument that earns generators their place where
the earlier lazy proposal did not: the objection was per-element boxing, and the state-machine
transform has none.

### 5.2 Streaming encode in constant memory

A JSON (or any) encoder as `gen fn encode(v) -> str` yields document chunks; the driver
writes each to a socket or file and drops it. Peak memory is one chunk plus the frame, not
the whole serialised document — versus `string::concat` (`stdlib/std/string.neon`) building
the entire output first. Same story for CSV, log formatting, template rendering: constant
memory output from a straight-line producer.

### 5.3 SAX tokenizer as straight-line code

A tokenizer written as `gen fn tokenize(input) -> Token` is an ordinary loop with `yield
token` — read top to bottom. The equivalent today is a hand-rolled struct with an explicit
`pos`/`state` field and a `next()` that re-derives where it was on every call (the shape
`string::split`'s `while at >= 0` loop hints at, but pushed into a caller-driven pull model).
The state-machine transform *generates* that struct (§1), so the **source stays
straight-line** while the **runtime is a pull-model state machine**. This is the same
ergonomic trade C# and Rust ship the feature for, and it composes directly with §5.1: a DOM
parser is `parse(tokenize(input))` where `parse` is itself a `gen fn` consuming tokens and
`fork`ing (§4) at each grammar alternation.

---

## 6. Hard parts and open questions, in one place

Honest accounting; none of this is implemented.

1. **Record-level sole-ownership.** `ir/unique.rs` establishes `rc == 1` only for `List`
   header params (`neon_list_ensure_unique`, chain keyed on `Repr::List`). Both in-place
   `resume` (§1.1) and COW fork (§4) need the same for a boxed *record* — a
   `neon_ensure_unique` on a record plus a `chain`-style analysis over record frames.
   `partial::SET_FIELD_INPLACE` exists; the establishing analysis does not.

2. **Per-program-point liveness.** `ir/refcount.rs::liveness` gives block-boundary sets;
   the transform needs the live set *at each yield*. Split blocks at yields, then reuse the
   existing (correctly root-collapsed) dataflow. Small, but not free.

3. **Generic `for` is two features away.** An `Iterator` protocol needs associated types
   (to name the element of a stateful iterator — `Mappable`'s method-param trick does not
   apply) *and* union-receiver dispatch (`Resolution::Switch`, stubbed at
   `ir/lower.rs:1975`). Ship a **structural `Gen` desugar** first, exactly as `List` is
   special-cased in `lower_for` today; unify later.

4. **throws-across-yield ordering.** Lower `gen fn` bodies yield-aware (handlers in scope)
   and split the CFG afterward, so try/catch composes as plain edges (§2.2). Make `resume` a
   throwing function and reuse the entire tagged-result ABI (§2.1) — the risk is an
   implementation that invents a bespoke 3-way result instead.

5. **Effectful-generator fork.** No linear types, so gate the footgun on the effect bit with
   a diagnostic and an explicit `.fork()` (§4.3). Note the effect lattice does not see
   `throws` (§2.2), which is correct here but surprising.

6. **Recursion and unbounded frames.** Recursive generators lean on
   `Repr::BoxedRec`/`Recursive` (already supported); the fork-cost bound relies on a bounded
   yield-live set (true for parsers, not for arbitrary generators). State it as a
   precondition, not a guarantee.

7. **The immutability story, stated for the record.** A generator's state record is
   *mutated* by `resume`, yet neon records are immutable values. The resolution is the same
   one lists already use: mutation is unobservable because it only happens under `rc == 1`,
   and aliasing forks to value semantics. The state record is to a generator what the buffer
   is to a `List`. If that equivalence holds under scrutiny, generators are not a new
   evaluation model bolted onto neon — they are the existing one, applied to control flow.
