# Opacity

What `opaque` promises: a value of the record is **holdable** anywhere — passed,
returned, stored, listed — but outside the owning module it is **not constructable**
and **not introspectable**. The promise is meant to hold *structurally*, not just by
hiding a declaration: `std::fs`'s `File` is a guard in a field, and a reachable guard
is the same as no guard. Who counts as inside is `env::opacity_permits`: the owner,
anything nested in it — its subtree, and nothing else: not its parent, not a sibling,
not the root.

## Who can see in: the subtree rule

**An opaque record is visible within its declaring module's subtree and nowhere else.**
The owner, and anything nested inside the owner. Not its parent, not a sibling, not the
root. Everyone else may hold, pass, return and store a value — they simply cannot build
one or look inside.

```neon
internal mod db {
    opaque record Conn { fd: i64 }              // owner module = [db]

    fn open(n: i64) -> Conn { Conn { fd: n } }  // ok — the owner
    fn fd(c: Conn) -> i64 { c.fd }              // ok — the owner

    internal mod pool {                          // a descendant
        fn make(n: i64) -> Conn { Conn { fd: n } }   // ok
        internal mod deep {
            fn peek(c: Conn) -> i64 { c.fd }         // ok — any depth
        }
    }
}

internal mod metrics {                           // a sibling of `db`
    fn record(c: db::Conn) -> i64 { c.fd }       // REJECTED
}

fn main() {
    let c = db::open(3);
    io::println(to_string(db::fd(c)));            // ok — holding and passing always are
    io::println(to_string(c.fd));                 // REJECTED — root is not in db's subtree
}
```

And the direction that was removed on 2026-07-20 — a **parent** reaching into a type its
**child** declared:

```neon
internal mod app {
    internal mod secrets {
        opaque record Token { code: i64 }        // owner = [app, secrets]
        fn value(t: Token) -> i64 { t.code }
    }
    fn peek(t: secrets::Token) -> i64 { t.code }         // REJECTED (was allowed)
    fn peek_ok(t: secrets::Token) -> i64 { secrets::value(t) }   // ok — go through the accessor
}
```

| accessor | verdict |
|---|---|
| the owner | ✓ |
| a descendant, any depth | ✓ |
| the parent | ✗ |
| a sibling | ✗ |
| the root / program | ✗ |
| anyone, holding / passing / returning | ✓ always |

**Why the parent case went.** An audit of the stdlib, the corpus and the examples found
exactly one caller — a corpus test written to exercise the rule itself. The stdlib does
not use it: `std::fs::raw::guard` builds the `File` that `std::fs` declares, which is a
*descendant reaching an ancestor's* type, the opposite direction. Several comments in
the compiler and corpus credited the stdlib to the parent branch; they were wrong about
which branch does the work. It also cost the author's mental model ("opaque means hidden
from everyone but me") and made refactoring hazardous, since moving a declaration one
level up or down silently changed who could see into it.

### The precondition: module paths must be un-claimable

"Visible to descendants" is only as strong as the impossibility of *declaring yourself* a
descendant:

```neon
mod std { mod fs { mod thief { /* now a descendant of File's owner */ } } }
```

`claim_module` is what stops this, and it does — verified 2026-07-20, the above is
rejected with `ModuleCollision` (`internal mod` does not dodge it either), and pinned as
`records/opaque_cannot_graft_into_owner_module.neon`. **The subtree rule and the
path-claiming rule are a pair; neither is sound alone.**

The prelude used to be the hole in that pair: its path was `[]`, which every program's own
root shares, so root code satisfied `same` for every prelude-declared opaque. Closed
2026-07-20 — the prelude now has a path of its own (`Env::PRELUDE`) that no source can
write, and `List`/`Map` moved to the collection modules that implement them.

What remains is **unclaimed intermediate paths**: no stdlib module occupies bare `std`
alone, so `mod std { mod totally_new { .. } }` is accepted. It grants no access to any
opaque (nothing is owned by `std` itself), but it is a namespace claim, and
claim-by-registration-order stops being meaningful once `use` loads more than one
untrusted unit. See `cross-library-identity.md`.

## Why syntactic checks could not be enough

A nominal record is an ordinary record with a `#nominal` tag field (`types.rs`), so

    Secret  <:  { code: i64 }

