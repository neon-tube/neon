# The IR, and the backend it exists for

Status: **built, end to end.** The representation map, the SSA data structure and its
printer, lowering with monomorphisation, the effect analysis, the optimiser, refcount
insertion, the C emitter and the C runtime all exist and are wired behind `neon compile`,
`neon build` and `neon run`; `neon ir --stage` prints any intermediate stage. What is
*deferred* is volume in the optimiser, not architecture — see the last section, which
distinguishes the two honestly.

This document is the design and the reasoning. It is deliberately **not** an API listing:
where a module doc in the code says the same thing more precisely, this file points at it
rather than keeping a second copy that can drift. The code decides; where this file and a
module doc disagree, the module doc is nearer the code and this file is the bug.

## Why an IR at all, and the one lesson from the graveyard

The previous compiler lowered the typed AST straight to output, re-deriving on the way
down every type the checker had already worked out — because the checker threw its
`expr_types` away. That re-derivation could not always succeed, so it fell back to
`Erased`, `Erased` leaked into a boxed `NeonValue` with an invented vtable, the vtable
produced `*_Any` collections with 24-byte slots that `push` read as 8, and a
`list::new()` was an ASan overflow. One discarded hashmap, four subsystems of consequence.

The IR exists so that never happens again. It consumes `TypecheckResult` (`ty` / `call` /
`lambda`, keyed by `ExprId`) and **re-derives nothing**. Every expression already has a
type; every call already has a `Resolution`; every lambda already has an arrow. The IR's
job is to make those facts *explicit and total* — a representation for every type, with no
"unknown" case — and hand them to a backend.

## The shape of the pipeline

```
typed AST + TypecheckResult
  → lower to SSA            (dispatch resolved; generics monomorphised; reprs assigned)
  → optimise                (IR→IR, a pass pipeline over SSA, to a fixpoint)
  → sole-ownership rewrite  (IR→IR; qualifying loop writes become in-place stores)
  → insert refcounts        (IR→IR; retain/release made explicit)
  → backend::c::emit        (one C translation unit for the whole program)
```

`ir::compile` (`compiler/src/ir/mod.rs`) is exactly this, with a `Stage` to stop early.
Monomorphisation is not a separate IR→IR pass: it happens *during* lowering, as call sites
discover which instances exist.

The order is forced, not chosen, and `ir::compile`'s doc says why. The optimiser must run
before refcounting because it rewrites control flow and refcount placement is pinned to a
specific CFG; refcounting must be last because it splits edges and is not idempotent. The
sole-ownership rewrite (`ir::unique`, its module doc is the spec) sits between them for
its own forced reasons: after refcounting, the pass's own retains are indistinguishable
from a real second reference, and refcount placement must see the rewritten writes to
give the in-place native its borrow convention. `Stage::Lowered` and `Stage::Optimised`
carry no retains or releases at all, so only `Stage::Final` is safe to emit — the earlier
stages exist for `neon ir` to print, and `Stage::Optimised` prints the IR *before* the
sole-ownership rewrite.

Every pass above `emit` is **backend-independent**. Only the last step knows what a target
looks like, and that split is the whole portability story.

## The seam is a module boundary, not a trait

An earlier draft of this document specified a `Backend` trait with `declare_type` /
`emit_fn` / `finish`. **It does not exist and was not built.** The seam is
`compiler/src/backend/`: `c::emit(&Program) -> String` is the single entry point, and
`ctype` — the C struct names, the witness names, the mangling scheme — is private to the
module so nothing outside can depend on the shape of the generated source.

That is the same line the trait was for (runtime-ABI knowledge and lowering on one side,
target syntax on the other) drawn with one fewer abstraction, and it is validated by one
real backend rather than asserted by two half ones. A second backend would introduce the
trait then, against two implementations that exist.

A backend receives a program that is already fully lowered — monomorphised, refcounted,
every repr concrete — and is **not permitted to make semantic decisions about it**.
Anything it cannot express is a bug upstream, which is why `ctype::c_type` calls `ice()`
on a repr it cannot pin rather than picking something that compiles. `c_type` is an
exhaustive match with no catch-all arm; the fallbacks inside its arms (an unregistered
aggregate, a back-edge that does not resolve, a boxed record with no wrapper, a surviving
`Repr::Var`) all panic. Every one of those was once a silent `neon_value`, and each
silence cost a bug: an unpinned repr becomes a bare `void*` that C accepts anywhere, so the
value is not erased — it is *typed* as erased while holding unboxed bits, with no header,
witness or tag.

