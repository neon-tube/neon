//! Writing back only the fields of a record that actually changed.
//!
//! # The cost this exists to remove
//!
//! A record is immutable, so updating one field means rebuilding all of them:
//!
//! ```neon
//! bodies = try! list::set(bodies, i, Body {
//!     x: bi.x, y: bi.y, z: bi.z,        // unchanged, copied straight back
//!     vx: bi.vx - ..., vy: ..., vz: ..., // the actual update
//!     mass: bi.mass,                     // unchanged
//! })
//! ```
//!
//! `ir::unique` already turns that into an in-place store, so there is no allocation and no
//! refcount traffic. What remains is that the store is *whole*: 56 bytes for a change of 24.
//!
//! The cost is not the extra bytes. On `bench/n-body` the whole-record store compiles to
//! unaligned 16-byte `movups`, and the next loop iteration reads the same record back with
//! 8-byte loads — which the store buffer cannot forward, so each load waits for the store to
//! reach cache. Measured: **96% of the entire run sat on three store instructions**, and
//! writing only the changed fields took it from 2.82s to 0.92s, 4.41× C to 1.43×.
//!
//! That the win is a stall and not a traffic reduction is worth keeping in mind, because it
//! is counter-intuitive and it is what the counters say: the partial version executes *more*
//! instructions than the whole-record one (24.2G against 22.3G) in a third of the cycles
//! (4.8G against 15.2G). IPC goes 1.46 → 5.04.
//!
//! # What is matched
//!
//! Exactly the shape above, after `unique` has run:
//!
//! ```text
//! %16 = index %11[%6]                     ← the record is read out of the slot
//! %37 = field %16.x                       ← a field copied back verbatim
//! %56 = record Body{x: %37, .., vx: %44}  ← ..and one that is not
//! native "neon_list_set_inplace"(%11, %6, %56)
//! ```
//!
//! A field of the stored record is **unchanged** when it is `field %S.f` for the *same* `f`,
//! where `%S` is an `index` of the same list value at the same index value. Same *value*,
//! not merely equal: `%11` and `%6` are SSA names, and two names that happen to hold the
//! same list are not something this pass tries to see through.
//!
//! Every unchanged field is dropped from the store. The remaining ones become one
//! `neon_list_set_field_inplace` each. If nothing is unchanged the site is left alone —
//! a whole-record store is the better code when the whole record really is new.
//!
//! # The safety condition, and why it is the interesting part
//!
//! Dropping a field's store is only sound if the slot **already holds that value**. The
//! record was read from the slot, so it did at the moment of the read; the obligation is
//! that nothing wrote to that list in between. If something did, the source semantics say
//! the slot ends up holding the *old* field values (the ones captured in `%S`), and skipping
//! the stores would leave the newer ones in place. That is a miscompile, and it is silent.
//!
//! So: no write to the same list may lie between the `index` and the store. "Between" is
//! over the CFG, not the instruction list — a write in another block that can reach this one
//! counts. The walk below is deliberately blunt: any `neon_list_set*` on that list, anywhere
//! on any path from the read to the store, declines the site.
//!
//! **What this costs, and it is the thing to improve first.** `advance`'s inner loop reads
//! body `i` and body `j`, then writes `i` and then `j`. The write to `j` has the write to `i`
//! between it and its read, so this pass declines it — even though `i != j` always, since
//! `j` runs `i+1..n`. Proving that needs induction-variable range analysis, which this pass
//! does not have. Measured, the two sites it does take are worth 4.41× C → 2.53×, and the
//! declined third is worth as much again (→ 1.43×). It is the single largest known
//! optimisation left in the compiler.
//!
//! # Why a native and not a new `Op`
//!
//! `neon_list_set_field_inplace(list, index, field_index, value)`, where `field_index` is an
//! ordinary `const.i64` value naming the field's position in the record repr. A new `Op`
//! variant would be the more honest IR, and it would also mean touching all seven files that
//! match on `Op` exhaustively, for a store that every one of those passes wants to treat
//! exactly as it treats `neon_list_set_inplace`. Encoding the field as an operand gets that
//! for free: effects sees an effectful native, refcounting sees borrowed arguments, and the
//! operand walkers need no new arm.
//!
//! The position operand is a *real* instruction, not a sentinel packed into a `Value`. A
//! fake value would be silently wrong the moment any pass rewrote or refcounted operands,
//! and every one of them does. Codegen reads the constant back out of its defining
//! instruction, which is why `emit_list_builder` takes the function and not just the args.
//!
//! Downstream obligations, stated where they land: `effects::op_is_effectful` must call this
//! effectful (it is a store), `refcount::operand_uses` must treat it as *borrowing* like its
//! whole-record counterpart, and `backend/c.rs` derives the record type from the list
//! argument, since there is no result to ask.

