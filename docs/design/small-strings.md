# Small strings

`neon_str` is 24 bytes and always points at a heap allocation. On the word-frequency
benchmark that costs more than everything else combined: ~77% of the run is `malloc` and
`cfree`, against 6% in the map probe and 6% formatting integers. Every counted token builds
a five-byte key, hashes it, frees it. Five bytes fit in the struct with room to spare.

This is the plan for putting them there, and the record of what has been done so far.

## Where it stands

**Phase 1 is done.** Nothing outside four accessors touches the representation:

```c
const char* neon_str_data(const neon_str* s);   // core.h
char*       neon_str_data_mut(neon_str* s);
size_t      neon_str_len(const neon_str* s);
void        neon_str_retain(neon_str s);        // lifecycle.h
void        neon_str_release(neon_str s);
```

They are trivial today — each returns the field it names — and exist so that phase 2 is a
change to *them* rather than an audit of a hundred field accesses. `backend/c.rs` emits
calls to `neon_str_retain`/`neon_str_release` rather than reaching for `.owner`, so the
layout is entirely the runtime's business and no emitted C has to be revisited.

Phase 1 changed no behaviour and cost no measurable time: word-frequency stayed at 0.95× C
across the migration.

**Phase 2, flipping the layout, is not done.** Neither is phase 3.

## The layout

Keep 24 bytes and tag on the high bit of `len`:

```c
typedef struct {
    union {
        struct { char* data; neon_header* owner; } heap;
        char small[16];
    } u;
    size_t len;          // high bit set => small; low bits are the length
} neon_str;
```

Sixteen inline bytes, which covers the benchmark's keys and most map keys generally.

**Why the high bit of `len` and not a pointer bit.** You can reach 23 inline bytes by
overlapping the tag with the top byte of `owner`, which is what libc++ does. It assumes the
high byte of a heap pointer is zero. ARM's top-byte-ignore and MTE both put tag bits there,
and CHERI invalidates the trick outright. A length is bounded by memory in a way a pointer
representation is not, so the high bit of `len` is portable by construction. Sixteen bytes
is where the win already is; 23 is not worth buying with a portability assumption.

**Three states, and the third already exists.** Small is `len`'s high bit; heap-owned is
`owner != NULL`; a static literal is `owner == NULL`, which is exactly what `neon_str_lit`
produces today. Literals need no new machinery, and `neon_str_retain`/`release` are already
no-ops for them — which is the same path an inline string wants.

## The hazard

`neon_str_data` on a small string returns a pointer *into the struct*. `neon_str` is passed
and returned by value everywhere, so:

> **A pointer from `neon_str_data` is valid only while that exact string object is alive and
> has not been copied or moved.** Copy the string and you must re-derive the pointer from
> the copy.

This is why `neon_str_data` takes `const neon_str*` and not `neon_str`. By value it would
compute the answer for the parameter copy, hand back a pointer into it, and dangle the
moment it returned. Taking the address forces the caller to name the object it means.

Two places in the runtime hold such a pointer across a call and are worth reading before
touching this:

- `neon_io_writev` (`runtime/src/file.c`) fills an `iovec` with pointers to list elements
  and calls `writev` after the loop. Sound, because they point into the list's buffer, which
  outlives the call — copying each element into a local first would *not* be sound.
- `neon_str_join` (`runtime/src/string.c`) borrows elements from the list for the same
  reason and in the same way.

The pattern to watch for is the opposite one: copying a `neon_str` into a local, taking its
data pointer, and letting the local die while the pointer is still in use.

## Phase 2

1. Flip the struct to the union above.
2. Rewrite the five accessors to branch on the tag.
3. Fix the constructors — `neon_str_lit`, `neon_str_new`, and every `neon_str s = {...}`
   literal in `string.c` — to build the right variant.
4. `neon_str_new` produces a small string when `len <= 16`; that is the step that actually
   removes the allocations.

Containers need no changes and are where the payoff lands. A map slot holds a `neon_str` by
value and `memcpy`s it, so an inline string's bytes move with the slot: no allocation, no
refcount traffic, no pointer chase when hashing or comparing. That is the ~77%.

## What to measure, and what could go wrong

Every `neon_str_data` gains a branch. That is free for short strings and a tax on long ones,
so this is the first change in this area that can plausibly make something *slower*.
Brainfuck and binary-trees are the regression check, not word-frequency.

`runtime/models/` should get a model for the property that matters and is easy to get wrong:
a small string survives being `memcpy`'d into a map slot and read back. That is both the hot
path and the sharp edge, and it is exactly the kind of thing a test passes and a model
catches.
