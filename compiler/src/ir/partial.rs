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
use super::ssa::{BlockId, Func, Inst, Op, Program, Term, Value};
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
            // ..and nothing may have written the list in between.
            if writes_between(f, (*src_block, *src_at), (b.id, at), list) {
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
fn writes_between(f: &Func, from: (BlockId, usize), to: (BlockId, usize), list: Value) -> bool {
    let mut writers: Vec<(BlockId, usize)> = Vec::new();
    for b in &f.blocks {
        for (i, inst) in b.insts.iter().enumerate() {
            if let Op::Native { symbol, args } = &inst.op {
                let writes_list = symbol.starts_with("neon_list_set")
                    || symbol == "neon_list_push"
                    || symbol == "neon_list_ensure_unique";
                if writes_list && args.first() == Some(&list) && (b.id, i) != to {
                    writers.push((b.id, i));
                }
            }
        }
    }
    if from.0 == to.0 {
        // One block. A write outside it cannot be between them without re-executing the
        // read; a write inside it is between them exactly when it sits between them.
        return from.1 >= to.1
            || writers.iter().any(|&(wb, wi)| wb == from.0 && wi > from.1 && wi < to.1);
    }
    // Different blocks: anything after the read in its own block, or in any block reachable
    // without going back through the read.
    let reach = reachable_without(f, from.0);
    writers.iter().any(|&(wb, wi)| {
        if wb == from.0 {
            wi > from.1
        } else {
            reach.contains(&wb)
        }
    })
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