use super::repr::Repr;
use super::ssa::{BlockId, Func, Inst, Op, PrimOp, Program, Term, Value};
use std::collections::{HashMap, HashSet};

/// The whole-record in-place store this pass splits up.
const SET_INPLACE: &str = "neon_list_set_inplace";

/// The per-field store it splits into.
pub const SET_FIELD_INPLACE: &str = "neon_list_set_field_inplace";

/// A store that can be narrowed, and what to narrow it to.
struct Site {
    block: BlockId,
    /// Index of the store instruction within its block.
    at: usize,
    list: Value,
    index: Value,
    /// `(field position in the repr, value)` for the fields that actually change.
    changed: Vec<(usize, Value)>,
}

/// Narrow every whole-record in-place store whose record is mostly the slot's own contents.
pub fn apply(program: &mut Program) {
    for f in &mut program.funcs {
        let sites = find(f);
        // Back to front within each block, so an earlier site's index stays valid while a
        // later one is being replaced.
        let mut by_block: HashMap<BlockId, Vec<Site>> = HashMap::new();
        for s in sites {
            by_block.entry(s.block).or_default().push(s);
        }
        for (block, mut sites) in by_block {
            sites.sort_by_key(|s| std::cmp::Reverse(s.at));
            for s in sites {
                rewrite(f, block, &s);
            }
        }
    }
}

/// Every store in `f` that qualifies.
fn find(f: &Func) -> Vec<Site> {
    let defs = definitions(f);
    let mut out = Vec::new();
    for b in &f.blocks {
        for (at, inst) in b.insts.iter().enumerate() {
            let Op::Native { symbol, args } = &inst.op else { continue };
            if symbol != SET_INPLACE || args.len() != 3 {
                continue;
            }
            let (list, index, rec) = (args[0], args[1], args[2]);
            let Some(Op::MakeRecord { fields, .. }) = defs.get(&rec).map(|(_, _, op)| op) else {
                continue;
            };
            // The order of `fields` is the record's declared order, which is the order the
            // repr lays out and the order codegen names them in. Position is what the
            // emitted native carries, so it must come from here and not be recomputed.
            let mut changed = Vec::new();
            let mut source: Option<Value> = None;
            for (pos, (name, v)) in fields.iter().enumerate() {
                match unchanged_from(&defs, *v, name) {
                    // `field %S.name` — a candidate for skipping, if `%S` is this slot.
                    Some(s) if source.is_none_or(|prev| prev == s) => source = Some(s),
                    _ => changed.push((pos, *v)),
                }
            }
            // Every field is new: a whole-record store is the right code, leave it.
            if changed.len() == fields.len() {
                continue;
            }
            // Every field is *unchanged*: the store writes the slot's own contents back over
            // itself. Tempting to delete outright, and wrong to — `neon_list_set_inplace`
            // traps on a bad index, and the per-field stores that would have carried that
            // check do not exist when there are no fields to store. Deleting it turns
            // `xs[99] = xs[99]` on a three-element list from a trap into silence.
            if changed.is_empty() {
                continue;
            }
            // The record the unchanged fields came from must be *this* slot.
            let Some(src) = source else { continue };
            let Some((src_block, src_at, Op::Index { base, index: idx })) = defs.get(&src) else {
                continue;
            };
            if *base != list || *idx != index {
                continue;
            }
            // ..and every write to the list in between must land on a different slot.
            // Usually there are none; when there are, `provably_distinct` is what lets
            // `advance`'s write to `j` survive the write to `i` that precedes it.
            let blocking = intervening_writes(f, (*src_block, *src_at), (b.id, at), list);
            if !blocking.iter().all(|&w| provably_distinct(f, w, index)) {
                continue;
            }
            out.push(Site { block: b.id, at, list, index, changed });
        }
    }
    out
}

