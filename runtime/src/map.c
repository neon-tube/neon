#include "libneon_rt.h"

#include <stdlib.h>

// ---- map ----
//
// Open addressing with linear probing. `ctrl` marks each slot empty, dead (a tombstone
// left by a removal) or full; keys and values sit in parallel arrays sized by their
// witnesses. Equality and hashing come from the key witness, so a `str` key compares by
// content rather than by address.

static void neon_map_drop(void* p) {
    neon_map* m = (neon_map*)p;
    for (size_t i = 0; i < m->cap; i++) {
        if (m->ctrl[i] != NEON_MAP_FULL) continue;
        if (m->kw->value->release) m->kw->value->release(m->keys + i * m->kw->value->size);
        if (m->vw->release) m->vw->release(m->vals + i * m->vw->size);
    }
    free(m->ctrl);
    free(m->keys);
    free(m->vals);
    neon_free(m);
}

static neon_map* neon_map_alloc(const neon_key_witness* kw, const neon_witness* vw, size_t cap) {
    neon_map* m = (neon_map*)neon_alloc(sizeof(neon_map) - sizeof(neon_header), neon_map_drop);
    m->kw = kw;
    m->vw = vw;
    m->len = 0;
    m->cap = cap;
    m->ctrl = (unsigned char*)calloc(cap ? cap : 1, 1);
    m->keys = (char*)malloc((cap ? cap : 1) * kw->value->size);
    m->vals = (char*)malloc((cap ? cap : 1) * vw->size);
    if (m->ctrl == NULL || m->keys == NULL || m->vals == NULL) neon_trap("out of memory");
    return m;
}

neon_map* neon_map_new(const neon_key_witness* kw, const neon_witness* vw) {
    return neon_map_alloc(kw, vw, 8);
}

// The slot a key belongs in: its own if present, else the first free slot on its probe.
static size_t neon_map_slot(neon_map* m, const void* key, bool* found) {
    size_t ksz = m->kw->value->size;
    size_t mask = m->cap - 1;
    size_t i = (size_t)m->kw->hash(key) & mask;
    size_t first_dead = SIZE_MAX;
    for (size_t n = 0; n < m->cap; n++) {
        unsigned char c = m->ctrl[i];
        if (c == NEON_MAP_EMPTY) {
            *found = false;
            return first_dead != SIZE_MAX ? first_dead : i;
        }
        if (c == NEON_MAP_DEAD) {
            if (first_dead == SIZE_MAX) first_dead = i;
        } else if (m->kw->eq(m->keys + i * ksz, key)) {
            *found = true;
            return i;
        }
        i = (i + 1) & mask;
    }
    *found = false;
    return first_dead != SIZE_MAX ? first_dead : 0;
}

// Release a key the map is not going to store. Every map native *consumes* its key, the
// same convention every other native follows: the caller hands over one reference and does
// not release it afterwards. `set` discharges that by moving the key into the table, and
// the lookups discharge it here. Missing this leaked a key per lookup -- invisible for an
// `i64` or a literal `str`, and 72 bytes a call for a `List` key.
static void neon_map_release_key(neon_map* m, const void* key) {
    if (m->kw->value->release) {
        m->kw->value->release((void*)key);
    }
}

void* neon_map_find(neon_map* m, const void* key) {
    bool found = false;
    size_t i = neon_map_slot(m, key, &found);
    return found ? m->vals + i * m->vw->size : NULL;
}

// Borrows its key, unlike `contains` and `set`. It is reached through `Op::Index` rather
// than `Op::Native`, and the refcount pass releases an index's operands itself -- releasing
// here as well double-freed the key.
void* neon_map_at(neon_map* m, const void* key) {
    void* v = neon_map_find(m, key);
    if (v == NULL) {
        neon_trap("key not present");
    }
    return v;
}

int64_t neon_map_len(neon_map* m) {
    int64_t n = (int64_t)m->len;
    neon_release((neon_header*)m);
    return n;
}

bool neon_map_contains(neon_map* m, const void* key) {
    bool r = neon_map_find(m, key) != NULL;
    neon_map_release_key(m, key);
    neon_release((neon_header*)m);
    return r;
}

// A fresh map holding everything this one does, retaining each entry it copies.
static neon_map* neon_map_clone(neon_map* m, size_t cap) {
    neon_map* c = neon_map_alloc(m->kw, m->vw, cap);
    size_t ksz = m->kw->value->size, vsz = m->vw->size;
    for (size_t i = 0; i < m->cap; i++) {
        if (m->ctrl[i] != NEON_MAP_FULL) continue;
        bool found = false;
        size_t j = neon_map_slot(c, m->keys + i * ksz, &found);
        memcpy(c->keys + j * ksz, m->keys + i * ksz, ksz);
        memcpy(c->vals + j * vsz, m->vals + i * vsz, vsz);
        c->ctrl[j] = NEON_MAP_FULL;
        c->len++;
        if (m->kw->value->retain) m->kw->value->retain(c->keys + j * ksz);
        if (m->vw->retain) m->vw->retain(c->vals + j * vsz);
    }
    return c;
}

