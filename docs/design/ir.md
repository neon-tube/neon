# The IR, and the road to a backend

Status: **design**, unbuilt. The front end (lexer → parser → typecheck) is complete
and holds at 198/198 on the corpus. This document pins the phase after it: an
intermediate representation, and the first backend behind a seam a second could also
sit behind.

## Decisions (settled)

The prose below predates these and is superseded by them where they disagree.

**Backend & CLI.** C, emitting C **source text**, compiled by shelling out to `cc`
configured through env vars and/or `neon.toml`. One `.c` for the whole program. Verbs:
`neon compile` → executable; `neon codegen` → emit C; `neon ir [--stage]` → emit IR
text; `neon build` → build a `neon.toml` project to an executable.

**Memory.** Reference counting, **non-atomic, 64-bit** counts (single-threaded v1,
x86-64). **No leaks, and no cycle collector — because immutability makes every value
acyclic.** A cycle needs mutation or a value-level fixpoint to tie the knot; Neon has
neither, so a recursive *type* still only ever holds finite, acyclic *values*, and
refcounting is complete — the last release always runs. Allocation goes through a
swappable runtime shim (`neon_alloc`/`neon_free`), selected by the existing
`--allocator` / `neon.toml` mechanism. `atexit` runs teardown; globals release there.

**Representations.** **Inline aggregates** — records, tuples and containers store
elements *by value*, not boxed (`List[Point]` has `Point`-sized slots). Generic
containers carry a per-element **value-witness** — `{ size, retain, release, drop }`
generated for each monomorphic element type — so they can copy and drop elements whose
shape they cannot see. Unions are inline `{tag, payload}` when they fit; nullable-of-
pointer is a null pointer, no tag. `bool` is one byte. Atoms are tagged by a **64-bit
hash of the name**, globally consistent with no intern table (collisions astronomically
unlikely, checkable within a program). Closures are `{ fn_ptr, env }` with a **null env**
when capture-free, so those allocate nothing.

**Runtime ABI (frozen contract).** Object header `{ u64 rc; u32 flags; void(*drop)(void*) }`
— `drop` per-object so the runtime frees what it holds with no switch; a `flags` bit
marks **immortal** objects. **String literals are immortal `.rodata`**: a static header
whose immortal flag makes retain/release no-ops and needs no copy. Other `str` is a flat
refcounted byte buffer; slices are `{buf, offset, len}` views sharing it. `List` stores
elements inline (size from the witness) plus the witness for bulk retain/drop. `Map` is
an immutable **HAMT**.

**Errors.** A throwing call returns a **tagged result** `{ tag, union{ ok, err } }`; the
caller checks the tag. An uncaught error reaching `main` prints `to_string` to **stderr**
and exits with the chosen panic code.

**Codegen & IR.** Full **monomorphisation** (no generic boxing), instances mangled from
`(fn, concrete types)`, termination trusted to `TooDeep`. **SSA with block arguments**;
loops carry state as header block args. IR values typed by **both** `Repr` and `TyId`.
**Value-level** instructions (`GetField`/`MakeAggregate`); the backend lowers aggregates
to memory. Primitive ops (`i64` add, compare) are **IR instructions**, not native calls.
`const` is **compile-time folded**. The C `main` initialises the runtime and packs
`argc`/`argv` (though `fn main()` takes none in v1).

**Optimisation.** An always-on set (fold, DCE, simplify-CFG, refcount-pair cancellation)
run at **maximum aggression by default** — no `-O` to opt into. Inline wherever able.
Debug info (`#line`) only under a debug flag.

**Textual form.** A **custom, LLVM-ish-but-deliberately-distinct** syntax (not mistakable
for LLVM). **Printer only, never a parser.** Stage selectable via a `neon ir` argument.

**Testing.** Corpus `.stdout` files become end-to-end execution oracles; IR dumps guard
passes.

**Open.** *Effects.* Aggressive-always optimisation needs to know which calls are pure.
Proposed: an inferred, **IR-level** effect analysis — natives tagged `pure`/`io`/`throw`
(via `@native`), Neon functions inferred transitively, never in a signature; DCE keeps
`io`/`throw`, CSE shares only `pure`. Awaiting confirmation.

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

### Inline aggregates and the value-witness