/// `Some(base)` when `v` is `field base.name` — the projection that copies a field straight
/// back out of the record it came from. The field name must match the one being stored into:
/// `Body { x: bi.y }` moves a value between fields and changes the record.
fn unchanged_from(
    defs: &HashMap<Value, (BlockId, usize, Op)>,
    v: Value,
    name: &str,
) -> Option<Value> {
    match defs.get(&v).map(|(_, _, op)| op) {
        Some(Op::Field { base, field }) if field == name => Some(*base),
        _ => None,
    }
}

/// Whether any write to `list` can execute between the read at `from` and the store at `to`.
///
/// **The same-block case is the one that matters, and it is not the blunt one.** When the
/// read and the store are in one block, a path that leaves the block and comes back has
/// re-executed the read — which rebinds it, and restarts the question for the next
/// iteration's value. So only a write lying strictly between them in that block counts. This
/// is the rule `ir::unique` states as "re-entering the defining block rebinds the value";
/// without it, `advance`'s write to slot `i` is declined because the *later* write to slot
/// `j` is reachable round the loop, which is true and irrelevant.
///
/// The cross-block case stays blunt: any write to the list in a block reachable from the
/// read, without re-entering the read's own block, declines the site. That is imprecise in
/// the accept-nothing direction, which is the side to be wrong on — a wrongly accepted site
/// is a silent miscompile.
fn intervening_writes(
    f: &Func,
    from: (BlockId, usize),
    to: (BlockId, usize),
    list: Value,
) -> Vec<Value> {
    // The index each write lands on, so the caller can ask whether it is a different slot.
    // A write with no index operand (`push`, `ensure_unique`) can touch anything, so it is
    // recorded as `None` and can never be argued away.
    let mut writers: Vec<(BlockId, usize, Option<Value>)> = Vec::new();
    for b in &f.blocks {
        for (i, inst) in b.insts.iter().enumerate() {
            if let Op::Native { symbol, args } = &inst.op {
                let indexed = symbol.starts_with("neon_list_set");
                let whole_list = symbol == "neon_list_push" || symbol == "neon_list_ensure_unique";
                if (indexed || whole_list) && args.first() == Some(&list) && (b.id, i) != to {
                    writers.push((b.id, i, if indexed { args.get(1).copied() } else { None }));
                }
            }
        }
    }
    if from.0 == to.0 && from.1 >= to.1 {
        // The read does not precede the store: not a shape this pass matched. One
        // unattributable write is enough to decline.
        return vec![Value(u32::MAX)];
    }
    let reach = reachable_without(f, from.0);
    let between = |wb: BlockId, wi: usize| {
        if from.0 == to.0 {
            // One block. Leaving it and coming back re-executes the read, which rebinds it
            // and restarts the question, so only a write lying strictly between counts.
            wb == from.0 && wi > from.1 && wi < to.1
        } else if wb == from.0 {
            wi > from.1
        } else {
            reach.contains(&wb)
        }
    };
    writers
        .iter()
        .filter(|&&(wb, wi, _)| between(wb, wi))
        // `None` -- a write with no index -- becomes an index nothing can be proved distinct
        // from, which is exactly how it should behave.
        .map(|&(_, _, idx)| idx.unwrap_or(Value(u32::MAX)))
        .collect()
}