// Drop `key` if present. Consumes the map and the key, like `set`.
//
// The slot becomes a tombstone rather than empty: a probe that walked past this slot to
// place a later key must still walk past it, and marking it empty would cut that chain and
// lose the entry. `len` drops, so the load factor still falls.
neon_map* neon_map_remove(neon_map* m, const void* key) {
    // Copy before mutating when shared, exactly as `set` does -- removal is a mutation, and
    // these are values.
    if (m->header.rc > 1) {
        neon_map* c = neon_map_clone(m, m->cap);
        neon_release((neon_header*)m);
        m = c;
    }
    bool found = false;
    size_t i = neon_map_slot(m, key, &found);
    if (found) {
        size_t ksz = m->kw->value->size, vsz = m->vw->size;
        if (m->kw->value->release) m->kw->value->release(m->keys + i * ksz);
        if (m->vw->release) m->vw->release(m->vals + i * vsz);
        m->ctrl[i] = NEON_MAP_DEAD;
        m->len--;
    }
    neon_map_release_key(m, key);
    return m;
}

// Two maps are equal when they hold the same set of keys with equal values. Borrows both.
//
// Iteration order is not part of the answer, so this looks each key up in `b` rather than
// walking the two slot arrays in step: the same entries can sit at different slots after a
// different insertion history, and an open-addressed table has no canonical order.
// Comparing lengths first makes "same keys" enough -- if every key of `a` is in `b` and the
// counts match, neither can hold a key the other lacks.
bool neon_map_eq(neon_map* a, neon_map* b) {
    if (a == b) {
        return true;
    }
    if (a->len != b->len) {
        return false;
    }
    size_t ksz = a->kw->value->size, vsz = a->vw->size;
    for (size_t i = 0; i < a->cap; i++) {
        if (a->ctrl[i] != NEON_MAP_FULL) {
            continue;
        }
        const void* key = a->keys + i * ksz;
        void* other = neon_map_find(b, key);
        if (other == NULL || !a->vw->eq(a->vals + i * vsz, other)) {
            return false;
        }
    }
    return true;
}

neon_map* neon_map_set(neon_map* m, const void* key, const void* val) {
    // Shared, or too full to probe well: copy before mutating. Uniquely owned maps are
    // updated in place, which is what makes the immutable interface cheap.
    if (m->header.rc > 1 || (m->len + 1) * 4 >= m->cap * 3) {
        size_t cap = (m->len + 1) * 4 >= m->cap * 3 ? m->cap * 2 : m->cap;
        neon_map* c = neon_map_clone(m, cap);
        neon_release((neon_header*)m);
        m = c;
    }
    size_t ksz = m->kw->value->size, vsz = m->vw->size;
    bool found = false;
    size_t i = neon_map_slot(m, key, &found);
    if (found) {
        // The table keeps the key it already has, so the incoming one is ours to drop.
        neon_map_release_key(m, key);
        if (m->vw->release) m->vw->release(m->vals + i * vsz);
    } else {
        memcpy(m->keys + i * ksz, key, ksz);
        m->ctrl[i] = NEON_MAP_FULL;
        m->len++;
    }
    memcpy(m->vals + i * vsz, val, vsz);
    return m;
}

// `keys`/`values` hand back a list; the element witness comes from codegen, which knows
// the concrete element type.
static neon_list* neon_map_collect(neon_map* m, const neon_witness* w, bool want_keys) {
    neon_list* out = neon_list_new_with_capacity(w, (int64_t)m->len);
    size_t esz = w->size;
    size_t ksz = m->kw->value->size, vsz = m->vw->size;
    for (size_t i = 0; i < m->cap; i++) {
        if (m->ctrl[i] != NEON_MAP_FULL) continue;
        const char* src = want_keys ? m->keys + i * ksz : m->vals + i * vsz;
        memcpy(out->data + out->len * esz, src, esz);
        if (w->retain) w->retain(out->data + out->len * esz);
        out->len++;
    }
    neon_release((neon_header*)m);
    return out;
}

neon_list* neon_map_keys(neon_map* m, const neon_witness* w) {
    return neon_map_collect(m, w, true);
}

neon_list* neon_map_values(neon_map* m, const neon_witness* w) {
    return neon_map_collect(m, w, false);
}
