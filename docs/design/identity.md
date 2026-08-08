# Nominal identity

**Decision (2026-07-20): a nominal type's identity is `(declaring module path, name)`.**

Two declarations are the same type when they were written at the same path under the same
name. `vault::Secret` and `forge::Secret` are different types. `List` declared in
`std::list` is `std::list::List` wherever it is written from.

## What it is today, and why that is a hole

Identity is the **bare name**. `env.rs::record_body` ends

    let n = self.solver.t.name(&r.name);
    self.solver.t.nominal(n, args, fields)

and `r.name` is the written identifier with the module dropped. Two modules that both
declare `record Secret` do not merely look alike to the checker — they *are* one type:

    internal mod vault { opaque record Secret { code: i64 }
                         fn reveal(s: Secret) -> i64 { s.code } }
    internal mod forge { record Secret { code: i64 }
                         fn fake(n: i64) -> Secret { Secret { code: n } } }

    vault::reveal(forge::fake(99))   // prints 99

That defeats `opaque` outright, and with it `std::fs`'s cleanup guard — a `File` is a
guard in a field, and a forgeable `File` is no guard. Every gate in `opacity.md` assumes
this decision has been made; until it is, they stop honest mistakes and determined
callers alike only up to a name collision. Pinned as
`tests/lang/types/a_nominal_name_is_not_a_module_identity.neon`.

## The alternative, and why it was rejected

A **stable per-declaration id** — identity independent of location, so moving a
declaration between modules preserves it — was considered and rejected. It has no honest
derivation:

- *the path* is not stable (that is the premise);
- *(unit, name)* collides the moment one unit declares `Secret` in two modules;
- *a content hash* changes whenever a field changes, so it is stable under the one
  refactor it was supposed to survive and unstable under an unrelated one;
- *an explicit annotation* (a written id per declaration) is ergonomically hostile.

And the problem it solves is smaller than it appears. Within a compilation nothing
observes identity across a move — every reference re-resolves and the whole program is
rebuilt. Across a library boundary a moved *public* type is a breaking change, which is
correct and is exactly what Rust does. The convenience we currently enjoy — this session
relocated `List` and `Map` out of the prelude without touching a single call site — is an
artifact of identity being *wrong*, not a property worth preserving.

## Consequences, stated because people trip on them

1. **A type's module path is part of its public API.** Moving a public type between
   modules is semver-breaking for dependents. Moving a private one is free.
2. **The runtime type tag derives from the same qualified identity and must move in
   lockstep.** `ctype.rs::type_tag_name` hashes the name into the box tag that `is` and
   `as` compare on erased values. Qualify the checker but not the tag and the checker
   distinguishes two same-named types while the runtime cannot — a fresh soundness hole
   of exactly the class `opacity.md` exists to close. This is why the change is atomic.
3. **`List` and `Map` now qualify under `std` directly.** They were moved out of the
   prelude on 2026-07-20 (so that their opacity owner is a real module rather than the
   root every program shares), so the literal-matching sites below must expect
   `std::list::List`, not `List`.
4. **A same-named type in the program and the stdlib stops colliding**, which is what
   `tests/lang/modules/prelude_names_can_be_shadowed.neon` pins. The checker already
   handles it since the prelude moved to its own scope; the backend does not, and panics
   in `backend::c::op_rhs`.

## Where identity is formed and read

Formation — two sites, both small:

- `env.rs::record_body`
- the `Sort::Newtype` arm of `env.rs::instantiate`

Readback — every one of these must change in the same commit:

- `dispatch::nominal_head` — impl head matching. Note `ImplDef.target_head` is *already*
  qualified while this is bare, which may mean qualified-path impls never match (TODO
  lead L4); verify rather than assume.
- `ordered.rs` — literal `"List"` / `"Map"`
- `ir/repr.rs` — literal `"List"` / `"Map"`
- `ir/lower.rs` — `repr_from_typespec`, `repr_head`
- `check.rs` — the list literal, which builds `t.name("List")` directly rather than going
  through the declaration table
- `ctype.rs::tag_name_inner` — the runtime tag

## Acceptance

`types/a_nominal_name_is_not_a_module_identity.neon` and
`modules/prelude_names_can_be_shadowed.neon` both flip green and get ratcheted; the
corpus stays green under ASan.
