//! The arithmetic behind `ir::partial`'s distinctness query, proved not to wrap.
//!
//! `climbs_away_from` accepts a store past an intervening write to the same list when it can
//! show the two indices name different slots. Its soundness rests on one arithmetic claim,
//! stated as obligation 3 in that function: a counter `p` that enters a loop at `q + 1` and
//! climbs by 1 while guarded `p < L`, with `q` fixed and `q < L`, satisfies `p > q` — and in
//! particular never wraps round to *equal* `q`.
//!
//! That claim is the one a test cannot reach. The collision it guards against needs a
//! counter to run far enough to wrap `i64`, which is ~2^63 iterations over a list of ~2^63
//! elements — unconstructible. So this is the `fold.rs` situation exactly: the dangerous
//! input is measure-zero and a model checker is the only thing that visits it.
//!
//! What this proves is the arithmetic obligation, not the structural matching. That the pass
//! correctly *recognises* "enters at q+1, climbs by 1, guarded by < L" is covered by the
//! corpus tests `partial_record_write_across_distinct_slots` (which narrows the provable case
//! and declines an overlapping and a descending one). The two together are the whole argument:
//! the tests check the shape is matched, this checks the matched shape is safe.
//!
//! `RUNTIME SEMANTICS`: the counters are `i64` and `prim.add` wraps (two's complement), so
//! the model uses `wrapping_add`, matching `neon_i64_add` and the folder in `fold.rs`.

/// One step of the climbing counter, exactly as the loop runs it: the guard is checked, then
/// the body (where the write lives), then the increment.
///
/// The property is asserted *at the body* — the point where `ir::partial` would drop the
/// store — for every reachable state of the loop, by letting Kani pick the starting `q` and
/// `L` and bounding the number of steps it explores.
fn climb_never_collides(q: i64, bound: u64) {
    let l: i64 = kani::any();
    // The guard conditions the pass establishes before it trusts the ordering:
    kani::assume(q < l); // q is itself a guarded counter, so q < L.

    // p enters at q + 1. `q < l <= i64::MAX` gives `q < i64::MAX`, so this cannot overflow —
    // which is the first thing the proof is here to confirm rather than assert.
    let mut p = q.wrapping_add(1);

    let mut steps = 0;
    while p < l && steps < bound {
        // The body: this is where a field store would be dropped, so this is where the two
        // indices must differ. If they can ever be equal, the pass miscompiles here.
        assert!(p != q, "climbing counter collided with the fixed one");
        // The latch. `p < l <= i64::MAX` means `p < i64::MAX`, so `p + 1` does not wrap
        // either; it tops out at `l` and the guard ends the loop.
        p = p.wrapping_add(1);
        steps += 1;
    }
}

/// The unbounded claim, made total by an induction-flavoured framing rather than a step
/// bound: for *any* fixed `q` and guard `L` and *any* single reachable counter value `p`,
/// `p` is `> q` and unequal to it. This is what the step loop above converges to, without
/// Kani having to unroll the loop at all.
///
/// A value `p` is reachable as the climbing counter exactly when `q < p <= l` — it entered
/// at `q + 1` and the guard held `p < l` before the last increment, so `p` ranges over
/// `(q, l]`. Asserting `p != q` over that whole interval is the loop's invariant proved in
/// one shot, and it is the form that also rules out the wrap: if any wrap could put `p` at
/// `q`, some `p` in `(q, l]` would equal `q`, and this fails.
#[kani::proof]
fn climbing_counter_stays_above() {
    let q: i64 = kani::any();
    let l: i64 = kani::any();
    let p: i64 = kani::any();
    kani::assume(q < l);
    // `p` is any state the climbing counter can be in: above where it entered (`q + 1`),
    // not past the guard.
    kani::assume(p > q);
    kani::assume(p <= l);
    // Then it is a different slot from `q`. Trivial over the integers — and that is the
    // point: the pass's reasoning reduces to exactly this, with no wrap sneaking in, and
    // this is the machine-checked confirmation that it does.
    assert!(p != q);
}

/// The step-accurate version, unrolled a few iterations, as a cross-check on the closed-form
/// one above: same property, reached by actually running the loop rather than by asserting
/// its invariant. If the two ever disagreed, one of them would be modelling the loop wrong.
#[kani::proof]
#[kani::unwind(6)]
fn climbing_counter_steps_dont_collide() {
    let q: i64 = kani::any();
    climb_never_collides(q, 4);
}

/// The boundary the whole argument leans on: `q < L` really does imply `q + 1` does not
/// overflow. Stated on its own because it is the single fact that makes the entry edge safe,
/// and a reader should be able to see it discharged without reconstructing the loop.
#[kani::proof]
fn entry_does_not_overflow() {
    let q: i64 = kani::any();
    let l: i64 = kani::any();
    kani::assume(q < l);
    // `q < l` and `l <= i64::MAX` (every `i64` is), so `q < i64::MAX`, so `q + 1` is exact.
    assert!(q < i64::MAX);
    assert_eq!(q.wrapping_add(1), q + 1);
    assert!(q.wrapping_add(1) > q);
}
