# Modules and visibility

Module paths come from file paths (`stdlib/std/io.neon` is `std::io`; a project's
`src/util.neon` is `util` and `src/net/http.neon` is `net::http`, by the same rule) and
from inline `mod` blocks, which nest inside the module that contains them. The entry
`src/main.neon` is the root module, whose path is anonymous: it can import every module
and no module can import it.

A module path is not just a namespace. It is the **identity** two other rules are decided
against: whether an `internal mod`'s contents are reachable, and whether an `opaque`
record's contents are. Both compare the accessing module's path with the declaring one.

## `internal mod`

One visibility mechanism, module-granular. There is no per-item modifier.

    // std::list
    internal mod raw {
        @native("neon_list_set") fn set_unchecked[T](xs: List[T], i: i64, v: T) -> List[T]
    }

    fn set[T](xs: List[T], i: i64, v: T) throws IndexError -> List[T] {
        if i < 0 or i >= len(xs) { throw IndexError { message: "..." }; }
        raw::set_unchecked(xs, i, v)
    }

Names inside an `internal mod` resolve **only from the subtree rooted at the module that
declared it**. `std::list::raw` resolves from `std::list` and
anything beneath it, and nowhere else. A path segment literally named `internal` marks a
directory the same way, with the same rule.

Subtree rather than parent-only, so two siblings can share an internal helper — a hashing
or witness routine wanted by both `std::list` and `std::map` would go in `std::internal`
and be reachable from both. Parent-only would force it up into the parent and out of
reach of the modules that need it.

**Enforced while resolving a name**, in `Env::candidates` — the choke point every lookup
passes through — not while checking a `use`. `candidates` calls `visible_from` and *drops*
what the caller may not reach, so the filter is the enforcement and no lookup built on it
can return an internal name by accident. Import-time enforcement would be bypassable by
writing the path out in full:

    outer::raw::secret()      // no `use` in sight

A failed lookup distinguishes the two cases, because "nothing named `secret` is in scope"
is actively misleading when the name exists. `candidates_unfiltered` re-runs the lookup
without the filter, and `hidden_by_internal` reports the owner:

    `outer::raw::secret` is internal to `outer::raw` and cannot be used from here

An `internal mod` at the root of a program is vacuous: its parent is the root, so its
subtree is everything. Harmless — there is nothing outside it to keep out.

One narrow gap in the `internal` *directory* rule: only the **first** such segment is
judged, so `a::internal::b::internal::c` is checked against `a` and not also against
`a::internal::b`. Unreachable from source today — `internal` is a keyword, so no
`mod internal` parses — and it arises only from a library tree with two nested `internal/`
directories, which the stdlib does not have. It becomes a hole the day one appears.

## Opacity: modules decide who may reach inside a record

`opaque record` hides a record's *contents*, not its type. Three routes in are closed —
reading a field, building a literal, destructuring a pattern
(`tests/lang/records/opaque_hides_its_contents.neon`) — while the value itself travels
anywhere: held in a local, passed, returned, stored in another module's record, put in a
list (`tests/lang/records/opaque_values_still_travel.neon`).

The rule is `env::opacity_permits(module, owner)`: **an opaque record is visible within
its declaring module's subtree and nowhere else.** Two cases —

- the declaring module itself;
- **anything nested inside it** — an `internal mod` is the implementation of the module
  around it and cannot be barred from the type it exists to implement:
  `std::fs::raw::guard` builds the `File` that `std::fs` declares.

