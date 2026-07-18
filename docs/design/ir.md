# The IR, and the road to a backend

Status: **design**, unbuilt. The front end (lexer → parser → typecheck) is complete
and holds at 198/198 on the corpus. This document pins the phase after it: an
intermediate representation, and the first backend behind a seam a second could also
sit behind.

## Why an IR at all, and the one lesson from the graveyard

The previous compiler lowered the typed AST straight to output, re-deriving on the way
down every type the checker had already worked out — because the checker threw its
`expr_types` away. That re-derivation could not always succeed, so it fell back to
`Erased`, `Erased` leaked into a boxed `NeonValue` with an invented vtable, the vtable
produced `*_Any` collections with 24-byte slots that `push` read as 8, and a
`list::new()` was an ASan overflow. One discarded hashmap, four subsystems of
consequence.

The IR exists so that never happens again. It consumes `TypecheckResult` (`ty` /
`call` / `lambda`, keyed by `ExprId`) and **re-derives nothing**. Every expression
already has a type; every call already has a `Resolution`; every lambda already has an
arrow. The IR's job is to make those facts *explicit and total* — a representation for
every type with no "unknown" case — and hand them to a backend.

## The shape of the pipeline

```
typed AST + TypecheckResult
  → monomorphise         (IR→IR; generics become concrete instances)
  → lower to SSA          (dispatch resolved to calls/switches; reprs assigned)
  → optimise              (IR→IR; a pass pipeline over SSA — see below)
  → insert refcounts      (IR→IR; retain/release/free made explicit)
  → optimise refcounts    (IR→IR; elide redundant pairs, in-place reuse)
  → Backend::emit         (a trait; C is the only implementation, for now)
```

Every pass above `emit` is **backend-independent**. Only the last step knows what a
target looks like. That split is the whole portability story: a second backend
reimplements `Backend`, and nothing above it moves. Refcount insertion runs *after* the
value-level optimiser on purpose — dead code is gone before its retains and releases
would be written, so they never need to be optimised away.

## Representations are abstract

The representation map is `TyId → Repr`, and `Repr` is a *descriptor*, never a C type:

```
Repr =
  | Scalar { bits: 1|8|64, kind: int | float | bool }
  | Tag { count }                         // atoms, enum-like unions: a small integer
  | Aggregate { fields: [Repr] }          // record, tuple — ordered, unnamed here
  | Union { tag: Repr, variants: [Repr] } // a tagged union
  | Box(Repr)                             // a pointer to a heap object (see below)
  | Closure { params: [Repr], ret: Repr } // fn pointer + environment
  | Runtime(RtType)                       // str, List, Map — owned by the runtime ABI
```

The C backend turns `Aggregate` into a `struct`, `Union` into a `struct { tag; union }`,
`Scalar{64,int}` into `int64_t`. An LLVM backend turns the same descriptors into LLVM
types; Cranelift into slot sets. **The IR never commits to padding, pointer width, or
field offsets** — that is each backend's arithmetic to do. This is the exact discipline
the graveyard broke when "different C structs leaked into the type system."

The map is **total by construction**: `⊤` (`any`) is the only type without a fixed
representation, and the checker forbids it except at the one deliberate erasure
boundary, so lowering meets no `any` it did not expect. The test for this pass is
mechanical — every `TyId` reachable from a corpus program maps to a `Repr` with no gap.
That test *is* the guarantee erasure cannot return.

### Word-or-box

One rule keeps representations simple and containers generic: **a value is either an
unboxed scalar that fits in a 64-bit word, or a `Box` — a pointer to a heap object.**

- `i64`, `f64`, `bool`, `null`, an atom's tag, a pointer: unboxed, one word.
- A record, a tuple, a union too wide for a word: `Box` — heap-allocated, one word at
  the use site (the pointer).
- `i64 | null` and other small unions: a two-word `Union` when they fit inline, a `Box`
  when they do not. (Nullable-of-`Box` is special-cased to a nullable pointer — `null`
  is the null pointer, no tag — but that is an optimisation, not part of the contract.)

Storing aggregates behind a `Box` in the first cut costs an allocation per record in a
list. Inline aggregate storage (a `List[Point]` with `Point`-sized slots) is a known
optimisation and is **explicitly deferred** — correctness first, then `rc == 1` reuse
and inline slots.

## The shared contract: the runtime ABI

This is the part that does not move between backends, and the part worth being precise
about. Every backend links the same `runtime/` (a C library today), so everything that
**crosses the runtime boundary** has a layout `rt.h` dictates and all backends honour.
Everything that does not is a backend's private choice.

**1. The object header.** Every heap object — every `Box`, every `str`, `List`, `Map`,
every closure environment — begins with the same header:

```c
typedef struct { uint64_t rc; void (*drop)(void*); } neon_header;
```

`rc` is the reference count; `drop` is how to free *this* object (it releases the
object's own refcounted fields, then frees). `neon_retain(void*)` and
`neon_release(void*)` operate on the header alone and are the only refcount primitives.
A backend emits calls to these two symbols; it never open-codes the count. Putting
`drop` *in the header* rather than in a type-indexed table is what lets the runtime free
an object it holds (a list element, a map value) without a compile-time switch — the
header carries its own destructor.

**2. `str`, `List`, `Map`.** Their layouts live in `rt.h` and the `neon_*` natives read
and write them directly, so they are ABI. A `List` is a growable array of **one-word
slots** plus a flag for whether those slots are boxed (so `drop` on the list releases
each element) — that single bit is all the runtime needs to manage elements
generically, and it is set by codegen, which always knows the element's `Repr`. (The
richer "value-witness" version — an ops pointer per element type instead of a bit — is
the door left open for inline aggregates later; the bit is enough for word-or-box.)

**3. Closures.** A closure value is `{ void (*fn)(void* env, ...); neon_header* env }` —
a function pointer and a boxed environment holding the captures. A native that takes a
closure (`map`, `filter`, `fold`) calls `fn(env, args...)`. This pair is ABI, because
`Mappable`'s natives invoke it.

**Everything else is backend-internal.** A boxed record's field layout past the header,
the calling convention for Neon-to-Neon calls, how control flow is emitted — a native
never reads a record's fields (natives are generic over element type via the header and
the slot), so the same backend that wrote a record is the only thing that reads it, and
its layout need not be in `rt.h`. The shared contract is deliberately *small*: header +
retain/release, the three runtime types, and the closure pair. That is the entire price
of reusing one C runtime from any backend.

## The IR itself

**SSA, with basic-block arguments rather than φ-nodes.** Every value is defined once;
where control flow joins, the merged value is a *block argument* — a block takes
parameters, and each predecessor passes them when it branches. This is the same idea as
φ-nodes but the cleaner encoding (Cranelift, MLIR and Swift SIL all use it): there is no
φ-placement pass, a loop's carried state is just the loop header's arguments, and a
backend maps a block to a label whose "parameters" are assigned before each jump.

SSA is worth it because the optimisations below want it, and because it is *cheap to
build here*: Neon is immutable, so the only source of a second definition is local
reassignment (`x = x + 1`), which becomes a fresh value, and the only joins are `if`,
`match` and `loop`, which become block arguments. There is no dominance-frontier
machinery to write.

Every value has a known `Repr`; every block ends in a terminator (`return`, `branch`,
`switch`). Instructions are **semantic, not textual**:

```
Instr =
  | Const(value, Repr)
  | Call(target, args)            // target: a resolved function or a native symbol
  | CallClosure(closure, args)
  | Switch(tag, arms)             // from Resolution::Switch, and from match
  | GetField(v, index) | MakeAggregate(Repr, fields)
  | GetTag(v) | MakeUnion(Repr, tag, payload)
  | Alloc(Repr) | Retain(v) | Release(v)
```

Dispatch arrives already decided: `Resolution::Direct` becomes a `Call`,
`Resolution::Switch` a `Switch` with a `Call` per arm, `Resolution::Bound` is discharged
by monomorphisation (below) into a `Call` to the concrete instance. There is no vtable
and no runtime method lookup — the checker settled all of it, which is the point of
`dispatch.md`.

## Monomorphisation

Generics are specialised, not boxed. `fold[T,A]` called at `(List[i64], i64)` becomes a
concrete `fold$List_i64$i64`; `Resolution::Bound { param, protocol }` is resolved by
substituting the instance's concrete type and emitting a direct call to *its* impl. This
is a pure IR→IR pass — it decides *which concrete functions and types exist*, not how
they are laid out — so it runs once, before any backend. It is also where the "monomorphic
escape hatch" the stdlib notes describe stops being an escape hatch: everything is
monomorphic here, uniformly, by construction.

Two guards worth stating: recursion through a generic (`f[T]` calling `f[Box[T]]`) is
already rejected by the checker's `TooDeep`, so monomorphisation terminates; and a
generic never instantiated is simply never emitted.

## Refcount insertion

A backend-independent pass inserts `Retain`/`Release` so every boxed value's count is
balanced: a value is retained when it is stored or captured, released when a binding
leaves scope, and the last release runs the header's `drop`. Because the language is
immutable and values are trees, there are no reference cycles to collect *except* through
`mu`-recursive types, which is why the checker already computes `is_cycle_root` — that
flag tells this pass where a cycle collector (or a conservative leak, in the first cut)
is needed. Getting the retain/release discipline right here, once, keeps every backend
from reimplementing it.