## Representations are abstract

The representation map is `TyId → Repr` (`ir/repr.rs`), and `Repr` is a *descriptor*, never
a C type. The variants, in the code's own order:

```
Repr =
  | I64 | F64 | Bool | Str | Null | Unit
  | Tag                                     // an atom or a union of atoms: a 64-bit hash
  | Record { name: Option<String>, fields: [(String, Repr)] }
  | Tuple([Repr])
  | List(Repr) | Map(Repr, Repr)
  | Runtime { nominal, c_type, args: [Repr] }   // a runtime-owned refcounted object
  | Closure { params: [Repr], throws: Repr, ret: Repr }
  | Union([Repr])                           // two or more distinct variants
  | Nullable(Repr)                          // `T | null`, T pointer-backed: a null pointer
  | Var(String)                             // rigid, abstract until monomorphisation
  | BoxedRec(u32)                           // a record whose cycle closes by value
  | Recursive(TyId)                         // a `mu` back-edge
  | Any                                     // the one erasure boundary
  | Never                                   // uninhabited
```

The C backend turns `Record`/`Tuple`/`Union` into named `struct`s, `I64` into `int64_t`,
`Nullable` into the bare pointer. **The IR never commits to padding, pointer width, or
field offsets** — that is each backend's arithmetic. This is the exact discipline the
graveyard broke when "different C structs leaked into the type system."

The map is **total by construction**: `repr_of` has no unknown case, and the
no-component case comes out as `Never` rather than as an absence a caller has to guess at.
`Var` is abstract but not unknown — it exists only inside a generic body and is gone after
monomorphisation, guarded by `no_type_variable_survives_lowering` in
`compiler/tests/ir_lower.rs` and by `c_type`'s panic. `Any` is erased but not a fallback —
it is reached only when the source wrote `any`. That totality *is* the guarantee erasure
cannot return.

Three variants deserve their reasoning stated here, because it is design and not detail.

**`Runtime { nominal, c_type, args }`** replaced a hardcoded name table — `record_repr`
matched the strings `List`, `Map` and `File` — with a declaration: `@runtime("neon_file")`
on a field-less `record`. That is what lets a runtime-backed type live in an ordinary
stdlib module instead of being a name the compiler recognises. It carries *both* halves
under names that cannot be confused: `nominal` is the Neon name and the type's identity
(the printed form, the mangled key, the boxed type tag), while `c_type` is a spelling of
the pointee that only `ctype::c_type` may read. A single `name` field holding the C symbol
was the earlier design and is precisely why `type_tag_name` had no Neon name to answer with
and every `is` against a runtime-backed type was false. Refcounting, pointer-ness and
substitution are uniform across all such types; only equality, ordering and hashing
genuinely differ, and those live in one name-keyed table in the emitter. `List` and `Map`
remain their own variants because their element reprs feed witness emission.

**`Closure`'s `throws`** is calling convention, not layout: a throwing closure's function
returns the tagged result `Union([ret, throws])`, exactly like a named throwing function.
It is a separate field rather than folded into `ret` because folding changed the type graph
and broke recursive arrow types — the union's struct and its value-witness resolved the
back-edge differently, so the witness emitted `.env` on a `void*`.

**`Recursive` is not a pointer.** It names a type without describing it, and the named type
may well be an inline union: `mu type A = :ok | List[A]` is `{tag, payload}` by value, whose
recursion terminates through the list's pointer. Calling it a pointer had the refcount pass
emit `neon_retain((neon_header*)x)` against a stack union. `Program::recursive` and
`Program::boxed` are the side tables the backend resolves back-edges through.

### Inline aggregates and the value-witness

Aggregates are stored **by value**, not boxed. A record lives inline in whatever holds it;
a `List[Point]` has `Point`-sized slots, not a slot of pointers-to-`Point`.

The cost of "by value" is that a generic container cannot see the shape of what it holds,
yet still has to copy, drop and compare it. That is what the **value-witness** is for: one
static table per element *type*, generated by codegen and passed to `neon_list_new`.

