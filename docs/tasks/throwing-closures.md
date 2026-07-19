# Task: make throwing closures representable

Repo: `~/projects/personal/neon2` (not `neon` — that's an older sibling checkout, ignore it).
Read `HANDOFF.md` and `docs/finalpush.md` first. Everything below is verified as of
2026-07-19, on a tree at 716 tests / 169 backend corpus, all green.

## The bug in one sentence

`Repr::Closure` records a closure's parameters and result but **not its `throws`**, so a
throwing function cannot be represented as a value — and the arrow type that describes one,
`(i64) throws E -> i64`, is currently **uninhabited**.

## Why it matters

Both ways of producing such a value are closed off, deliberately, because neither works:

- A **lambda** cannot throw. `check.rs:722` pins its sink to `Throws::Declared(never)`.
- A **named throwing function** used as a value is a diagnostic (`ThrowingFnAsValue`,
  `check.rs:1124`). Without it, the code compiled and returned garbage.

`types/arrow_type_throws.neon` passes only because it *declares* such types and never builds
one.

Downstream, this blocks `docs/design/resources.md`, whose cleanup is specified as
`(T) throws E -> ()`. Only `Resource[T, never]` is constructible today, so `release` cannot
report failure and `fs::close` cannot surface a close error — which is the entire
justification for that design's explicit path.

## Background: the calling convention

A throwing function does not return its declared type. It returns a **tagged result**:

    Func::result_repr()  =  Union([ret, throws])          // ir/ssa.rs:56

`fn_ret_type` (`backend/c.rs:130`) already knows this and is the function to imitate. A
direct call unwraps it via `wrap_throwing` (`ir/lower.rs:1514`), which retypes the result to
that union and emits the `IsErr` / branch / `UnwrapOk` sequence.

None of that reaches closures, because the closure's repr never says it throws.

## What was tried, and exactly how it failed

**Do not re-walk this.** Attempted 2026-07-19, reverted.

The approach: fold the throws into the closure's return rather than adding a field —
at `ir/repr.rs:457`, where an `ArrowAtom` becomes a `Repr::Closure`, emit
`ret = Union([ret, throws])` when `throws` is not `Never`. `ArrowAtom` already carries
`throws` (`typecheck/types.rs:112`); repr.rs was simply dropping it.

That needs no new field, and `CallClosure`'s C cast is built from the *result value's* repr
(`backend/c.rs:921`), which lowering controls — so it required only:

- `wrap_throwing` after each `Op::CallClosure` (`ir/lower.rs:1195` and `:1258`), reusing the
  direct-call machinery, with the throws read from the callee's arrow *type* (the repr can no
  longer be asked, since it was folded);
- a `set_throws` call in `lower_lambda_job` (`ir/lower.rs:559`), which was simply never made,
  so a lifted lambda never returned a tagged result.

**It works for the flat case and breaks recursive arrow types.** The failing test is
`tests/lang/types/mu_type_through_arrow_that_throws.neon`:

    mu type F = null | (i64) throws :err -> F

Symptom — the generated C does not compile:

    static void nw4_retain(void* p) { nu0* e = (nu0*)p;
        ((*e).tag == 0 ? ((void)(neon_retain((*e).u._0.env))) : ((void)0)); }
    error: request for member 'env' in something not a structure or union

Diagnosis: `nu0` is the tagged-result union. Its emitted **struct** has variant `_0` as
`neon_value` (the recursive `F` is boxed), while its **value-witness** is generated from a
repr whose variant `_0` is still a `Closure` — so the witness emits `.env` on a `void*`. The
recursive back-edge resolves differently along the two paths.

## Suggested approach

Give `Repr::Closure` a real `throws` field instead of folding:

    Closure { params: Vec<Repr>, throws: Box<Repr>, ret: Box<Repr> }   // ir/repr.rs:54

This leaves the type graph identical to today's — which is what the recursive case is
sensitive to — and combines the two only where the C signature is actually built. There are
**20 `Repr::Closure` sites** across 8 files (`ir/repr.rs`, `ir/lower.rs`, `ir/ssa/print.rs`,
`backend/c.rs`, `backend/ctype.rs`, `typecheck/check.rs`, plus two test files); most are
mechanical pattern updates.