/// Blocks reachable from `start`'s successors without re-entering `start` itself.
fn reachable_without(f: &Func, start: BlockId) -> HashSet<BlockId> {
    let succ: HashMap<BlockId, Vec<BlockId>> =
        f.blocks.iter().map(|b| (b.id, successors(&b.term))).collect();
    let mut seen = HashSet::new();
    let mut stack: Vec<BlockId> = succ.get(&start).cloned().unwrap_or_default();
    while let Some(b) = stack.pop() {
        if b == start || !seen.insert(b) {
            continue;
        }
        for s in succ.get(&b).into_iter().flatten() {
            stack.push(*s);
        }
    }
    seen
}

/// Whether a native is the per-field store, and if so which field position it names.
///
/// The position is a `const.i64` operand, so this needs the function to read it back out of
/// its defining instruction. Codegen is the only caller: every other pass wants to treat
/// this exactly as it treats the whole-record store, which is what encoding the field as an
/// operand rather than a new `Op` variant buys.
pub fn field_position(f: &Func, symbol: &str, args: &[Value]) -> Option<usize> {
    if symbol != SET_FIELD_INPLACE || args.len() != 4 {
        return None;
    }
    for b in &f.blocks {
        for inst in &b.insts {
            if inst.result == Some(args[2]) {
                if let Op::ConstI64(n) = inst.op {
                    return usize::try_from(n).ok();
                }
            }
        }
    }
    None
}

fn successors(t: &Term) -> Vec<BlockId> {
    match t {
        Term::Jump(tg) => vec![tg.to],
        Term::Branch { then, els, .. } => vec![then.to, els.to],
        Term::Switch { arms, default, .. } => {
            arms.iter().map(|(_, t)| t.to).chain(std::iter::once(default.to)).collect()
        }
        Term::Ret(_) | Term::Throw(_) | Term::Unreachable => vec![],
    }
}

/// Where each value is defined: its block, its position in that block, and its op.
fn definitions(f: &Func) -> HashMap<Value, (BlockId, usize, Op)> {
    let mut out = HashMap::new();
    for b in &f.blocks {
        for (i, inst) in b.insts.iter().enumerate() {
            if let Some(v) = inst.result {
                out.insert(v, (b.id, i, inst.op.clone()));
            }
        }
    }
    out
}

/// Replace one whole-record store with one store per changed field, each preceded by the
/// constant naming which field it writes.
fn rewrite(f: &mut Func, block: BlockId, s: &Site) {
    // Mint the position constants first: `new_value` borrows the function mutably, and the
    // block does too.
    let ty = f.value_ty(s.index);
    let positions: Vec<Value> =
        s.changed.iter().map(|_| f.new_value(Repr::I64, ty)).collect();

    let Some(b) = f.blocks.iter_mut().find(|b| b.id == block) else { return };
    let mut replacement = Vec::with_capacity(s.changed.len() * 2);
    for ((pos, v), posv) in s.changed.iter().zip(positions) {
        replacement.push(Inst { result: Some(posv), op: Op::ConstI64(*pos as i64) });
        replacement.push(Inst {
            result: None,
            op: Op::Native {
                symbol: SET_FIELD_INPLACE.into(),
                args: vec![s.list, s.index, posv, *v],
            },
        });
    }
    b.insts.splice(s.at..=s.at, replacement);
}

// ---- proving two indices name different slots ----

/// Whether `a` and `b` are guaranteed to differ wherever both are live.
///
/// One shape only, and it is the one that matters: a loop counter that starts strictly
/// above another and only ever climbs. That is `advance`'s `j`, which runs `i+1..n` while
/// `i` sits still — the case that, unproved, costs half the available win on n-body.
///
/// Deliberately narrow. This is not range analysis and does not want to become it: a wrong
/// answer here does not crash, it silently drops a field store and prints wrong numbers. The
/// query answers "yes" only for a shape it can walk end to end, and "no" for everything
/// else, including plenty of pairs that do in fact differ.
fn provably_distinct(f: &Func, a: Value, b: Value) -> bool {
    climbs_away_from(f, a, b) || climbs_away_from(f, b, a)
}

