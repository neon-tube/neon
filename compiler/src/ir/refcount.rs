//! Reference-count insertion, Perceus-style: last-use-driven. See `docs/design/ir.md`.
//!
//! Every counted (pointer-backed) value owns one reference when it is produced. A use is
//! either **consuming** (it takes ownership -- a call argument, a field stored into an
//! aggregate, a returned or branched value) or **borrowing** (it only reads -- a field
//! access, a tag test). The pass walks each block backwards over a cross-block liveness
//! result and inserts:
//!
//! - a `Retain` before a consuming use of a value that is still live afterwards (it needs
//!   its own owned reference), and
//! - a `Release` after the last use of a value that is not moved out (a borrow with no
//!   later use, or a dead result).
//!
//! Because the language is immutable, values are acyclic and this is complete: the last
//! release always runs, and nothing leaks. Moves at last use and `rc == 1` reuse are the
//! refinements the optimiser adds on top; this establishes the balanced baseline.

use super::ssa::{Func, Inst, Op, Program, Term, Value};
use std::collections::{HashMap, HashSet};

pub fn insert(program: &mut Program) {
    for f in &mut program.funcs {
        insert_fn(f);
    }
}

/// Where a value was read out of: a `Field`/`Elem` result *aliases* what its aggregate
/// owns rather than holding a reference of its own. The base must therefore outlive every
/// use of what was read from it — releasing the base the moment the base itself is dead
/// frees the thing the reader is still holding.
fn base_of(f: &Func, ptr: &HashSet<Value>) -> HashMap<Value, Value> {
    let mut out = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            // Every projection: a field or element read, a cast between a union and one of
            // its variants, and the two tagged-result unwraps. All of them hand back a
            // view into their operand. `Index` is not one — `emit_index` retains what it
            // reads, so that result owns itself.
            let projected = match (inst.result, &inst.op) {
                (Some(v), Op::Field { base, .. } | Op::Elem { base, .. }) => Some((v, *base)),
                (
                    Some(v),
                    Op::Cast(base) | Op::UnwrapOk(base) | Op::UnwrapErr(base),
                ) => Some((v, *base)),
                _ => None,
            };
            if let Some((v, base)) = projected {
                if ptr.contains(&v) && ptr.contains(&base) {
                    out.insert(v, base);
                }
            }
        }
    }
    out
}

/// Extend a live set with the bases every live value was read out of.
fn with_bases(set: &mut HashSet<Value>, base_of: &HashMap<Value, Value>) {
    let mut queue: Vec<Value> = set.iter().copied().collect();
    while let Some(v) = queue.pop() {
        if let Some(&b) = base_of.get(&v) {
            if set.insert(b) {
                queue.push(b);
            }
        }
    }
}

/// Mark a value live, and with it every base it was read out of.
fn mark_live(live: &mut HashSet<Value>, v: Value, base_of: &HashMap<Value, Value>) {
    let mut cur = Some(v);
    while let Some(x) = cur {
        if !live.insert(x) {
            break;
        }
        cur = base_of.get(&x).copied();
    }
}

fn insert_fn(f: &mut Func) {
    let ptr: HashSet<Value> = f.values().filter(|&v| f.value_repr(v).is_counted()).collect();
    if ptr.is_empty() {
        return;
    }
    let bases = base_of(f, &ptr);
    let live_out = liveness(f, &ptr, &bases);

    for b in &mut f.blocks {
        let mut live: HashSet<Value> = live_out[&b.id].clone();
        // Terminator operands: consuming uses (a returned/branched value is handed on).
        // They are already in `live_out` for values used by successors; a returned value
        // is consumed here, so mark it live so nothing releases it before the return.
        let mut term_uses = Vec::new();
        term_consuming(&b.term, &mut |v| {
            if ptr.contains(v) {
                term_uses.push(*v);
            }
        });
        for v in &term_uses {
            live.insert(*v);
        }

        let mut rev: Vec<Inst> = Vec::new();
        for inst in b.insts.iter().rev() {
            let mut releases_after: Vec<Value> = Vec::new();
            let mut retains_before: Vec<Value> = Vec::new();

            // A dead pointer result is dropped immediately.
            if let Some(v) = inst.result {
                if ptr.contains(&v) && !live.contains(&v) {
                    releases_after.push(v);
                }
                live.remove(&v);
            }

            let (consuming, borrowing) = operand_uses(&inst.op, &ptr);
            for w in borrowing {
                if !live.contains(&w) {
                    // Dead after this borrow: release it once the borrow has read it.
                    releases_after.push(w);
                    mark_live(&mut live, w, &bases);
                }
            }
            for w in consuming {
                if live.contains(&w) {
                    // Used again later, so this consume needs its own owned reference.
                    retains_before.push(w);
                } else {
                    mark_live(&mut live, w, &bases);
                }
            }

            // Emit in reverse-of-forward order; reversed below to `retains, inst, releases`.
            for v in releases_after {
                rev.push(Inst { result: None, op: Op::Release(v) });
            }
            rev.push(inst.clone());
            for v in retains_before {
                rev.push(Inst { result: None, op: Op::Retain(v) });
            }
        }
        rev.reverse();
        // A block parameter (a function parameter, or a value received on a jump) is an
        // owned reference. If it was never used, `live` no longer holds it: release it at
        // the top so it does not leak.
        let mut head: Vec<Inst> = b
            .params
            .iter()
            .filter(|p| ptr.contains(p) && !live.contains(p))
            .map(|&p| Inst { result: None, op: Op::Release(p) })
            .collect();
        head.extend(rev);
        b.insts = head;
    }
}