Aggregates are stored **by value**, not boxed. A record lives inline in whatever holds
it; a `List[Point]` has `Point`-sized slots, not a slot of pointers-to-`Point`. Scalars
(`i64`, `f64`, `bool`, atom tag, pointer) are their natural width; a union is inline
`{tag, payload}` when it fits, and nullable-of-pointer collapses to a nullable pointer
with no tag.

The cost of "by value" is that a generic container cannot see the shape of what it
holds, yet still has to copy and drop it. That is what the **value-witness** is for: for
each monomorphic element type, the compiler generates a static `{ size, retain, release,
drop }`, and `neon_list_new` takes it. Only *bulk* runtime operations (grow, clone,
drop-all) go through the witness; element *access* is emitted by codegen, which knows the
type statically and reads the slot directly. The witness is one static table per element
*type*, resolved at compile time — not a per-*value* vtable, which is the thing the
graveyard's erasure produced and this design refuses.

This is more machinery than boxing every aggregate, and it is the deliberate trade: no
per-element allocation, at the cost of generating and threading witnesses. Word-or-box
(box every aggregate, one pointer per slot, no witness) is the simpler fallback if inline
storage proves painful before it proves fast.

## The shared contract: the runtime ABI

This is the part that does not move between backends, and the part worth being precise
about. Every backend links the same `runtime/` (a C library today), so everything that
**crosses the runtime boundary** has a layout `rt.h` dictates and all backends honour.
Everything that does not is a backend's private choice.

**1. The object header.** Every heap object — every aggregate, every `str`, `List`,
`Map`, every closure environment — begins with the same header:

```c
typedef struct { uint64_t rc; uint32_t flags; void (*drop)(void*); } neon_header;
```

`rc` is the (non-atomic, 64-bit) reference count; `flags` carries the **immortal** bit
for `.rodata` objects like string literals; `drop` is how to free *this* object (it
releases the object's own counted fields, then frees). `neon_retain(void*)` and
`neon_release(void*)` operate on the header alone, are no-ops on an immortal object, and
are the only refcount primitives. A backend emits calls to these two symbols; it never
open-codes the count. Putting `drop` *in the header* rather than in a type-indexed table
is what lets the runtime free
an object it holds (a list element, a map value) without a compile-time switch — the
header carries its own destructor.

**2. `str`, `List`, `Map`.** Their layouts live in `rt.h` and the `neon_*` natives read
and write them directly, so they are ABI. `str` is a flat refcounted byte buffer, with
slices as `{buf, offset, len}` views sharing it; a **string literal is an immortal
`.rodata` object** whose header flag makes retain/release no-ops and needs no copy.
`List` stores elements **inline** and carries the element's **value-witness** (`{ size,
retain, release, drop }`), so grow/clone/drop-all work over elements the runtime cannot
see the shape of, while codegen reads and writes slots directly by their known `Repr`.
`Map` is an immutable **HAMT**.

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

A backend-independent pass inserts `Retain`/`Release` so every counted value's count is
balanced: a value is retained when it is stored or captured, released when a binding
leaves scope, and the last release runs the header's `drop`. **This is complete — no
leaks — because the language is immutable.** A reference cycle needs mutation or a
value-level fixpoint to close the loop, and Neon has neither, so every value is a finite
DAG; the count always reaches zero. A recursive *type* (a `mu` list) does not change
this: its *values* are still acyclic. `is_cycle_root` concerns the *representation* of a
recursive type — where its layout needs a pointer at the back-edge to stay finite — not a
runtime cycle. There is no collector to write, now or later. Getting the retain/release
discipline right here, once, keeps every backend from reimplementing it, and immortal
objects (string literals) short-circuit both operations via their header flag.

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
- **Optimisation passes beyond the always-on set**, and `rc == 1` in-place reuse. The
  pass manager is there and runs aggressively; each further pass is an addition, not a
  redesign.
- **The text → IR parser.** The printer only, forever (a decision, not a deferral) — but
  the *first* pass may still want a scratch parser for isolated tests; if so it is a test
  aid, not a supported input.
- **A threading story.** Single-threaded v1 with non-atomic counts; a `shared` bit in the
  header flags is the room left for atomic counts later without an ABI break.

There is **no cycle collector, ever** — immutability makes it unnecessary, not deferred.

The order of work: the representation map (`Repr`, with the no-gaps test) → the SSA IR and
lowering for the scalar/first-order subset, with its printer → `emit_c` for that subset → a
runtime with real `neon_*` bodies → widen to aggregates, closures, `Mappable`, then grow
the optimiser. First a program that prints `42`, then the language, then speed.