Then:

1. **`ir/repr.rs:457`** — populate `throws` from `ArrowAtom::throws`.
2. **`backend/ctype.rs`** — the C type for a throwing closure's function pointer must return
   `Union([ret, throws])`. Note the union has to be *registered* as a type, or its struct is
   never emitted.
3. **`backend/c.rs:921`** (`Op::CallClosure`) — the cast already derives from the result
   value's repr; make lowering set that to the tagged result.
4. **`ir/lower.rs:1195`, `:1258`** — call `wrap_throwing` after `Op::CallClosure` when the
   callee's arrow throws.
5. **`ir/lower.rs:559`** (`lower_lambda_job`) — call `set_throws` (`ir/lower.rs:2050`) so a
   lifted throwing lambda returns a tagged result.
6. **`typecheck/check.rs:722`** — stop pinning the lambda's sink to `never`; infer the
   throws from the body and put it in the arrow the lambda builds. (Lambdas cannot *declare*
   `throws` — there is no syntax — but they never needed to: parameters are inputs and need a
   source, while the return type and throws are outputs and are always derivable from the
   body. The same reasoning already covers the return type.)
7. **`typecheck/check.rs:1124` and `env.rs:95,239`** — delete the `ThrowingFnAsValue`
   diagnostic once values actually work.

`emit_thunks` (`backend/c.rs:462`) was already fixed and should be left alone: it built the
closure-ABI adapter from the *declared* return type rather than the tagged result. Correct on
its own, and any fix needs it.

## How to verify

    cargo nextest run                                       # full suite, 716 expected
    cargo nextest run -p neon-compiler --test backend_run   # the real score; sanitizers on

To inspect one program (the CLI needs a sysroot; the repo has no installed layout):

    mkdir -p /tmp/neon-sysroot && cd /tmp/neon-sysroot
    ln -sfn ~/projects/personal/neon2/target/debug/lib lib
    ln -sfn ~/projects/personal/neon2/target/debug/include include
    ln -sfn ~/projects/personal/neon2/stdlib stdlib
    ln -sfn ~/projects/personal/neon2/runtime runtime
    export NEON_SYSROOT=/tmp/neon-sysroot
    cargo run -q -p neon-cli -- ir tests/lang/<prog>.neon
    cargo run -q -p neon-cli -- compile tests/lang/<prog>.neon -o /tmp/x \
        --sanitize address --sanitize undefined
    ASAN_OPTIONS=detect_leaks=1 /tmp/x

**The method is load-bearing** (from `HANDOFF.md`): every bug that fell in this project was
found by reading `neon ir` output or the generated `.c`; every failed fix started from
reasoning about a pass in the abstract. Read the output before theorising.

## Cases that must work when it lands

    // 1. a lambda that throws
    fn run(f: (i64) throws IndexError -> i64) throws IndexError -> i64 { try f(1) }
    run((n: i64) => if n < 0 { throw IndexError { message: "x" }; 0 } else { n })

    // 2. a named throwing function as a value
    fn boom(n: i64) throws IndexError -> i64 { if n < 0 { throw IndexError { message: "x" }; }; n }
    run(boom)                                   // currently: ThrowingFnAsValue diagnostic

    // 3. the recursive case that broke the last attempt — already in the corpus
    mu type F = null | (i64) throws :err -> F

    // 4. non-throwing closures must keep working unchanged
    //    (closures/*.neon, and `Mappable`'s map/filter/fold in std/collections/list.neon)

Add corpus tests under `tests/lang/` for 1 and 2 and register them in
`tests/lang/expected-pass.txt`. Goldens are diffed and sanitizer reports fail the test ahead
of the diff. **Check values, not just that it compiles** — two bugs this week compiled
cleanly and produced garbage, and one of them (`return` inside a lambda) was memory-unsafe.

## Related open items in `docs/finalpush.md`

Independent of this task, but adjacent — do not be surprised by them:

- A protocol method call inside an interpolation hole miscompiles: `"#{message(e)}"`.
- An error in a stdlib module is reported against the *user's* file with a nonsense span,
  which makes any stdlib mistake confusing to diagnose.