Not a sibling, not the root, and **not the parent**. A parent reaching into a type its
child declared was permitted for one level until 2026-07-20; an audit found exactly one
caller — a corpus test written to exercise the rule — while the stdlib uses the opposite
direction (a *descendant* reaching an *ancestor's* type). It cost the author's mental
model and made refactoring hazardous, since moving a declaration one level changed who
could see in. A parent asks through an accessor the child exposes, like any outsider.

There is no longer a root exception: the prelude has a module path of its own
(`Env::PRELUDE`), so it no longer shares the program's. Full route enumeration, the
sealing mechanism that closes the *type-directed* leaks, and the remaining residue are in
`opacity.md`.

This is what lets a module hold an invariant its callers cannot break — a descriptor that
must not be forged from an integer, a cleanup guard that must not be disarmed behind the
module's back. `std::fs` depends on it; see `docs/design/resources.md`.

*Limitation:* a nested opaque type cannot be **named** from outside, because
`vault::inner::Secret` does not resolve at the root — a module-path resolution limit, not an
opacity one. `tests/lang/records/opaque_values_still_travel.neon` works around it by having
`vault` hand out its own single-level type.

## Sealing: one owner per module path

A module path is a claim the source makes, and until 2026-07-19 it was exploitable. A
program could write

    mod std { mod fs { fn steal(f: std::fs::File) -> ... { f.r } } }

and, as far as the checker could tell, *be inside* `std::fs` — reading the guard out of the
stdlib's opaque `File`. Verified: the same `f.r` at the program's root was refused, and
inside the forged module it compiled and ran.

`Env::claim_module` now refuses the claim. Each module passed to `Env::build_with` is a
**unit**; the first unit to introduce a path owns it, and a second unit declaring a `mod` at
that path is an error:

    `std::fs` is already a module of another library, so this `mod` may not claim that
    path. A module path is an identity: `opaque` decides who may reach inside a record by
    comparing module paths, so claiming `std::fs` would give this code the right to read
    the insides of every opaque type declared there. Give the module a name of your own

Pinned by `tests/lang/types/a_module_path_may_not_be_forged.neon`.

Refusing the *claim* is the containable fix. The alternative is giving every declaration a
provenance and teaching `opacity_permits` to compare that instead.

Three things this does and does not cover, stated because the boundaries matter:

- **Paths within one unit are free.** `std::fs` declaring `internal mod raw` claims
  `std::fs::raw` for its own unit, which is not a collision. The consequence is that the
  same-file hijack still works:

        mod outer { internal mod raw { fn secret() -> i64 { 42 } } }
        mod outer { mod raw { fn hijack() -> i64 { secret() } } }   // still accepted

  Self-harm, not a breach: one unit reaching into its own internals.
- **A library owns the paths it passes through**, not only those it occupies. `std::fs`
  and `std::path` imply `std` is spoken for even though nothing is declared there alone,
  so `mod std { .. }` is rejected too — it was accepted until 2026-07-20, a namespace
  claim then and an access grant the day anything is declared at that path. A library's
  ancestors are pre-claimed under one shared owner, because the "units" here identify
  source *files*: making each stdlib file claim `std` individually collides the stdlib
  with itself. Pinned by `modules/cannot_squat_a_library_ancestor_path.neon`.
- **The root path is no longer exempt.** It was, because the prelude and the program
  shared it; the prelude now has `Env::PRELUDE` to itself.

## Resolution

Names live in one flat table keyed by fully qualified path. A written path is resolved by
proposing candidates and letting each caller keep the first its own table holds — one rule
serving types, functions and protocols, which is why they cannot drift apart.
`candidates_raw` proposes every scope from the current module outwards (at each: a `use`
binding on the first segment, then the plain relative reading, then globs), and then the
**prelude, last**.

### `use` binds a scope; it does not re-export

A `use` binds a name in the scope that wrote it, and **that binding is inherited by nested
scopes** — deliberate, and coherent with opacity's subtree rule: a nested module is its
parent's implementation, so it sees its parent's imports.

It does *not* make the name a member of the importing module. `a::List` does not resolve
when `a` merely imported `List`; there is no `pub use`, and the prelude's re-exports work
by scope inheritance rather than by re-export. See `cross-library-identity.md`.

### Inheritance stops at the unit boundary

Every module's scope walk ends at `""`, the **root application's** scope — so without a
boundary a program's root names are in scope while resolving inside the stdlib, and they
were: `use mine::List` at a program's root made `std::string`'s own `List[str]` resolve to
the program's record, with the error surfacing in stdlib code the program never called.
Two filters apply there, both keyed on the unit that wrote the thing: a `use` binding is
only consulted by its own unit, and a **single-segment** name is only read by the root
application. Multi-segment paths still resolve, because at the root scope the "relative"
reading *is* the absolute one — `std::list::push` resolves from anywhere
precisely because of it. Pinned by `modules/imports_do_not_cross_unit_boundaries.neon`.

### The prelude has a path of its own

`Env::PRELUDE` is `#prelude`, which no source can write (`#` is not an identifier
character, the same trick `#nominal` uses), and resolution consults it **last** — so a
prelude name is in scope everywhere *and* a program can shadow any of it. It used to be
declared at the root, which is also the program's own path; that collision made
prelude-declared opaques reachable from every program, forced `List`/`Map` out to their
collection modules, and made prelude `use` re-exports unshadowable while declared names
stayed shadowable. Pinned by `modules/prelude_names_can_be_shadowed.neon`.

## Not yet

- **Per-item visibility.** A module with mostly-public contents and one private function has
  to split that function into an `internal mod`. Fine for the stdlib; may not stay fine.
  `std::fs::fail` and `std::fs::collect` are the current casualties — helpers that read as
  private and are not.
- **Packages.** Sealing is enforced per unit, and "unit" today means "one module handed to
  `Env::build_with`" — a source *file*, not a library, which is why a library's ancestor
  paths need a shared sentinel owner. Path ownership is also still first-come, which is
  sound only while exactly one untrusted unit exists. What must replace it is decided in
  `cross-library-identity.md`. `Env::check_coherence` carries a related gap: one
  orphan-impl rule from `docs/decisions.md` cannot be written because ownership is a
  property of the library a declaration came from, and `use` does not load a dependency.
