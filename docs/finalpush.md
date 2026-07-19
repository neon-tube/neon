# Final push: refcount correctness

State as of 2026-07-19. **698/698 suite, 153/153 backend under ASan+UBSan, zero
use-after-free, zero leaks, zero UB.** The three-leak bug below is fixed; this section
records how, because the shape of the fix is the lesson.

## How to measure

```
cargo nextest run -p neon-compiler --test backend_run
```

Sanitizers are **on by default** — every corpus program compiles with
`-fsanitize=address,undefined` and runs with leak detection, and a sanitizer report
fails the test ahead of the output diff. This is not optional decoration. The corpus
ran green over a genuine stack-buffer-overflow (a value handed to `neon_map_set`
uncoerced, so the witness memcpy'd 32 bytes out of an 8-byte double — the graveyard's
"24-byte slots read as 8", again). A passing suite without sanitizers proves the
answers look right, nothing more.

To look at one program:

```
neon compile tests/lang/<prog>.neon -o /tmp/x --sanitize address --sanitize undefined
ASAN_OPTIONS=detect_leaks=1 /tmp/x
```

`neon ir <file>` is the other half. Every bug in this effort was found by reading the IR
or the generated C, not by reasoning about the pass.

## Resolution (2026-07-19): the redesign, not a sixth patch

The hypothesis held. Rule 8 (release boundary-dying values at the top of every
successor), the view-consumed-at-terminator retain, and the release-the-root-behind-the-
views rule were three mechanisms racing over one question, and the five recorded patches
only shifted work between them. The rebuild replaced all three with one placement
mechanism derived from one liveness answer. Two ideas carried it:

1. **Liveness over roots.** Every use of a view counts as a use of the owner at the
   bottom of its projection chain; views never appear in a live set. The old design
   tracked both and patched the gap with `with_bases` — which marked a root live
   wherever its views were, so "release the root once its last view dies" could never
   fire for a view consumed at a terminator. That guard's unfireability was recorded in
   attempt 3 and misread as a dead end instead of the diagnosis.
2. **All terminator bookkeeping on the edge.** Per CFG edge: retain the views passed as
   block arguments, then release every owner live at the terminator that is neither
   moved along that edge nor live into the successor. A `jump` edge is the end of its
   block; a `branch`/`switch` edge needing code gets a fresh block on the edge (so
   nothing fires on the path not taken); `ret`/`throw` place the same code before the
   terminator.

`docs/design/ir.md`'s refcount section now states this placement spec in full;
`refcount.rs`'s module doc is the same spec next to the code. The sixth attempt in the
old table — edge-precise releases, sound but 140/153 — was this design missing its other
half: it keyed on `live_out` without root-collapsing, and only handled single-successor
blocks.

Reading the generated C also found a leak the IR-level diagnosis had missed, and it, not
the conditional-edge retain, was the leak actually firing on `throw_from_catch`'s run
path: `Display$X$to_string(r)` returned a retained view of its parameter and never
released the parameter itself — a view consumed at `ret` kept its base alive (via
`with_bases`) and then nobody dropped it. Every function whose parameter's last use is a
projection leaked one reference per call. The root-liveness model fixes this as a side
effect: the base dies before the `ret`, after the view's retain.

The wrong-path retain was also latently worse than a leak: `retain %4` above
`branch %3, block2(%4), block3` read the *err* variant's bytes out of a union whose tag
said *ok* and retained whatever pointer those bytes spelled. It stayed benign only
because compound-literal zero-fill left the owner slot NULL. Edge placement removes the
read entirely on the untaken path.

Regression coverage: `refcount/tests.rs` pins both shapes
(`a_returned_view_retains_itself_and_releases_its_base`,
`a_view_passed_on_a_conditional_edge_is_retained_on_that_edge_only`), and the three
corpus programs run leak-free under ASan.

## Also outstanding, unrelated

- ~~**`==` ignores `Eq` impls.**~~ **Settled 2026-07-19: the doc moved.** Comparison is
  structural on every type and there is no `Eq`/`Ord` protocol; ordering is total *within*
  a type, and ordering a union is a diagnostic. See "Comparison is structural" in
  `docs/decisions.md` for the reasoning, including why the Elixir-style cross-type total
  order was declined and what NaN costs. Four bugs were hiding in the doc/code gap:
  `record == record`, `record < record` and `tuple == tuple` emitted C comparing two
  structs (not valid C — the *C compiler* failed, not Neon), and `list == list` compiled
  and returned pointer equality, so `[1,2,3] == [1,2,3]` was false.