/// `p` is a loop-header parameter that enters at `q + k` for some `k > 0`, only ever
/// increases, and cannot wrap round to meet `q` again.
///
/// The three obligations, and none is optional:
///
/// 1. **Every incoming edge** gives `p` either `q + k` (loop entry) or `p + k` (the latch),
///    `k` a positive constant. One unexplained edge and the answer is no.
/// 2. **`q` does not move inside the loop.** If it did, `p > q` at entry would say nothing
///    about the two at the write. Checked by requiring `q`'s definition to sit outside the
///    loop body.
/// 3. **Neither can wrap.** `prim.add` wraps, so "only increases" is not the same as "stays
///    above", and the gap is not academic: an entry of `q + k` for a pathological constant
///    `k` near `i64::MAX` wraps to *below* `q`, and a climbing `p` then meets `q` from
///    underneath. Closed by requiring the stride to be exactly `1` at both the entry and the
///    latch, and both loops to be guarded by `< L` against the *same* `L`. Then: `q < L <=
///    i64::MAX`, so `q + 1` does not overflow and is `> q`; `p` enters there and the guard
///    holds `p < L <= i64::MAX`, so each `p + 1` tops out at `L` rather than wrapping. `p`
///    starts above `q`, `q` is fixed, and `p` climbs without wrapping — so `p > q` at the
///    write. A stride of 1 is what every real counting loop uses and is n-body's; a wider or
///    variable stride is declined rather than reasoned about.
fn climbs_away_from(f: &Func, p: Value, q: Value) -> bool {
    let Some((header, pos)) = param_position(f, p) else { return false };
    let Some(guard) = loop_guard(f, header, p) else { return false };

    let body = loop_body(f, header);
    // (2) `q` must be fixed for the duration.
    if defined_inside(f, &body, q) {
        return false;
    }
    // (3) `q`'s own loop must be bounded by the same length value.
    let Some((q_header, _)) = param_position(f, q) else { return false };
    if loop_guard(f, q_header, q) != Some(guard) {
        return false;
    }

    // (1) every way into the header explains `p`.
    let mut saw_entry = false;
    let mut edges = 0;
    for b in &f.blocks {
        for tgt in targets(&b.term) {
            if tgt.to != header {
                continue;
            }
            edges += 1;
            let Some(&arg) = tgt.args.get(pos) else { return false };
            match add_of_one(f, arg) {
                // Loop entry: `p = q + 1`.
                Some(base) if base == q => saw_entry = true,
                // The latch: `p = p + 1`.
                Some(base) if base == p => {}
                _ => return false,
            }
        }
    }
    edges > 0 && saw_entry
}

/// The block and parameter position `v` is bound at, if it is a block parameter.
fn param_position(f: &Func, v: Value) -> Option<(BlockId, usize)> {
    f.blocks.iter().find_map(|b| b.params.iter().position(|&x| x == v).map(|i| (b.id, i)))
}

/// The value a header's loop is bounded by: `branch (prim.lt v, L)` gives `L`.
fn loop_guard(f: &Func, header: BlockId, v: Value) -> Option<Value> {
    let b = f.blocks.iter().find(|b| b.id == header)?;
    let Term::Branch { cond, .. } = &b.term else { return None };
    b.insts.iter().find_map(|inst| match (&inst.op, inst.result) {
        (Op::Prim(PrimOp::Lt, args), Some(r))
            if r == *cond && args.first() == Some(&v) =>
        {
            args.get(1).copied()
        }
        _ => None,
    })
}

/// `Some(base)` when `v` is `base + 1`. Exactly one, not any positive constant: see
/// obligation 3 on `climbs_away_from` for why a wider stride is refused rather than trusted.
fn add_of_one(f: &Func, v: Value) -> Option<Value> {
    for b in &f.blocks {
        for inst in &b.insts {
            if inst.result != Some(v) {
                continue;
            }
            let Op::Prim(PrimOp::Add, args) = &inst.op else { return None };
            let (&x, &y) = (args.first()?, args.get(1)?);
            // Either operand may be the constant `1`.
            if const_i64(f, y) == Some(1) {
                return Some(x);
            }
            if const_i64(f, x) == Some(1) {
                return Some(y);
            }
            return None;
        }
    }
    None
}