is *true by design* — it is the same field-wise rule that gives nominal-satisfies-
structural (tests/lang/records/structural_param_accepts_nominal.neon, tasks/207). The
original enforcement was three syntactic doors (field read, literal, destructuring),
but the leak is type-directed: any position with a structural expected type walks the
contents out, and no finite list of expression shapes covers "any position".

The core tension: subtyping is context-free — `is_empty(s ∧ ¬t)` is a function of two
types, which is what makes it memoisable — while opacity depends on a third input,
*who is asking*. Threading the asking module into `empty.rs` (option (a) of the
design discussion) would key the memo per module and poison the caching everything
rests on.

## The mechanism: sealing

`Types::seal(ty, hidden)` rewrites a type, erasing the contents of every record atom
whose `#nominal` tag is in `hidden`: user fields dropped, `rest` widened to
`any | undef`, identity (`#nominal`, `#0`… generic argument slots, `#inner`) kept and
sealed recursively. `hidden` is `Env::foreign_opaque_tags(module)` — computed per
viewing module, so opacity stays module-relative *outside* the solver, and `empty.rs`
is untouched.

The gate is then one extra question at each flow, `check.rs`:

- **`assignable` (the funnel every checked expression passes through):** when
  `actual <: expected` holds, additionally require
  `seal(actual) <: seal(expected)`. Sealing *both* sides is what keeps naming the
  type legal — `Secret` into `Secret` cancels — while a structural view fails,
  because the sealed value no longer promises the fields. Arrows are sealed inside,
  so contravariant smuggling (`let f: (Secret) -> i64 = (x: {code: i64}) => …`)
  fails by the same rule. Reported once, as an opacity error, not a mismatch.
- **`as` (`opaque_view`):** two rules, in order.
  - *Forgery, checked first:* if the **target** names a foreign opaque, the **source**
    must vouch for it — casting *to* an opaque is construction. A source vouches two
    ways: by *naming* it (`Secret | str as Secret` narrows a value that provably holds
    one), or by being *broad enough to have held one* (`seal(to) <: from`). `any` is
    broad enough — widening an opaque into `any` is a legal flow — so `(a: any) as
    List[i64]` is a recovery, and it is the pinned erased-round-trip idiom. A structural
    source is neither: outside the owner the assignable gate refuses `Secret ->
    {code: i64}`, so such a value provably never held one and `{code:99} as Secret` is
    fabrication. **The `any` case is therefore left to run time and is still open** —
    see residue 1; a static rule strict enough to stop it also rejected five corpus
    files, which is how the boundary was found.
  - *Structural view:* cast legality is overlap, and overlap survives sealing (open
    records always meet), so the cast has its own sealed rule: one side must subsume
    the other, or a newtype bridge must hold on sealed representations
    (`sealed_bridge`, `Present`-`#inner` only — an open struct also *projects*
    `#inner` and must not count as a newtype).
- **`is` (`member_gate` at all three test sites):** a structural test on an
  opaque-holding subject is rejected. Not because the boolean leaks much — there are
  no literal types in type specs, so a test cannot split an opaque record's members
  by value — but because a match arm's test *narrows*: `Secret ∧ {code: i64}` then
  satisfies `{code: i64}` sealed or not, past the point where any gate can tell the
  structural atom from honest knowledge. Naming the record (`is vault::Secret` on a
  union) stays legal.
- **dispatch (`dispatch_gate`, `bound_gate`):** impls are chosen by overlap, not
  assignability, so `impl Peek for {code: i64}` catches a `Secret` receiver — as does
  a `where T: Peek` bound discharged against that impl, and `#{s}` interpolation
  (it is `to_string` dispatch). Each chosen impl target gets the **member-wise**
  question (`member_gate`): for every hidden-tagged atom among the value's leaves,
  the member it denotes, sealed, must fit the sealed target. Member-wise, because
  the whole-type question is vacuous on intersections (`X ∧ Y <: Y` always).
- **field read / destructuring:** the syntactic doors now scan every nominal *leaf*
  (`nominal_leaves`), so a union or narrowed intersection holding a foreign opaque
  record is as closed as the record alone.

## The routes (each is a corpus file)

Closed — `tests/lang/records/`:

| route | file |
|---|---|
| argument, annotation, return, record field, list element, lambda param, contravariant fn value | opaque_no_structural_views.neon |
| generic inference and turbofish | opaque_no_generic_laundering.neon |
| `as` to a structural view; laundering via `newtype W = {code: i64}` | opaque_no_structural_cast.neon |
| `is` probe; match-arm narrow-then-flow | opaque_no_structural_test.neon |
| structural impl via dispatch; via `where` bound; via `#{}` interpolation | opaque_no_structural_impl.neon |
| field read through a union | opaque_no_union_field_read.neon |
| field read, literal, destructuring (the original doors) | opaque_hides_its_contents.neon |
| forgery by anonymous literal against a nominal expected type (annotation, argument) | opaque_no_anonymous_forgery.neon |
| forgery by cast, direct (`{code: 99} as Secret`) | opaque_no_anonymous_forgery.neon |

Still legal, deliberately — opaque_values_still_travel.neon and
opaque_nominal_flows_stay_legal.neon: holding, passing, returning, storing, listing;
`is vault::Secret` narrowing; the caller's own `newtype Wrap = vault::Secret`
wrapped and unwrapped.

## Residue — what the gate does not close

1. ~~**`any`, both directions.**~~ **The forge closed 2026-07-22; the read stays a
   pinned incoherence.** `as`-from-`any` now compares the box tag against the target's
   and traps on mismatch (`neon_box_expect` in `runtime/src/any.c`, emitted by
   `cast_expr` in `backend/c.rs`; model
   `box-expect-traps-on-a-mismatched-tag`). The forge —
   `let a: any = {code: 99}; a as vault::Secret` — traps: a structural box never
   carries `Secret`'s nominal tag, because tags are stamped only at genuine
   construction/erasure. A general soundness fix, not an opacity patch: unguarded
   `as`-from-`any` was unchecked for every type
   (`types/any_cast_checks_the_tag.neon`). `opaque_no_any_laundering.neon` is back on
   the ratchet: read answers 0, forge traps.

   *The read* — `a is {code: i64}` on an erased opaque — remains statically accepted
   and runtime-always-false: an incoherence, not a disclosure, and deliberately left
   (a naming test is legal, a structural test on `any` is module-blind by
   construction and answers honestly through the tag).

   Why it could not close statically, kept for the record: `any` genuinely holds
   opaques, so a cast out is indistinguishable *at the type level* from the pinned
   erased-recovery idiom `(li: any) as! List[i64]`; a rule strict enough to reject the
   forge also rejected five corpus files (2026-07-20). The surface design —
   `docs/design/checked-casts.md`, **implemented 2026-07-22** — finishes the story: the
   `as`/`as?`/`as!` triad makes fallibility visible, and the `opaque`/`sealed` split
   restores *compile-time* rejection for assertive casts to foreign `sealed` types
   (`records/sealed_no_assertive_recovery.neon`), with `as?`/`is` as the legal
   recovery spelling (`records/sealed_recovery_by_test.neon`).
2. **Equality and ordering.** `s == s2` and `s < s2` on foreign opaque values are
   allowed. `==` reveals only identity of contents; `<` is an ordering oracle — with
   the ability to request seals of chosen values, relative order supports binary
   search of the contents. **Closed 2026-07-22** (`docs/design/checked-casts.md`,
   decision 2): `<` on a foreign `sealed` type is a compile error
   (`records/sealed_bars_ord.neon`), `Eq` stays, plain `opaque` (List, Map)
   unaffected.
3. **Schema echoes in diagnostics.** Error messages print types; a probe file can
   learn a foreign record's field names and types from what the compiler says while
   rejecting it. Unwinnable against a compile-error oracle, and not worth chasing.
4. ~~**Bare-name tag collisions.**~~ **Closed 2026-07-20.** A `#nominal` tag used to
   carry the bare declaration name, so a program's own `Secret` and a foreign opaque
   `vault::Secret` shared one, and a same-named local record unsealed the foreign one.
   Identity is now the qualified declaration key (`docs/design/identity.md`), so a tag
   names one declaration and `opaque_record_named` is a direct lookup rather than a scan
   that had to decline when two candidates existed.
5. **`main`'s implicit error rendering.** A `Secret` thrown to the top level is
   rendered through whatever `Error` impl covers it; if that impl's target is
   structural, the rendering reads contents outside the gate's sites. Reachable only
   by combining several already-odd choices; close it by running `member_gate` where
   `implements_error` resolves, if it ever matters.

The residue is small and none of it re-opens the static story, so option (a) —
module-keyed emptiness — stays unnecessary. The one hole worth a decision is `any`,
and it is not a hole (a) could close.