- ~~**`==` is not structural on every type.**~~ **Closed 2026-07-19.** Maps compare by
  content (`neon_map_eq`: same length, then each key looked up in the other -- an
  open-addressed table has no canonical slot order), and a self-referencing record walks
  through its pointer via a generated function per boxed type, forward-declared so mutually
  recursive records can call each other. That walk needs no visited set: records are
  immutable, since field and index assignment are *parse* errors, so a value cannot point
  at itself and the graph is always a DAG.

  A **closure** remains rejected, permanently -- there is no structural answer for two
  functions. `operators/unequatable_is_rejected.neon` pins it.

- ~~**A `let` with a union annotation keeps the narrow repr.**~~ **Fixed 2026-07-19.** The
  checker always bound the annotation's type; lowering saw only the initialiser and used
  *its* repr, so `let none: P | :none = :none` became a bare tag and any later use
  expecting the union read the wrong layout. The declared type is now recorded against the
  initialiser and the binding widens to it. Pinned by `types/let_annotation_widens.neon`.

- ~~**A generic cannot call a generic.**~~ **Fixed 2026-07-19.** `generic::infer`
  short-circuited on `template == concrete`, which is exactly the case when a generic
  passes its own rigid `T` along: nothing was bound, the callee was left unmonomorphised,
  and lowering laid its instance out generically -- `id(x)` assigned a `void*` to an
  `int64_t`, and a `List[T]` round-trip read every element at the wrong width and
  corrupted the heap. The early-out now fires only when the template mentions none of the
  variables being solved, and a variable bound to *itself* no longer out-ranks a concrete
  answer found later (nested `map::set` calls raced exactly that way, `K := K` blocking
  `K := str`). Pinned by `functions/generic_calls_generic.neon`.

  This also unblocked marker propagation: `where T: Ord` is discharged from the call
  site's substitution, so with one there, `fn relay[T](a: T, b: T) { max(a, b) }` is now
  correctly rejected unless it declares the bound
  (`operators/marker_ord_propagates.neon`).

- ~~**A lambda inside a generic is not monomorphised.**~~ **Fixed 2026-07-19.** A lambda
  was lifted once, keyed on its source id alone, so every instantiation shared one erased
  function whose parameters were `neon_value` -- `(a: T, b: T) => a < b` compared two
  *pointers*, and each caller cast the closure to its own concrete signature on top. It is
  now keyed on (source id, enclosing substitution) and lowered under it, like any other
  instance. The failure was silent: a stable sort whose comparator always answers `:eq` is
  the identity, so `sort` on a `List[str]` returned it untouched with the sanitizers clean.
  `list::sort` is back to delegating to `sort_by` through exactly such a lambda, and
  `functions/lambda_in_generic.neon` checks values rather than just running.

- ~~**Bound failures inside a generic call report twice.**~~ **Fixed 2026-07-19.** A
  generic call checks each argument twice -- once while solving the callee's type
  parameters, then again under the solution, which is what lets an expected type reach a
  lambda argument -- so anything wrong inside an argument was reported twice. The finished
  diagnostic list is deduplicated by (span, kind), which is cheaper than threading a
  "probing, stay quiet" mode through every expression form.

- ~~**Control flow escaped a lambda.**~~ **Fixed 2026-07-19.** A lambda is lifted into its
  own function, but the checker did not reset the function-scoped context when descending
  into one -- so `return`, `throw` and `break` were all checked against the *enclosing*
  function. Three escapes, one cause, all pre-existing (verified on `7fbd131`):

  - **`return` was unsound.** Typed against the enclosing function's return type while
    lowering returned from the lambda, so `apply((x: i64) => { return "not an i64"; 7 })`
    compiled clean and read a `neon_str` as an `int64_t`. Memory-unsafe in general.
  - **`throw` was absorbed by an enclosing `try`**, so the checker called an error handled
    that escaped uncaught: the program printed `neon: uncaught error` and exited 101 from a
    `try`/`catch` the checker had accepted.
  - **`break` resolved to an enclosing loop** and reached `unreachable` at run time.

  `lambda()` now saves and resets `ret`, `throw_sinks` and `loop_breaks` alongside the
  `throws` it already handled, and a lambda's return type is the union of its tail and its
  `return`s. Separately, `break`/`continue` with *no* enclosing loop was silently accepted
  anywhere -- `fn main() { break; }` compiled -- and is now a diagnostic.