## Optimisation

SSA earns its keep here. The pipeline is a pass manager over the IR, run before backends
so every target inherits the result. The passes that matter for this language, roughly in
the order they help:

- **Inlining.** decisions.md already assumes it — "after monomorphisation and inlining a
  primitive compare is a single instruction." Monomorphisation turns every protocol call
  into a direct call to a small impl; inlining those, and the native-thin wrappers, is
  what makes `a == b` an integer compare rather than a call.
- **Constant folding and propagation.** decisions.md pins the arithmetic ("a folded
  expression and the same expression evaluated at runtime agree"), so folding is a
  correctness-preserving rewrite, not a guess.
- **Dead-code and dead-block elimination**, **simplify-CFG** (merge straight-line blocks,
  thread branches — the checker already "threads predecessors past empty forwarding
  blocks"; the same idea, on the IR).
- **GVN / CSE.** Needs the effect analysis below to know a value is safe to reuse.
- **Refcount optimisation**, run after insertion: cancel a `Retain` immediately followed
  by a `Release`, hoist/sink counts out of hot paths, and turn a copy of an object with
  `rc == 1` into an in-place mutation. This is the single largest win for a refcounted
  language and is the reason refcount *insertion* is its own pass with an optimiser after
  it.

First cut runs a minimal always-on set (fold, DCE, the obvious refcount-pair cancellation)
and grows. The point now is that the *substrate* — SSA + a pass manager — is in place, so a
pass is a self-contained addition rather than a rewrite.

## Effects, for the optimiser only

CSE, DCE and reordering must know what is safe to move or drop, which means knowing which
calls have effects. This is **not** purity in the type system — the decision to keep
purity out of signatures stands. It is an invisible, inferred, IR-level analysis:

- Each native is tagged with its effect — `neon_i64_add` is `pure`, `neon_io_println` is
  `io`, a throwing native carries `throw`. The tag can ride the annotation system
  (`@native("...") @pure`) or a small runtime table.
- A Neon function's effect is the join of its body's — inferred, transitively, never
  written by the user.
- DCE may delete a dead value only if it is `pure`; an `io` or a `throw` is preserved
  even when its result is unused. CSE may share only `pure` computations.

No surface syntax, no signature change, no monad. Codegen simply knows what it may touch.

## Textual form

The IR has a canonical text form — an SSA dump — printed behind `--emit-ir`. It exists for
two reasons: reading it is how you debug a lowering or a pass, and diffing it is how passes
are tested (dump the IR for a corpus program, hold it to a golden). A round-trip parser
(text → IR, so a pass can be exercised on hand-written IR in isolation) is a natural
follow-on and is added when the first pass wants a focused test, not before.

## The Backend trait

The seam. Roughly:

```
trait Backend {
    fn declare_type(&mut self, id, &Repr);
    fn declare_fn(&mut self, sig);
    fn emit_fn(&mut self, sig, blocks: &[Block]);
    fn finish(self) -> Artifact;   // C source now; an object file, LLVM module, ... later
}
```

`emit_c` is the only implementation. It writes C to be compiled and linked against
`runtime/` with `cc`. The trait is not speculative generality for its own sake — it is
the line that keeps runtime-ABI knowledge and lowering on one side and target syntax on
the other, and it is validated by one real backend rather than asserted by two half ones.

## What is deferred, on purpose

The *substrate* — SSA, a pass manager, the effect analysis, the textual form — is part of
the design from the start, because retrofitting SSA or effects later is a rewrite. What is
deferred is **volume**, not architecture:

- **A second backend.** The seam exists; building LLVM/Cranelift before C runs a program
  end-to-end is speculative. The value delivered now is the *boundary*, not a choice of
  targets.
- **Most optimisation passes.** The pass manager is there; the first cut runs a minimal
  always-on set and each further pass is an addition, not a redesign.
- **Inline aggregate storage** and `rc == 1` in-place reuse. Word-or-box is correct; these
  are the throughput follow-ups the git history is already reaching for.
- **The text → IR parser.** The printer comes first; the parser arrives with the first
  pass that wants to be tested on hand-written IR.
- **A cycle collector.** Only `mu`-recursive values can cycle; `is_cycle_root` marks them,
  and the first cut may leak them loudly rather than collect them.

The order of work: the representation map (`Repr`, with the no-gaps test) → the SSA IR and
lowering for the scalar/first-order subset, with its printer → `emit_c` for that subset → a
runtime with real `neon_*` bodies → widen to aggregates, closures, `Mappable`, then grow
the optimiser. First a program that prints `42`, then the language, then speed.