```c
typedef struct neon_witness {
    size_t size;
    void (*retain)(void* elem);
    void (*release)(void* elem);
    bool (*eq)(const void* a, const void* b);
    int (*cmp)(const void* a, const void* b);
} neon_witness;
```

`retain`/`release` are NULL when the element holds nothing counted. `eq` is always present
— equality is total on every type. `cmp` is NULL when the element has no structural order
(a union, which would need an invented rank between its arms); the checker rejects ordering
such a list, so a non-NULL `cmp` is the caller's precondition, not something to test at run
time. Hashing is *not* here: it is layered into `neon_key_witness { value, hash, eq }`,
because only a map key is hashed and folding it in would make every element type carry two
null pointers forever. Any list, by contrast, can be compared, which is why `eq`/`cmp` did
not get the same treatment.

Only *bulk* runtime operations (grow, clone, drop-all, structural `==`/`<`) go through the
witness; element *access* is emitted by codegen, which knows the type statically and reads
the slot directly. The witness is a static table per type resolved at compile time — not a
per-*value* vtable, which is the thing the graveyard's erasure produced and this design
refuses.

This is more machinery than boxing every aggregate, and it is the deliberate trade: no
per-element allocation, at the cost of generating and threading witnesses. Word-or-box (box
every aggregate, one pointer per slot, no witness) is the simpler fallback if inline storage
ever proves painful before it proves fast.

## The shared contract: the runtime ABI

This is the part that does not move between backends. The runtime's public surface is
`runtime/include/libneon_rt.h`, an **umbrella** header that includes twelve per-area
headers under `runtime/include/neon/` (`core`, `lifecycle`, `trap`, `arith`, `string`,
`list`, `map`, `any`, `resource`, `file`, `io`, `math`). Emitted C includes only the
umbrella. Every area header is self-contained and includable on its own — tests and the
CBMC models in `runtime/models/` rely on that, and the umbrella's include order is
deliberately not a dependency ordering. The implementation is eleven translation units in
`runtime/src/`, one per area, plus a private `internal.h`.

