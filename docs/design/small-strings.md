# Small strings

`neon_str` is 24 bytes and always points at a heap allocation. On the word-frequency
benchmark that looked like the whole cost: ~77% of the run was `malloc` and `cfree`, against
6% in the map probe and 6% formatting integers. Every counted token builds a five-byte key,
hashes it, frees it. Five bytes fit in the struct with room to spare.

**This was built, and it did not pay.** The allocations went away and the run time did not
move. This document is the record of that — what was tried, why the reasoning was wrong, and
what is worth taking from it — so the same argument does not get made again from the same
profile.

## Where it stands: tried, measured, rejected

**Phase 1 landed and is on `main`.** Nothing outside these accessors touches the
representation:

```c
const char* neon_str_data(const neon_str* s);   // core.h
char*       neon_str_data_mut(neon_str* s);
size_t      neon_str_len(const neon_str* s);
bool        neon_str_is_null(const neon_str* s);
void        neon_str_retain(neon_str s);        // lifecycle.h
void        neon_str_release(neon_str s);
```

They are trivial — each returns the field it names — and `backend/c.rs` emits calls to them
rather than reaching for `.owner`, so the layout is the runtime's business alone. Phase 1
changed no behaviour and cost no measurable time. It is worth keeping on its own merits.

**Phases 2 and 3 were built, worked, and are not merged.** They are on the `sso-experiment`
branch. Small-string optimisation is *neutral at best* on this codebase's benchmarks:

| bench | baseline ×C | SSO ×C | change |
|---|---|---|---|
| word-frequency | 0.989 | 0.988 | −0.1% |
| brainfuck | 1.339 | 1.444 | **+7.9%** |
| binary-trees | 1.156 | 1.161 | +0.4% |
| n-body | 4.349 | 4.346 | −0.1% |

It was correct, not broken: 862 tests passed, ASan and UBSan were clean, and the
word-frequency profile went to *100% our own code* — `malloc`, `free`, `memcpy` and `memcmp`
all vanished from it.

### Why the premise was wrong

The case for SSO was that ~77% of word-frequency is `malloc`/`cfree`, so removing the
allocation removes most of the run. The allocation did go away. **The time did not.** It
moved into the tag branch in every accessor and the inline copies, which together cost about
what glibc was charging for a five-byte block.

That is the lesson worth keeping: a hot loop of same-size, short-lived allocations is exactly
the case `tcache` is tuned for, and "77% of the profile is the allocator" is not the same
claim as "77% of the profile is *removable*". Brainfuck shows the other side — it allocates
few strings and reads many, so it pays the branch and collects none of the saving.

### Two findings worth keeping even so

Both were necessary to get SSO merely to neutral, and both are about libc call overhead
rather than about SSO:

- **`memcpy` for a four-byte copy is a bad trade.** With a runtime length it compiles to a
  call into libc's dispatcher. A byte loop bounded by `NEON_STR_SMALL_CAP` beat it:
  0.458 → 0.450.
- **`memcmp` for map-key equality was 29% of the run**, mostly call overhead, on keys of four
  or five bytes. Comparing short strings directly was worth more than removing every
  allocation was: 0.450 → 0.381.

The second is the interesting one, and it does *not* depend on SSO. A short-length fast path
in `neon_str_eq` on the current representation is untried and is the obvious next thing to
measure here.

### If this is revisited

Do not start by re-deriving the layout; it is below and it works. Start by answering why the
allocator was cheap, and find a workload where short strings are *created* far more than they
are *read* — that is the shape SSO wins on, and word-frequency is not it despite looking like
it. Re-check `brainfuck` early, not late.

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

## Phases 2 and 3, as built

On `sso-experiment`, for reference:

1. Flip the struct to the union above.
2. Rewrite the accessors to branch on the tag — including `neon_str_is_null`, which must
   consult the tag *before* the pointer: an empty inline string leaves the bytes where
   `data` used to sit unwritten, and they can read as NULL.
3. Point the constructors at `neon_str_view` (heap/static) and `neon_str_small` (inline).
4. `neon_str_new` takes the inline path at `len <= 16`, and the four builders — `concat`,
   `add`, `repeat`, `join` — go through `neon_str_init`, which takes a `neon_str*` rather
   than returning the buffer, because returning it by value would hand back a pointer into
   a local that the return then copies away from.

Containers needed no changes: a map slot holds a `neon_str` by value and `memcpy`s it, so an
inline string's bytes move with the slot. That part worked exactly as intended — no
allocation, no refcount traffic, no pointer chase. It simply was not where the time was.

## What the branch does not have

Two things were deliberately left undone once the numbers came back, and would be needed
before this could ship:

- **A CBMC model** for the property that is easy to get wrong and that tests pass anyway: a
  small string survives being `memcpy`'d into a map slot and read back. `runtime/models/`
  is where it belongs.
- **A `neon_str_cmp` fast path.** Only `neon_str_eq` got one, because only equality showed up
  in the profile. `cmp` would need the same treatment, and its sign convention makes it
  fiddlier than `eq`.
