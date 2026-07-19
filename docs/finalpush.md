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
- **`==` is not yet structural on two reprs.** All three are now *diagnostics* rather
  than wrong answers (`is_equatable` in `typecheck/ordered.rs`), so nothing silently
  compares addresses and nothing reaches the C compiler. Each is an opaque pointer today:

  | expression | why | fix |
  | --- | --- | --- |
  | `Map == Map` | opaque container | same length, then every key's value |
  | self-referencing record `==` | `BoxedRec` is a pointer | walk the fields through the pointer |

  `closure == closure` is also rejected, and that one is permanent -- there is no structural
  answer. Nullable equality *was* on this list and is now fixed: `eq_expr` grew a
  `Repr::Nullable` arm that null-tests each side before comparing the payload.

  Fixed on the way, both pinned by `operators/union_vs_union_equality.neon`: two *union*
  operands used to project each side to its first variant and compare those, so
  `(i64 | bool)` compared an i64 against a bool and `1 == true` was true; and
  `union_compare` compared a union against a bare variant with a raw C `==` on the payload,
  which is not valid C once the variant is a record, a tuple or a `str`. Both now compare
  tag first, then the payload through `eq_expr`.

- **A `let` with a union annotation keeps the narrow repr.** Pre-existing, verified
  identical on `7fbd131`, and not an equality bug -- it just surfaces through one:

  ```neon
  let none: P | :none = :none;
  none == P { x: 1 }        // gcc: invalid operands to binary == ('uint64_t' and 'nr0')
  ```

  The annotation says `P | :none`, but the value is lowered at the repr of the variant it
  was initialised with (`Tag`), so any later use expecting the union sees the wrong layout.
  A parameter or a function return of the same type is fine, which is why the corpus test
  goes through functions. The fix belongs with `let`'s lowering, not with comparison.

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

- **Bound failures inside a generic call report twice.** Pre-existing; a protocol bound
  does it too. Arguments are checked once while solving the callee's generics and again
  under the solution, so any diagnostic in an argument position is emitted twice.

- **A list used as a map key leaks.** Pre-existing, and unrelated to comparison — it
  reproduces identically on `7fbd131`, before that work. One list object plus its data
  buffer (72 bytes for `[1, 2]`) is never released:

  ```neon
  let m = map::set(map::new(), [1, 2], "found");   // 72 bytes leaked at exit
  ```

  A `str` key does not leak, and a plain `list::push` does not leak, so it is specific to
  the key path — most likely the map's own drop never releases keys through their witness.
  Worth fixing next to the map ABI; until then `Map[List[T], V]` cannot be a corpus test,
  because the corpus runs under ASan with leak detection on.

  (Its *lookup* now works: hashing a list by address while comparing it structurally would
  have broken the "equal keys hash equal" invariant, so `hash_expr` hashes the length. That
  fixed a second pre-existing bug — `map::contains(m, [1, 2])` used to return false for a
  key that was there — at the cost of a weak hash. See the comment on `hash_expr`.)
- Return overloading **does** work: `dispatch.rs` falls back to the expected type when no
  parameter mentions the subject, so `fn make() -> T` resolves from context.
- Stdlib breadth. Sorting landed 2026-07-19: `list::sort`/`sort_by`/`merge` and
  `std::cmp`'s `max`/`min`/`max_by`/`min_by`, merge sort so that a lying comparator (NaN
  guarantees one) costs a wrong order rather than a read past the end. Still missing: real
  file I/O, and math beyond the operators.
- Stacktrace is still unbuilt, and `opt-release` passes `-fomit-frame-pointer`, which
  fights it.