The runtime **ships as prebuilt static archives**, not as C source compiled on every build:
`runtime/CMakeLists.txt` builds three variants — `libneon_rt.a` (`-O3`),
`libneon_rt_debug.a` (`-O0 -g -DNEON_DEBUG`, which turns on the runtime's own assertions),
and `libneon_rt_san.a` (`-fsanitize=address,undefined`). `BuildConfig::runtime_variant`
in `cli/src/buildcfg.rs` picks one from the build mode and the requested sanitizers, and it
**refuses rather than substituting**: an uninstrumented archive inside a sanitized program
is a silent hole, because ASan cannot see an overflow that happens in code compiled without
`-fsanitize`. The link's own `-fsanitize` flags are derived from the chosen variant rather
than from the user's request, which makes the archive-versus-flags mismatch unrepresentable.

Three consequences of prebuilding are worth knowing rather than rediscovering: the runtime
is not LTO'd even in release builds (an LTO archive carries bitcode specific to the compiler
that produced it, which is not the `cc` that links a user's program); it is not built
`-march=native` (built once, for an unknown target); and `cflags`/`-D` a user passes reach
their program only, never the runtime.

**1. The object header.** Every heap object — every boxed record, `str` allocation, `List`,
`Map`, `neon_box`, `neon_resource`, closure environment — begins with:

```c
typedef struct neon_header { uint64_t rc; uint32_t flags; void (*drop)(void*); } neon_header;
#define NEON_IMMORTAL 1u
```

`rc` is the non-atomic 64-bit count. `drop` frees *this* object, releasing its own counted
fields first. Putting `drop` in the header rather than in a type-indexed table is what lets
the runtime free an object it holds — a list element, a map value — with no compile-time
switch. `neon_retain`/`neon_release` operate on the header alone, are no-ops on NULL and on
an immortal object, and are the only refcount primitives; a backend emits calls to these two
symbols and never open-codes the count. `neon_alloc(bytes, drop)` allocates
`sizeof(neon_header) + bytes` and initialises the count to 1.

*Undocumented / unbuilt, recorded rather than described:* nothing in the compiler or the
runtime ever **sets** `NEON_IMMORTAL` today. Only `neon_retain`/`neon_release` read it. The
immortal path that literals actually take is a different mechanism (below).

**2. `str`, `List`, `Map`.** Their layouts are ABI because the `neon_*` natives read and
write them directly.

`str` is a **view plus an owner**, not a `{buf, offset, len}` triple:

```c
typedef struct { char* data; size_t len; neon_header* owner; } neon_str;
```

`data`/`len` is the pair libc wants; `owner` is the refcounted allocation it points into,
and slices share it. **A string literal has `owner == NULL`** — static, never freed, with
retain and release no-ops because both return early on NULL. That is the immortality
mechanism in force; the header flag is the unused one.

`List` stores elements inline (`len` used of `cap` slots, each `w->size` bytes) and carries
the witness for bulk retain/drop and for structural `==`/`<`. The header is first, so a
`neon_list*` is also its `neon_header*`.

`Map` is a **flat open-addressing hash table** with a `ctrl` byte per slot
(empty/tombstone/full) and keys and values in parallel arrays sized by their witnesses, and
it is **copy-on-write**: `neon_map_set` mutates in place when the map is uniquely owned and
the load factor allows, and clones first when `rc > 1` or when growth is needed. That is
what makes the immutable interface cheap. A HAMT would exist to avoid the O(n) copy on a
persistent update, but uniqueness already handles the common case and the flat table wins
decisively on cache behaviour; HAMT stays deferred behind the same `map::` surface for the
rare heavily-shared-and-updated workload.

*Flag, not a fix:* `Repr::Map`'s doc comment in `ir/repr.rs` describes it as "an immutable
HAMT". That is stale — `runtime/src/map.c` is the open-addressed table described above.

**3. Closures.** `typedef struct { void* fn; neon_header* env; } neon_closure;` — a
function pointer and a boxed environment holding the captures, with a NULL `env` when
capture-free, so those allocate nothing. A native that takes a closure calls
`fn(env, args...)`. This pair is ABI because the stdlib's higher-order natives invoke it.
An ordinary function used as a closure value gets an **adapter thunk** with the `(env,
args…)` shape; a lifted lambda already has it.

**4. `any`.** One boundary, one representation: `neon_box { header; const neon_witness* w;
uint64_t type_tag; }` followed by the payload bytes inline, with `neon_value` a pointer to
one. `is`/`as` on an erased value compare `type_tag`, which `TypeTable::type_tag` derives
from the `Repr` by FNV-1a over a spelled name — the same hash both sides must go through
(see `Op::IsVariant` below).

**Everything else is backend-internal.** A record's field layout past the header, the
calling convention for Neon-to-Neon calls, how control flow is emitted. The shared contract
is deliberately *small*: the header plus retain/release, the runtime containers, the closure
pair, the box, and the resource protocol. That is the entire price of reusing one C runtime
from any backend.

## The IR itself

**SSA, with basic-block arguments rather than φ-nodes.** Every value is defined once; where
control flow joins, the merged value is a *block parameter*, and each predecessor passes
arguments on the edge. Same idea as φ-nodes, cleaner encoding (Cranelift, MLIR and Swift SIL
all use it): no φ-placement pass, a loop's carried state is just the loop header's
parameters, and a backend maps a block to a label whose parameters are assigned before each
jump. Arguments living on the `Target` rather than at the top of the block is what lets a
pass insert code on one incoming edge without disturbing the other — which is exactly what
refcount insertion needs.

SSA is cheap to build here: Neon is immutable, so the only source of a second definition is
local reassignment (`x = x + 1`), which becomes a fresh value, and the only joins are `if`,
`match` and `loop`. There is no dominance-frontier machinery to write.

Every value carries **both** its `Repr` (for codegen) and its `TyId` (for provenance).
Every block ends in exactly one terminator: `Ret`, `Throw`, `Jump`, `Branch`, `Switch`,
`Unreachable`. Instructions are semantic, not textual, and deliberately close to what a C
emitter prints in one line — there is no addressing and no allocation op. The full `Op` and
`Term` enums are in `ir/ssa.rs` with a comment per variant; the shape is:

- constants (`ConstI64`, `ConstF64` as a bit pattern so `Op` stays `Hash` for CSE,
  `ConstBool`, `ConstStr`, `ConstNull`, `ConstUnit`, `ConstAtom`);
- `Prim(PrimOp, args)` — arithmetic, comparison, logic and bitwise ops, with the operands'
  `Repr` disambiguating `i64` from `f64` rather than separate `IAdd`/`FAdd`;
- calls: `Call` (direct, by mangled name), `Native` (a runtime symbol), `CallClosure`
  (indirect), `MakeClosure`;
- aggregates and projections: `MakeRecord`/`Field`, `MakeTuple`/`Elem`, `MakeList`,
  `Index`, `Cast`;
- result and tag reads: `IsErr`, `UnwrapOk`, `UnwrapErr`, `IsNull`, `IsVariant`;
- `Retain`/`Release`, which **only** the refcount pass adds.

Two of these carry design worth stating.

`Op::IsVariant { value, variant, tested }` gained `tested`. `variant` is the head name,
which is all a *union* discriminant needs, since a union's arms are distinct types. It is
not enough for an **erased** subject: a box's tag is derived from a `Repr`, and `List[i64]`
and `List[str]` write the same head name, so every `is` on an erased generic answered yes
and the `as` a person writes next reinterpreted the payload — an `i64` read as a `neon_str`
header, which is a segfault, not a wrong answer. `tested` is the checker's resolved type, so
both sides of `a is List[str]` go through `TypeTable::type_tag`. This is the collapsing-key
class again; see below.

Dispatch arrives already decided: `Resolution::Direct` becomes a `Call`,
`Resolution::Switch` a `Switch` with a `Call` per arm, `Resolution::Bound` is discharged by
monomorphisation into a `Call` to the concrete instance. There is no vtable and no runtime
method lookup — the checker settled all of it, which is the point of `dispatch.md`.

### An invariant that is undefined, not merely unchecked

`Builder::block_param` records it, and it is still undefined: predecessors pass
arguments in parameter order, but nothing states what relation the argument **reprs** must
satisfy. It is not equality — lowering routinely passes a `str` and a `Null` into a `str?`
join and a bare `i64` into an `i64 | null` one, leaving the widen to the emitter, and a
verifier asserting equality flags thousands of sites across the corpus, all of which run
correctly. The real relation is "assignable", and it exists nowhere: not as a function, not
as a doc. No verifier can be written until someone defines it. Recorded here rather than
claimed as a check that does not exist.

## Errors: the tagged result

A throwing call returns a **tagged result** — `Union([ret, err])`, variant 0 the value and
variant 1 the error — and the caller checks the tag. This is the calling convention of every
throwing function, *including one reached through a closure*: `Repr::Closure` carries its
`throws`, a throwing lambda's lifted function and a named throwing function's adapter thunk
both return the tagged result, and a closure call unwraps it exactly like a direct call.

`Func::result_repr` builds that union **raw**, deliberately bypassing the type system's
`combine`/`normalize_union`. Those normalise — dedupe variants, reorder into canonical rank,
collapse `T | null` to a nullable pointer — and every one of those is wrong here, because
`IsErr`/`UnwrapOk`/`UnwrapErr` address the arms *by index*. `fn f() throws str -> str` must
stay a two-arm `Union([Str, Str])`, and `fn f() throws E -> null` must not become a nullable
`E`. It is a positional pair that happens to reuse the union layout and accessors.
Relatedly, `Builder::set_throws` asserts the error repr is never `Never`: a `throws never`
clause means the function does not throw, and tagging its result would make every caller
read a value that is not there.

In the emitter both `ret` and `throw` become C `return` statements differing only in which
tag they inject into. A `throw` in a function that declares no `throws` has no union to
return through, so it can only panic — that is an error escaping `main`, not an unhandled
case, and it becomes `neon_panic`, which prints `neon: uncaught error: …` to **stderr**,
flushes stdout first so the program's output up to the fault survives, and `_exit`s with
101. `neon_trap` — a bad index, a division by zero — uses the same code and the same
stdout-first discipline, and `abort()`s instead under `NEON_DEBUG` so a debugger catches
SIGABRT at the fault.

## Monomorphisation

Generics are specialised, not boxed, during lowering. `Resolution::Bound { param, protocol }`
is resolved by substituting the instance's concrete type and emitting a direct call to *its*
impl; `mangle_instance` names the instance from the base name and `repr_key` of each
concrete argument, and `lower_module`'s `lowered` set dedups on that name. Recursion through
a generic is already rejected by the checker's `TooDeep`, so this terminates; a generic never
instantiated is never emitted. This is where the "monomorphic escape hatch" the stdlib notes
describe stops being an escape hatch: everything is monomorphic here, uniformly.

### The collapsing key, and the injectivity obligation

`repr_key` is a mangled *name*, which makes it an **identity**, which makes it subject to an
obligation that is easy to miss because nothing about the function looks dangerous: it is
total, and every arm is a correct *description*. The defect is that its codomain was smaller
than its domain.

`Repr::Union(_) => "union"` and `Repr::Closure { .. } => "fn"` were constants. So every
instantiation of a generic at any two union types mangled to one name, the `lowered` set
dropped the second body, and one emitted instance — typed at whichever substitution was
popped first — served call sites that had agreed with the compiler on a different layout.
It is not reliably caught downstream: `fn ident[T](x: T)` at `i64 | str` and at `bool | f64`
produced two C structs and the C compiler rejected the mismatch, but the same collision at
`i64 | bool` and `i64 | f64` produced one struct and **compiled silently**, correct only by
coincidence of layout. `type_tag_name` had the same defect three separate times.

The rule this leaves behind, for anyone modifying `repr_key`, `type_tag_name`, `field_name`
or any sibling: **a total function from a structured type to a string or integer used as a
name, key or tag carries an injectivity obligation, and the obligation belongs in its doc
comment backed by an assertion, not prose.** Both functions now say so. The separator in
`repr_key` is `_`, which identifiers may also contain, so its injectivity is weaker than
`ctype::key`'s bracketed scheme — a new arm should bracket rather than add another `_`.

The class is not closed. `repr_from_typespec` still collapses — a turbofish `Box[i64]` and
`Box[str]` produce the same repr, so `ident[Box[i64]]` and `ident[Box[str]]` are one
instance, currently caught only by gcc's nominal struct typing, which is the same "correct by
coincidence" footing the union collision had. That reproducer and the remaining known
instances belong to the wider class of lossy projections used as identities; the fix is not
a cleverer projection but to stop deriving a third spelling of a type from syntax at all.
Three of that class have since been found and fixed — an interpolation hole's resolution
overwriting the hole expression's own, `derive`'s generated impls sharing the record's span,
and `collect_impl_bodies` keying bodies by `protocol$head$method`.

## Refcount insertion

A backend-independent pass makes every counted value's count balanced. It is
**last-use-driven (Perceus-style)**, not naive-insert-then-elide: where a value is *consumed*
at its last use the pass **moves** it — hands over the existing reference — rather than
emitting a retain/release pair.

The full placement rules are in the module doc of `compiler/src/ir/refcount.rs`, which is
the authority and is more precise than any restatement here would be. The design in one
paragraph: every counted value is an **owner** (holds one reference from production: call and
native results, aggregates, `Index` reads, block parameters) or a **view** (holds nothing:
`Field`, `Elem`, `Cast`, `UnwrapOk`, `UnwrapErr` alias what their operand owns). Liveness is
computed **over roots** — a use of a view is a use of the owner at the bottom of its
projection chain, and views never appear in a live set. That single collapse is what lets one
analysis place every retain and release; the previous design tracked views and roots
separately and needed a base-extension step that made "release the root once the last view
dies" unreachable exactly when a view was consumed at a terminator. Terminator bookkeeping
sits **on the edge**, with a fresh block spliced onto a `branch`/`switch` edge that needs
code, so nothing fires on a path not taken. There is no separate block-boundary rule; the
edge rule *is* the boundary rule, derived from the same liveness as everything else.

Three exceptions carry their own reasoning. A lambda's environment parameter is **borrowed** —
the closure owns it and may be called again — and `CallClosure` likewise borrows its callee,
because calling a closure reads it rather than destroying it. The native
`neon_list_set_inplace` (emitted only by `ir::unique`, never by lowering) also **borrows**
its arguments: it mutates the buffer but takes no reference and releases nothing, so the
chain's one owner stays live across it — a retain per write is exactly the traffic that
rewrite exists to remove, and would leak besides. And the one `Cast` that is not
a projection is erasure into `any`: it *allocates* a box and the operand's reference moves
into it, so it is an owner and a consuming use. Treating it as a view leaked the box and
everything it transitively owned, invisibly for flat records and visibly for recursive ones.

**This is complete, and there is no cycle collector — now or ever.** A reference cycle needs
mutation or a value-level fixpoint to tie the knot, and Neon has neither, so every value is a
finite DAG and the last release always runs. A recursive *type* does not change this: its
*values* are still acyclic, and `Repr::Recursive`/`Repr::BoxedRec` concern where a *layout*
needs an indirection to stay finite, not a runtime cycle. Getting this discipline right once,
here, keeps every backend from reimplementing it.

Adjacent but built elsewhere: the **sole-ownership rewrite** (`ir::unique`) turns a
qualifying loop's `list::set` calls into in-place stores — the FBIP-shaped win, taken not
by tracking `rc == 1` per write but by *establishing* it once on the loop's entry edge and
proving the function creates no second reference inside the loop. It runs before this
pass, and this pass's only involvement is the borrow exception above. Still not built:
**escape analysis** (stack-allocate a value that never escapes, with no header and no
count at all), described in the deferred section. `neon_map_set` implements the `rc > 1`
check on the runtime side, which is the same idea applied by hand in one place.

## Optimisation

The optimiser (`ir/opt.rs`) runs each function to its own fixpoint over four passes, with
purity computed once over the unoptimised program:

- **constant folding** on `i64` and `bool` primitives, leaving overflow and division by zero
  unfolded for the runtime to handle. `decisions.md` pins the arithmetic — a folded
  expression and the same expression evaluated at runtime agree — so folding is a
  correctness-preserving rewrite, not a guess;
- **dead-code elimination**, guided by the effect analysis so an effectful instruction is
  never dropped;
- **simplify-CFG**: fold a constant branch to a jump, thread empty forwarding blocks, merge a
  block into its sole predecessor;
- **unreachable-block removal**, renumbering blocks contiguously so ids stay indices.

Per-function-to-fixpoint rather than per-pass-over-the-program because the passes feed one
another: folding a branch condition orphans blocks, which makes a block single-predecessor,
which exposes more constants. Reusing one purity map across the whole run is sound because
these passes only remove work — a function that was pure cannot become effectful.

**Not built, despite earlier drafts of this document describing them as design:** inlining,
GVN/CSE, escape analysis, refcount-pair cancellation. They are listed under deferred. (The
loop-write case of FBIP reuse is built, as the sole-ownership rewrite between this pass
and refcounting — see the pipeline section.) The claim in `decisions.md` that "after monomorphisation and inlining a primitive
compare is a single instruction" is currently carried by the C compiler's inliner, not the
IR's.

Refcount insertion runs *after* the value-level optimiser on purpose: dead code is gone
before its retains and releases would be written, so they never need to be optimised away.

## Effects, for the optimiser only

DCE must know what is safe to drop, which means knowing which calls have effects. This is
**not** purity in the type system — keeping purity out of signatures stands. It is an
invisible IR-level analysis, and it is **pessimistic by design**: effectful unless cheaply
proven pure. Being wrong in the safe direction costs a missed optimisation; the reverse
miscompiles. Two states suffice, since immutability means there is no
read/write-of-mutable-memory category to model.

A function is pure iff every instruction is pure and every callee is pure, computed as a
monotonic fixpoint over the call graph. The fixpoint starts **optimistic** and only ever
removes purity, which is what lets recursion terminate *and* be classified usefully — a
recursive function stays pure while its own body is examined instead of demoting itself on
first sight of its own call. A callee absent from the map is read as effectful, so a call to
anything outside the lowered program stays un-eliminable.

What is effectful, per `op_is_effectful`:

- **a native, unless its declaration carried `@pure`.** A native's body is opaque, so this is
  not an analysis — it reports what the declaration *claimed*. The polarity is the
  load-bearing part: silence means effectful, so forgetting `@pure` costs an optimisation
  while a wrong `@pure` licenses DCE to delete a call that mattered. The rule this replaced
  inferred purity from the symbol's *spelling* and deleted a resource construction along with
  the cleanup that construction existed to schedule;
- **an indirect call** (`CallClosure`), which cannot be seen through;
- **`Index`**, because it traps — out of bounds for a list, absent key for a map — and ending
  the program is as observable as an effect gets. Deleting one because nobody reads the
  element deletes the check: `xs[10]` as a statement ran clean past the end of a
  three-element list;
- **`i64` arithmetic** (`Add`/`Sub`/`Mul`/`Div`/`Rem`/`Neg`, decided by operand repr).
  Precisely: only `Div` and `Rem` actually trap (zero divisor, and `INT64_MIN / -1`); the
  rest **wrap**, which is what `-fwrapv` buys. They are listed anyway as a conservative
  choice, costing missed deletions of dead wrapping arithmetic and nothing else. `f64`
  follows IEEE, produces an infinity or a NaN rather than trapping, and stays pure — a
  distinction worth the operand lookup, since calling all arithmetic effectful leaves DCE
  nothing to remove while calling it all pure deleted `1 / 0`.

Everything else — allocation, projection, comparison, `Retain`/`Release` — is a pure function
of its operands.

### An observable the abstraction could not express

**Non-termination is an effect**, and this is the second design-relevant bug class this layer
produced. If ending the program counts as observable, a program that never ends is observable
by exactly the same argument. Without it, DCE deleted a call to a pure function that loops
forever and a program that should have hung printed its next line and exited 0.

The reason no amount of care in `op_is_effectful` could have caught it is structural: that
function is a **per-instruction** two-state verdict, and non-termination is a property of a
function's *shape*, not of any instruction in it. Every individual instruction in an infinite
loop is pure and the verdict on each is correct. The fix therefore had to be a new analysis
at the right granularity rather than a new arm: `may_diverge` seeds the fixpoint, and a
function might not terminate if its own CFG has a **back edge** or if it sits on a **cycle in
the call graph**. The call-graph half is exactly what the fixpoint's optimism cannot catch on
its own — `fn a() { b() }` and `fn b() { a() }` reach a fixpoint at "both pure" while neither
ever returns.

The tell, for the next one: when a bug's fix does not fit as another arm of the function that
decided wrong, the abstraction cannot express the observable, and the answer is a different
analysis rather than a more careful one.

## Textual form

A canonical SSA dump, printed by `neon ir [--stage lowered|opt|final]`, defaulting to
`final`. **Printer only, never a parser** — a decision, not a deferral. The syntax is
LLVM-ish but deliberately its own so it is never mistaken for LLVM. It exists to debug a
lowering or a pass, and to be diffed against goldens. It shows code, not the type
environment: the `recursive`/`boxed` side tables are not printed, and a function's signature
shows its *declared* return rather than the tagged result a throwing function actually
returns.

## What is deferred, on purpose

The *substrate* — SSA, a pass pipeline, the effect analysis, the textual form — was built
first, because retrofitting SSA or effects later is a rewrite. What is deferred is volume:

- **Optimisation passes beyond the four always-on ones**: inlining, GVN/CSE, refcount-pair
  cancellation. Each is an addition to `optimize`'s loop, not a redesign.
- **Escape analysis**, and the rest of in-place reuse. The sole-ownership rewrite
  (`ir::unique`) now takes the loop-write case of FBIP reuse — `list::set` in a qualifying
  loop is an O(1) in-place store, worth 35% on the brainfuck benchmark — but only for
  uncounted elements and only round loops; general reuse (counted elements with a
  displaced-value release, straight-line writes, maps) is open. Escape analysis needs a
  notion of a stack-allocated value the emitter does not have yet.
- **A second backend.** The seam is a module boundary today (see above) and becomes a trait
  when there are two implementations to abstract over. Building LLVM or Cranelift before C
  ran a program end to end would have been speculative.
- **The text → IR parser.** Printer only, forever. A scratch parser for isolated pass tests
  would be a test aid, not a supported input.
- **A threading story.** Single-threaded v1 with non-atomic counts; a `shared` bit in the
  header's `flags` is the room left for atomic counts later without an ABI break.
- **Program-level teardown.** The C entry point is `int main(int argc, char** argv) {
  neon_rt_init(argc, argv); nl_main(); return 0; }`. There are no globals to release and
  no `atexit` teardown; `neon_rt_init` stashes argv for `std::os` and sets stream modes,
  nothing more. `fn main()` still takes no parameters — the command line is `os::args()`,
  read on demand. Earlier drafts of this document described teardown as built. It is not.

There is **no cycle collector, ever** — immutability makes it unnecessary, not deferred.
