# CBMC models

Each model is a directory:

    <name>/main.c        the harness
    <name>/sources.txt   the runtime .c files it verifies, one per line, relative to runtime/

CMake globs `*/main.c`, so adding a model touches no shared file.

    cmake -B build -S .
    cmake --build build --target verify-<name>
    cmake --build build --target verify-all

Include `../support/cbmc_support.h` **first**, before the runtime header. It gives you
`PROVE`, `ASSUME`, `NONDET_UPTO`, the `nondet_*` declarations, and the `_exit`/`abort`
stubs without which CBMC walks off the end of a trap.

---

## The rules

These are not style. Every one of them is here because breaking it cost hours.

### 1. Models verify the shipping source. Never a copy.

`sources.txt` names the real `.c` files and CBMC compiles them with the harness. Do not
paste a simplified `neon_resource_take` into the model.

The predecessor project's models inlined copies of the runtime functions, so they proved
properties about source that no longer existed and kept passing after the real code
changed. The runtime is one translation unit per area precisely so a model can take the two
files it cares about and leave the rest.

### 2. One model, one invariant.

Prefer many small models over one large one. `verify-map-probe-does-not-stop-at-a-tombstone`
beats a single `verify-map` that checks everything the map does.

Three reasons, in order of how much they matter:

- **A small model solves fast.** Solve time is superlinear in what the harness reaches, so
  splitting is not a linear trade — two models are much cheaper than one twice as big.
- **A failure names the property.** `verify-map` failing tells you the map is broken
  somewhere. A named model failing tells you which contract broke.
- **A small model is honest about its bounds.** When one harness covers ten behaviours, the
  bounds needed for the worst one silently narrow the other nine.

### 3. Avoid loops. Where you cannot, bound them with an assumption.

CBMC unrolls every loop to `--unwind` and builds all of it into the formula **before** any
assumption prunes it. So `for (i = 0; i < k; i++)` with a nondeterministic `k` costs the
full bound no matter what you later assume about `k`.

Write a constant guard with an inner break, and constrain the bound:

    ASSUME(n <= 3, "why three is enough to reach every distinct state");
    for (size_t i = 0; i < 3; i++) {
        if (i >= n) break;
        ...
    }

Keep every bound well under `--unwind`, and leave `--unwinding-assertions` on so guessing
too low is a failure rather than a proof that quietly covers less than it claims.

### 4. Stub libc. Verify our code, not the C library.

Anything crossing into libc gets a shadow in the harness that emulates the behaviour the
model needs. Otherwise CBMC spends its budget proving things about `fprintf`.

`cbmc_support.h` already stubs `_exit` and `abort`. Add your own for whatever else your
model reaches — `neon_trap`'s `fprintf`/`fflush` pull a `FILE` into every trap site, and
traps are reachable from every allocation check, which alone exhausted the default
`--object-bits 8` budget in the list model.

CBMC's own `memcpy` is also imprecise with a symbolic byte count: it leaves the copied
bytes unconstrained and every downstream property fails spuriously. That is not
Neon-specific — it reproduces in twenty lines of plain C. Enter such scenarios with
concrete lengths.

### 5. Every `ASSUME` is a hole in the proof. Say why.

`ASSUME`'s second argument is not passed to CBMC; it exists to make you write the reason.
An assumption silently narrows what was verified, so a model that assumes away the case
containing the bug still reports success **and looks like evidence**.

Prefer a literal bound in the harness's control flow over an assumption where you can: a
hardcoded `3` is visible in the code, while an assumption is something CBMC is told.

### 6. A passing model is not a result until you have seen it fail.

Break the code deliberately and confirm the model catches it. `resource/main.c` was
validated this way — three mutations, including the exact use-after-free that had shipped
that morning, each confirmed to fail the model and then reverted.

This matters most when the model finds nothing. "No bug found" and "no bug findable" look
identical in CBMC's output.

### 7. Exercise the case that makes the bug visible, not the case the code happens to use.

The resource model uses a **counted** payload, with a real retain/release witness. Every
`Resource` in the tree holds a plain integer, whose witness has no `release` — and with
that payload all three mutations above pass silently.

The shipped use-after-free survived for exactly this reason. A model built around the
common case would have reproduced the blind spot rather than closing it.

---

## Performance, and what it costs you

Sequence *depth* is the expensive dimension, not path count.

When a model releases something CBMC cannot constant-fold, the object becomes symbolically
freed: every later dereference carries that disjunction, and the drop recursion behind it
re-expands to the full unwind depth at each one. In the resource model, adding one
operation ahead of the final one took a run from **0.45s to over 300s**, and cutting the
choices at that position from seven to two changed nothing.

Making a count nondeterministic instead of enumerating it concretely has the same shape:
armed and disarmed merge into one symbolic flag ahead of a six-way switch, each branch with
an inlined drop — **under 1s to over 5 minutes**, covering identical executions.

This is the main argument for rule 2. If a model is slow, splitting it is usually the fix,
and enumerating a choice concretely usually beats letting CBMC explore it.

---

## A known boundary: heap-allocated containers

A model can drive a heap object only so far. Past that point the harness has to use a
statically allocated fixture, and some properties become unreachable — the six `map-*`
models all do this, and their SCOPE note 2 says which properties it costs.

The cause is that CBMC models a heap allocation as an untyped byte array, so **every field
read back out of a heap object is symbolic**. Two consequences, and they are independent:

- **Function pointers.** An indirect call through a heap-read pointer branches over every
  address-taken function of matching type. `neon_map_drop` is `void (*)(void*)` exactly
  like a witness `release`, so `m->vw->release` may be `neon_map_drop`, and CBMC recurses.
- **Sizes and capacities.** A loop bounded by `m->cap` has a symbolic bound, and a body
  indexing at `m->vw->size` does symbolic-stride pointer arithmetic under `--pointer-check`.

Measured on the map: three `set`s triggering one resize did not finish in 400s; the same
harness on a static map finished in **0.25s**.

**`goto-instrument --restrict-function-pointer` does not rescue this.** Tried on CBMC
6.10.0, negative result, recorded so it is not rediscovered:

- It applies cleanly and *soundly* — restricting a site emits `ASSERT false` on the
  excluded branches, so a wrong restriction fails the proof instead of quietly narrowing it.
- It does fix the first consequence: nested `neon_map_drop` loop entries fell from **9 to 2**
  over an identical symex window.
- It does not help overall, because the second consequence is untouched and is on its own
  sufficient. A harness doing **one `set` and one `release`** on a heap map — restriction
  fully applied, OOM checks off — still timed out at 100s.
- The call sites are also not discoverable: there is no `--list-function-pointers` in 6.10.0.
  Names follow `<function>.function_pointer_call.<N>` and must be recovered by applying a
  dummy restriction and reading `--show-goto-functions`. They shift whenever a call site is
  added or reordered, so any pipeline built on them is silently fragile.

The untried avenue, if this is ever revisited: assert-then-pin `m->cap` and the witness
sizes in the harness, as `pin_len` already does for `len`. Plausible — but it pins the very
field a resize property is about, so it would need care not to prove that property vacuously.
