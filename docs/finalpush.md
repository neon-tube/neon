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
- **`==` is not yet structural on four reprs.** The 2026-07-19 work made equality
  structural for records, tuples, lists and unions; these four were *already* wrong before
  it and are unchanged by it — each verified against `7fbd131`, which behaves identically.
  The decision doc claims equality is total on every type, so this is the gap between the
  claim and the code, and the checker offers no diagnostic for any of them because the
  `==` arm never consults a shape check at all:

  | expression | today | should be |
  | --- | --- | --- |
  | `Map == Map` | `false` for equal contents (pointer compare) | structural |
  | `(List[i64] \| null) == same` | `false` for equal contents (pointer compare) | structural |
  | `closure == closure` | **gcc error**, `invalid operands to binary ==` | a diagnostic |
  | `(P \| :none) == P` | **gcc error** on a record/tuple payload | structural |

  The last is `union_compare` (`c.rs:955`) emitting a raw C `==` on the projected payload;
  it needs to call `eq_expr` on the variant repr instead. `Nullable` also needs unwrapping
  in `scalar_repr`, or its own arm in the aggregate routing. Closure equality has no
  structural answer and should be refused by the checker, which means `==` needs an
  `is_equatable` gate the way `<` now has `is_ordered`.

  A related trap, since two operands that are *both* unions never reach `union_compare`:
  `prim_operand` projects the first non-null variant of each and compares those, so
  `(i64 | bool)` operands compare an `i64` against a `bool` — `1 == true` is `true`.

- **A generic cannot call a generic.** Pre-existing and unrelated to comparison —
  verified identical on `7fbd131`. Passing an argument whose type is the caller's own
  rigid type variable solves *nothing*: `solve_generics` returns an empty substitution, so
  the callee's type parameters are never bound and lowering emits `neon_value` where the
  concrete type belongs.

  ```neon
  fn id[T](x: T) -> T { x }
  fn relay[T](x: T) -> T { id(x) }        // gcc: assignment to 'int64_t' from 'neon_value'
  ```

  This is worth fixing early: it blocks any layered generic code, and it is also why a
  `where T: Ord` marker bound cannot yet be *propagated*. The bound is discharged from the
  call site's substitution, and when that substitution is empty there is nothing to check —
  so `fn relay[T](a: T, b: T) { max(a, b) }` with no bound of its own is accepted, and
  `relay(map, map)` reaches the backend. Direct calls are checked correctly; it is only the
  generic-to-generic hop that escapes, and it escapes because the hop is broken anyway.

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
- Stdlib breadth: 48 functions across 5 modules. No sort, no real file I/O, no math.
  `sort` is now unblocked — every type has a structural order — and `sort_by(xs, key)` is
  the documented escape hatch for a type whose meaningful order is not its structural one.
  Pick a sort that stays memory-safe under an inconsistent comparator: NaN makes the
  comparison lie, and an introsort-style implementation can read out of bounds when it does.
- Stacktrace is still unbuilt, and `opt-release` passes `-fomit-frame-pointer`, which
  fights it.