/// Live-out per block: the counted values a block's successors still need. Standard
/// backward dataflow; a block's parameters are definitions, not live-in.
fn liveness(f: &Func, ptr: &HashSet<Value>, base_of: &HashMap<Value, Value>) -> HashMap<super::ssa::BlockId, HashSet<Value>> {
    let mut live_in: HashMap<_, HashSet<Value>> = f.blocks.iter().map(|b| (b.id, HashSet::new())).collect();
    let mut live_out: HashMap<_, HashSet<Value>> = live_in.clone();

    loop {
        let mut changed = false;
        for b in f.blocks.iter().rev() {
            // live_out = union of successors' live_in, plus the args passed on jumps.
            let mut out = HashSet::new();
            for (succ, args) in successor_edges(&b.term) {
                out.extend(live_in[&succ].iter().copied());
                out.extend(args.into_iter().filter(|v| ptr.contains(v)));
            }
            term_consuming(&b.term, &mut |v| {
                if ptr.contains(v) {
                    out.insert(*v);
                }
            });

            // live_in = (out \ defs) ∪ uses.
            let mut defs: HashSet<Value> = b.params.iter().copied().collect();
            for inst in &b.insts {
                if let Some(v) = inst.result {
                    defs.insert(v);
                }
            }
            #[allow(unused_mut)]
            let mut ins: HashSet<Value> = out.iter().copied().filter(|v| !defs.contains(v)).collect();
            for inst in &b.insts {
                let (c, br) = operand_uses(&inst.op, ptr);
                for w in c.into_iter().chain(br) {
                    if !defs.contains(&w) {
                        ins.insert(w);
                    }
                }
            }

            with_bases(&mut out, base_of);
            with_bases(&mut ins, base_of);
            if out != live_out[&b.id] {
                live_out.insert(b.id, out);
                changed = true;
            }
            if ins != live_in[&b.id] {
                live_in.insert(b.id, ins);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    live_out
}

/// A pointer op's operands split into consuming and borrowing uses.
fn operand_uses(op: &Op, ptr: &HashSet<Value>) -> (Vec<Value>, Vec<Value>) {
    let mut consuming = Vec::new();
    let mut borrowing = Vec::new();
    match op {
        Op::Call { args, .. } | Op::Native { args, .. } | Op::MakeTuple(args) | Op::MakeList(args) => {
            consuming.extend(args.iter().copied())
        }
        Op::CallClosure { callee, args } => {
            consuming.push(*callee);
            consuming.extend(args.iter().copied());
        }
        Op::MakeClosure { captures, .. } => consuming.extend(captures.iter().copied()),
        Op::MakeRecord { fields, .. } => consuming.extend(fields.iter().map(|(_, v)| *v)),
        // Borrows: they read but do not take ownership.
        Op::Field { base, .. } | Op::Elem { base, .. } => borrowing.push(*base),
        Op::Index { base, index } => {
            borrowing.push(*base);
            borrowing.push(*index);
        }
        Op::Cast(v)
        | Op::IsNull(v)
        | Op::IsErr(v)
        | Op::UnwrapOk(v)
        | Op::UnwrapErr(v)
        | Op::IsVariant { value: v, .. } => borrowing.push(*v),
        Op::Prim(_, vs) => borrowing.extend(vs.iter().copied()),
        Op::Retain(_) | Op::Release(_) => {}
        _ => {}
    }
    consuming.retain(|v| ptr.contains(v));
    borrowing.retain(|v| ptr.contains(v));
    (consuming, borrowing)
}

/// A terminator's consuming operands (a returned, thrown, or branched value).
fn term_consuming(term: &Term, f: &mut impl FnMut(&Value)) {
    match term {
        Term::Ret(Some(v)) | Term::Throw(v) => f(v),
        Term::Jump(t) => t.args.iter().for_each(f),
        Term::Branch { then, els, .. } => {
            then.args.iter().for_each(&mut *f);
            els.args.iter().for_each(f);
        }
        Term::Switch { arms, default, .. } => {
            for (_, t) in arms {
                t.args.iter().for_each(&mut *f);
            }
            default.args.iter().for_each(f);
        }
        Term::Ret(None) | Term::Unreachable => {}
    }
}

fn successor_edges(term: &Term) -> Vec<(super::ssa::BlockId, Vec<Value>)> {
    match term {
        Term::Jump(t) => vec![(t.to, t.args.clone())],
        Term::Branch { then, els, .. } => {
            vec![(then.to, then.args.clone()), (els.to, els.args.clone())]
        }
        Term::Switch { arms, default, .. } => arms
            .iter()
            .map(|(_, t)| (t.to, t.args.clone()))
            .chain(std::iter::once((default.to, default.args.clone())))
            .collect(),
        Term::Ret(_) | Term::Throw(_) | Term::Unreachable => vec![],
    }
}

#[cfg(test)]
mod tests;