- **Throwing closures do not exist, and the arrow type that describes one is uninhabited.**
  `Repr::Closure` records parameters and result but **not `throws`**, so the tagged result a
  throwing function returns would be read as its declared type. Both ways of producing such
  a value are therefore closed: a lambda cannot throw (`lambda()` pins its sink to `never`),
  and a named throwing function used as a value is a diagnostic. So
  `(i64) throws E -> i64` parses, and nothing can inhabit it —
  `types/arrow_type_throws.neon` passes only because it declares such types without
  building one.

  This blocks `Resource[T, E]` in `docs/design/resources.md`: cleanup is specified as
  `(T) throws E -> ()`, which cannot currently be written. Non-throwing cleanup works today,
  so `Resource[T, never]` is buildable and `resource::new(fd, close)` compiles.

  **An attempt was made 2026-07-19 and reverted.** Recorded so it is not re-walked:

  - The approach: fold the throws into the closure's return, `ret = Union([ret, throws])`,
    matching `Func::result_repr`. No new field, and `CallClosure`'s C cast is built from the
    result value's repr, which lowering controls — so it needs only `wrap_throwing` after
    the call (reusing what direct calls already do) and a `set_throws` in
    `lower_lambda_job`, which was simply never called.
  - It works for the flat case and **breaks recursive arrow types**:
    `mu type F = null | (i64) throws :err -> F`. The tagged-result union's C struct gets
    `neon_value` for the boxed `F`, while its *witness* is generated from a repr whose
    variant is still a `Closure`, so the witness emits `.env` on a `void*`. The recursive
    back-edge resolves differently in the two paths.
  - So the next attempt should probably give `Repr::Closure` a real `throws` field rather
    than folding, leaving the type graph unchanged and combining the two only where the C
    signature is built. That keeps the recursion structure identical to today's.
  - `emit_thunks` was fixed on the way and kept: it built the adapter from the declared
    return type rather than the tagged result. Correct on its own, and needed by any fix.

  `docs/tasks/throwing-closures.md` is a self-contained brief for picking this up: every
  call site, the failed approach with its exact symptom, and the cases that must work.

- **A protocol method call inside an interpolation hole miscompiles.** Accepted by the
  checker, rejected by the C compiler -- so it is a miscompile, and the shape is one
  everybody writes:

  ```neon
  let e = KeyError { message: "x" };
  io::println("failed: #{message(e)}");   // 'neon_unit' has no member named 'fn'
  ```

  Interpolation desugars to `to_string(hole)`, so this is nested dispatch, and the inner
  call's resolution appears to be lost -- codegen emits a closure call on a unit. A plain
  function call in a hole is fine; it is specifically a protocol method. `try`/`catch` is
  not involved, despite where it was first noticed.

- **A named throwing function used as a value miscompiles.** `emit_thunks` builds the
  closure-ABI adapter from the function's *declared* return type, but a throwing function
  returns the tagged result, so the thunk returns `nu1` from a slot typed `int64_t`:

  ```neon
  fn boom(n: i64) throws IndexError -> i64 { ... }
  fn run(f: (i64) throws IndexError -> i64) throws IndexError -> i64 { try f(1) }
  run(boom)                                // gcc: incompatible types when returning 'nu1'
  ```

  A lambda in the same position is fine, which is why this went unnoticed. It blocks the
  point-free `resource::new(fd, close)` shape in `docs/design/resources.md`.

- **An error in a stdlib module is reported against the *user's* file, with a nonsense
  span.** A broken stdlib file poisons every compile -- expected -- but the diagnostic
  points at the closing brace of whatever the user was compiling, naming a call that is not
  there. Cost real time during the `std::fs` work; would badly confuse anyone hitting it.

- ~~**A list used as a map key leaks.**~~ **Fixed 2026-07-19.** The map natives never had
  a stated ownership rule for their *key*, only for the map. `contains` released the map
  and dropped the key on the floor, and `set` discharged it only on the path where it
  stored the key -- so overwriting an existing key leaked too. Both now consume the key,
  like any other native. `at` and `find` deliberately do not: they are reached through
  `Op::Index`, whose operands the refcount pass releases itself, and consuming there
  double-freed (caught by ASan while writing the test). The header now states the rule per
  function. Pinned by `collections/map_list_key.neon`, which uses a `List` key because a
  scalar or literal key owns nothing and hides the bug.

- Return overloading **does** work: `dispatch.rs` falls back to the expected type when no
  parameter mentions the subject, so `fn make() -> T` resolves from context.
- Stdlib breadth. Sorting landed 2026-07-19: `list::sort`/`sort_by`/`merge` and
  `std::cmp`'s `max`/`min`/`max_by`/`min_by`, merge sort so that a lying comparator (NaN
  guarantees one) costs a wrong order rather than a read past the end. Still missing: real
  file I/O, and math beyond the operators.
- Stacktrace is still unbuilt: no capture, no slot in the error struct. The
  frame-pointer conflict that used to sit alongside it is settled -- `--stacktrace` (or
  `stacktrace = true` in `neon.toml`) is mutually exclusive with `opt-release`'s
  `-fomit-frame-pointer` and wins, passing `-fno-omit-frame-pointer` explicitly since `-O3`
  trims frame pointers by itself. See `docs/design/errors.md`.