fn const_i64(f: &Func, v: Value) -> Option<i64> {
    f.blocks.iter().flat_map(|b| b.insts.iter()).find_map(|inst| match (&inst.op, inst.result) {
        (Op::ConstI64(n), Some(r)) if r == v => Some(*n),
        _ => None,
    })
}

/// The *natural* loop `header` heads: the header, plus every block that can reach one of its
/// back edges without passing through the header again.
///
/// "Reachable from the header and able to get back" is not the same thing, and was the first
/// version's bug. In `advance`, `block4` heads the inner loop but its exit runs
/// `block6 -> block1 -> block2 -> block4`, so the outer header is reachable from the inner one
/// and can reach it again. That made the inner loop's body swallow the outer loop, and the
/// outer counter `i` then looked as though it changed inside the inner loop -- which is
/// precisely the fact this analysis needs to be false.
///
/// The distinguisher is dominance: `block10` is a back edge because `block4` dominates it,
/// while `block2` is not, because control reaches it without ever entering `block4`.
fn loop_body(f: &Func, header: BlockId) -> HashSet<BlockId> {
    let dom = dominators(f);
    let preds = predecessors(f);
    let mut body: HashSet<BlockId> = std::iter::once(header).collect();
    for b in &f.blocks {
        let is_latch = targets(&b.term).iter().any(|t| t.to == header)
            && dom.get(&b.id).is_some_and(|d| d.contains(&header));
        if !is_latch {
            continue;
        }
        let mut stack = vec![b.id];
        while let Some(n) = stack.pop() {
            if n == header || !body.insert(n) {
                continue;
            }
            for &p in preds.get(&n).into_iter().flatten() {
                stack.push(p);
            }
        }
    }
    body
}

/// Dominator sets, by the standard iterative fixpoint. The CFGs here are small, so the naive
/// form is the right one: no dominator tree and no immediate-dominator bookkeeping to keep
/// correct for the sake of an asymptotic nobody reaches.
fn dominators(f: &Func) -> HashMap<BlockId, HashSet<BlockId>> {
    let all: HashSet<BlockId> = f.blocks.iter().map(|b| b.id).collect();
    let preds = predecessors(f);
    let mut dom: HashMap<BlockId, HashSet<BlockId>> = f
        .blocks
        .iter()
        .map(|b| {
            let set = if b.id == f.entry { std::iter::once(b.id).collect() } else { all.clone() };
            (b.id, set)
        })
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        for b in &f.blocks {
            if b.id == f.entry {
                continue;
            }
            let mut next: Option<HashSet<BlockId>> = None;
            for p in preds.get(&b.id).into_iter().flatten() {
                let d = &dom[p];
                next = Some(match next {
                    None => d.clone(),
                    Some(acc) => acc.intersection(d).copied().collect(),
                });
            }
            let mut next = next.unwrap_or_default();
            next.insert(b.id);
            if dom[&b.id] != next {
                dom.insert(b.id, next);
                changed = true;
            }
        }
    }
    dom
}

fn predecessors(f: &Func) -> HashMap<BlockId, Vec<BlockId>> {
    let mut out: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    for b in &f.blocks {
        for t in targets(&b.term) {
            out.entry(t.to).or_default().push(b.id);
        }
    }
    out
}

/// Whether `v` is bound anywhere inside `body` -- as an instruction result or a block
/// parameter. A value bound inside the loop takes a fresh value each iteration.
fn defined_inside(f: &Func, body: &HashSet<BlockId>, v: Value) -> bool {
    f.blocks.iter().filter(|b| body.contains(&b.id)).any(|b| {
        b.params.contains(&v) || b.insts.iter().any(|i| i.result == Some(v))
    })
}

fn targets(t: &Term) -> Vec<&super::ssa::Target> {
    match t {
        Term::Jump(tg) => vec![tg],
        Term::Branch { then, els, .. } => vec![then, els],
        Term::Switch { arms, default, .. } => {
            arms.iter().map(|(_, t)| t).chain(std::iter::once(default)).collect()
        }
        Term::Ret(_) | Term::Throw(_) | Term::Unreachable => vec![],
    }
}
