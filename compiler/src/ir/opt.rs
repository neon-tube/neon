//! The optimiser: a pass pipeline over SSA, run to a fixpoint. See `docs/design/ir.md`.
//!
//! The always-on set is small and correctness-preserving: constant folding, dead-code
//! elimination (guided by the effect analysis, so an effectful instruction is never
//! dropped), and CFG cleanup (removing unreachable blocks). It is written so a further
//! pass is an addition, not a redesign.

use super::effects;
use super::ssa::{BlockId, Func, Op, PrimOp, Program, Term, Value};
use std::collections::{HashMap, HashSet};

/// Optimise every function in the program to a fixpoint.
pub fn optimize(program: &mut Program) {
    let pure = effects::analyze(program);
    for f in &mut program.funcs {
        loop {
            let a = const_fold(f);
            let b = dead_code(f, &pure);
            let c = simplify_cfg(f);
            let d = drop_unreachable_blocks(f);
            if !(a || b || c || d) {
                break;
            }
        }
    }
}

// ---- constant folding ----

/// Fold a primitive op on constant operands into a constant. Returns whether anything
/// changed. Overflow and divide-by-zero are left to the runtime, unfolded.
fn const_fold(f: &mut Func) -> bool {
    let mut ints: HashMap<Value, i64> = HashMap::new();
    let mut bools: HashMap<Value, bool> = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            match (inst.result, &inst.op) {
                (Some(v), Op::ConstI64(n)) => {
                    ints.insert(v, *n);
                }
                (Some(v), Op::ConstBool(x)) => {
                    bools.insert(v, *x);
                }
                _ => {}
            }
        }
    }

    let mut changed = false;
    for b in &mut f.blocks {
        for inst in &mut b.insts {
            if let Op::Prim(op, args) = &inst.op {
                if let Some(folded) = fold_prim(*op, args, &ints, &bools) {
                    inst.op = folded;
                    changed = true;
                }
            }
        }
    }
    changed
}

fn fold_prim(
    op: PrimOp,
    args: &[Value],
    ints: &HashMap<Value, i64>,
    bools: &HashMap<Value, bool>,
) -> Option<Op> {
    match (op, args) {
        (PrimOp::Neg, [a]) => ints.get(a).map(|n| Op::ConstI64(n.wrapping_neg())),
        (PrimOp::Not, [a]) => bools.get(a).map(|x| Op::ConstBool(!x)),
        (_, [a, b]) => {
            if let (Some(&x), Some(&y)) = (ints.get(a), ints.get(b)) {
                return fold_int(op, x, y);
            }
            if let (Some(&x), Some(&y)) = (bools.get(a), bools.get(b)) {
                return fold_bool(op, x, y);
            }
            None
        }
        _ => None,
    }
}

fn fold_int(op: PrimOp, x: i64, y: i64) -> Option<Op> {
    Some(match op {
        PrimOp::Add => Op::ConstI64(x.checked_add(y)?),
        PrimOp::Sub => Op::ConstI64(x.checked_sub(y)?),
        PrimOp::Mul => Op::ConstI64(x.checked_mul(y)?),
        PrimOp::Div => Op::ConstI64(x.checked_div(y)?),
        PrimOp::Rem => Op::ConstI64(x.checked_rem(y)?),
        PrimOp::Band => Op::ConstI64(x & y),
        PrimOp::Bor => Op::ConstI64(x | y),
        PrimOp::Bxor => Op::ConstI64(x ^ y),
        PrimOp::Eq => Op::ConstBool(x == y),
        PrimOp::Ne => Op::ConstBool(x != y),
        PrimOp::Lt => Op::ConstBool(x < y),
        PrimOp::Le => Op::ConstBool(x <= y),
        PrimOp::Gt => Op::ConstBool(x > y),
        PrimOp::Ge => Op::ConstBool(x >= y),
        _ => return None,
    })
}

fn fold_bool(op: PrimOp, x: bool, y: bool) -> Option<Op> {
    Some(match op {
        PrimOp::And => Op::ConstBool(x && y),
        PrimOp::Or => Op::ConstBool(x || y),
        PrimOp::Eq => Op::ConstBool(x == y),
        PrimOp::Ne => Op::ConstBool(x != y),
        _ => return None,
    })
}

// ---- dead-code elimination ----

/// Remove instructions whose result is unused and whose op is pure. Effectful
/// instructions stay even when their result is dead. Returns whether anything changed.
fn dead_code(f: &mut Func, pure: &HashMap<String, bool>) -> bool {
    let used = used_values(f);
    let mut changed = false;
    for b in &mut f.blocks {
        let before = b.insts.len();
        b.insts.retain(|inst| {
            let dead = inst.result.is_some_and(|v| !used.contains(&v));
            let removable = dead && !effects::op_is_effectful(&inst.op, pure);
            !removable
        });
        changed |= b.insts.len() != before;
    }
    changed
}

/// Every value read by an instruction operand, a terminator, or a branch's block args.
fn used_values(f: &Func) -> HashSet<Value> {
    let mut used = HashSet::new();
    let mut note = |v: &Value| {
        used.insert(*v);
    };
    for b in &f.blocks {
        for inst in &b.insts {
            op_operands(&inst.op, &mut note);
        }
        term_operands(&b.term, &mut note);
    }
    used
}

fn op_operands(op: &Op, f: &mut impl FnMut(&Value)) {
    match op {
        Op::Prim(_, vs) | Op::MakeTuple(vs) | Op::MakeList(vs) => vs.iter().for_each(f),
        Op::Call { args, .. } | Op::Native { args, .. } => args.iter().for_each(f),
        Op::CallClosure { callee, args } => {
            f(callee);
            args.iter().for_each(f);
        }
        Op::MakeClosure { captures, .. } => captures.iter().for_each(f),
        Op::MakeRecord { fields, .. } => fields.iter().for_each(|(_, v)| f(v)),
        Op::Field { base, .. } | Op::Elem { base, .. } => f(base),
        Op::Index { base, index } => {
            f(base);
            f(index);
        }
        Op::Cast(v)
        | Op::IsNull(v)
        | Op::IsErr(v)
        | Op::UnwrapOk(v)
        | Op::UnwrapErr(v)
        | Op::IsVariant { value: v, .. }
        | Op::Retain(v)
        | Op::Release(v) => f(v),
        Op::ConstI64(_)
        | Op::ConstF64(_)
        | Op::ConstBool(_)
        | Op::ConstStr(_)
        | Op::ConstNull
        | Op::ConstUnit
        | Op::ConstAtom(_) => {}
    }
}

fn term_operands(term: &Term, f: &mut impl FnMut(&Value)) {
    match term {
        Term::Ret(Some(v)) | Term::Throw(v) => f(v),
        Term::Ret(None) | Term::Unreachable => {}
        Term::Jump(t) => t.args.iter().for_each(f),
        Term::Branch { cond, then, els } => {
            f(cond);
            then.args.iter().for_each(&mut *f);
            els.args.iter().for_each(f);
        }
        Term::Switch { on, arms, default } => {
            f(on);
            for (_, t) in arms {
                t.args.iter().for_each(&mut *f);
            }
            default.args.iter().for_each(f);
        }
    }
}

// ---- simplify-CFG ----

/// Fold a constant branch into a jump, thread empty forwarding blocks, and merge a block
/// into its sole predecessor. Returns whether anything changed. Orphaned blocks are left
/// for `drop_unreachable_blocks`.
fn simplify_cfg(f: &mut Func) -> bool {
    let a = fold_const_branches(f);
    let b = thread_empty_jumps(f);
    let c = merge_single_pred(f);
    a || b || c
}

/// A `branch` on a constant condition becomes a `jump` to the taken side.
fn fold_const_branches(f: &mut Func) -> bool {
    let mut bools: HashMap<Value, bool> = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            if let (Some(v), Op::ConstBool(x)) = (inst.result, &inst.op) {
                bools.insert(v, *x);
            }
        }
    }
    let mut changed = false;
    for b in &mut f.blocks {
        if let Term::Branch { cond, then, els } = &b.term {
            if let Some(&x) = bools.get(cond) {
                let taken = if x { then.clone() } else { els.clone() };
                b.term = Term::Jump(taken);
                changed = true;
            }
        }
    }
    changed
}

/// Redirect edges that target an empty, parameter-less, argument-less forwarding block
/// straight to the block it forwards to.
fn thread_empty_jumps(f: &mut Func) -> bool {
    let mut fwd: HashMap<BlockId, BlockId> = HashMap::new();
    for b in &f.blocks {
        if b.id != f.entry && b.params.is_empty() && b.insts.is_empty() {
            if let Term::Jump(t) = &b.term {
                if t.args.is_empty() && t.to != b.id {
                    fwd.insert(b.id, t.to);
                }
            }
        }
    }
    if fwd.is_empty() {
        return false;
    }
    let mut changed = false;
    for b in &mut f.blocks {
        for t in targets_mut(&mut b.term) {
            let r = resolve_forward(&fwd, t.to);
            if r != t.to {
                t.to = r;
                changed = true;
            }
        }
    }
    changed
}

/// Follow a chain of forwarders to its end, stopping on a cycle.
fn resolve_forward(fwd: &HashMap<BlockId, BlockId>, mut id: BlockId) -> BlockId {
    let mut seen = HashSet::new();
    while let Some(&next) = fwd.get(&id) {
        if !seen.insert(id) {
            break;
        }
        id = next;
    }
    id
}

/// Fuse a block into its predecessor when that predecessor jumps to it unconditionally
/// and it has no other predecessor. Iterates so a chain collapses fully.
fn merge_single_pred(f: &mut Func) -> bool {
    let mut changed = false;
    loop {
        let preds = predecessor_counts(f);
        let mut fuse = None;
        for (ai, a) in f.blocks.iter().enumerate() {
            if let Term::Jump(t) = &a.term {
                if t.to != a.id && t.to != f.entry && preds.get(&t.to) == Some(&1) {
                    fuse = Some((ai, t.to, t.args.clone()));
                    break;
                }
            }
        }
        let Some((ai, b_id, args)) = fuse else { break };
        let bi = f.blocks.iter().position(|b| b.id == b_id).expect("target exists");

        // Substitute B's parameters with the arguments A passed, then splice B's body and
        // terminator into A. B is left unreachable for the DCE pass to remove.
        let subst: HashMap<Value, Value> =
            f.blocks[bi].params.iter().copied().zip(args).collect();
        let mut insts = f.blocks[bi].insts.clone();
        let mut term = f.blocks[bi].term.clone();
        for inst in &mut insts {
            rewrite_op(&mut inst.op, &subst);
        }
        rewrite_term(&mut term, &subst);
        f.blocks[ai].insts.extend(insts);
        f.blocks[ai].term = term;
        changed = true;
    }
    changed
}

fn predecessor_counts(f: &Func) -> HashMap<BlockId, usize> {
    let mut count: HashMap<BlockId, usize> = f.blocks.iter().map(|b| (b.id, 0)).collect();
    for b in &f.blocks {
        for s in successors(&b.term) {
            *count.entry(s).or_insert(0) += 1;
        }
    }
    count
}

fn targets_mut(term: &mut Term) -> Vec<&mut super::ssa::Target> {
    match term {
        Term::Jump(t) => vec![t],
        Term::Branch { then, els, .. } => vec![then, els],
        Term::Switch { arms, default, .. } => {
            arms.iter_mut().map(|(_, t)| t).chain(std::iter::once(default)).collect()
        }
        Term::Ret(_) | Term::Throw(_) | Term::Unreachable => vec![],
    }
}

fn rewrite_op(op: &mut Op, m: &HashMap<Value, Value>) {
    let sub = |v: &mut Value| {
        if let Some(&nv) = m.get(v) {
            *v = nv;
        }
    };
    match op {
        Op::Prim(_, vs) | Op::MakeTuple(vs) | Op::MakeList(vs) => vs.iter_mut().for_each(sub),
        Op::Call { args, .. } | Op::Native { args, .. } => args.iter_mut().for_each(sub),
        Op::CallClosure { callee, args } => {
            sub(callee);
            args.iter_mut().for_each(sub);
        }
        Op::MakeClosure { captures, .. } => captures.iter_mut().for_each(sub),
        Op::MakeRecord { fields, .. } => fields.iter_mut().for_each(|(_, v)| sub(v)),
        Op::Field { base, .. } | Op::Elem { base, .. } => sub(base),
        Op::Index { base, index } => {
            sub(base);
            sub(index);
        }
        Op::Cast(v)
        | Op::IsNull(v)
        | Op::IsErr(v)
        | Op::UnwrapOk(v)
        | Op::UnwrapErr(v)
        | Op::IsVariant { value: v, .. }
        | Op::Retain(v)
        | Op::Release(v) => sub(v),
        _ => {}
    }
}

fn rewrite_term(term: &mut Term, m: &HashMap<Value, Value>) {
    let sub = |v: &mut Value| {
        if let Some(&nv) = m.get(v) {
            *v = nv;
        }
    };
    match term {
        Term::Ret(Some(v)) | Term::Throw(v) => sub(v),
        Term::Jump(t) => t.args.iter_mut().for_each(sub),
        Term::Branch { cond, then, els } => {
            sub(cond);
            then.args.iter_mut().for_each(sub);
            els.args.iter_mut().for_each(sub);
        }
        Term::Switch { on, arms, default } => {
            sub(on);
            for (_, t) in arms {
                t.args.iter_mut().for_each(sub);
            }
            default.args.iter_mut().for_each(sub);
        }
        Term::Ret(None) | Term::Unreachable => {}
    }
}

// ---- CFG cleanup ----

/// Drop blocks unreachable from the entry, then renumber the survivors so a block's id
/// is again its index -- the invariant the accessors rely on -- remapping every
/// terminator target and the entry. Returns whether anything changed.
fn drop_unreachable_blocks(f: &mut Func) -> bool {
    let by_id: HashMap<BlockId, &_> = f.blocks.iter().map(|b| (b.id, b)).collect();
    let mut reachable = HashSet::new();
    let mut stack = vec![f.entry];
    while let Some(id) = stack.pop() {
        if !reachable.insert(id) {
            continue;
        }
        if let Some(b) = by_id.get(&id) {
            stack.extend(successors(&b.term));
        }
    }
    if reachable.len() == f.blocks.len() {
        return false;
    }
    drop(by_id);
    f.blocks.retain(|b| reachable.contains(&b.id));

    // Renumber to contiguous ids and remap every reference.
    let remap: HashMap<BlockId, BlockId> =
        f.blocks.iter().enumerate().map(|(i, b)| (b.id, BlockId(i as u32))).collect();
    for b in &mut f.blocks {
        b.id = remap[&b.id];
        for t in targets_mut(&mut b.term) {
            t.to = remap[&t.to];
        }
    }
    f.entry = remap[&f.entry];
    true
}

fn successors(term: &Term) -> Vec<BlockId> {
    match term {
        Term::Jump(t) => vec![t.to],
        Term::Branch { then, els, .. } => vec![then.to, els.to],
        Term::Switch { arms, default, .. } => {
            arms.iter().map(|(_, t)| t.to).chain(std::iter::once(default.to)).collect()
        }
        Term::Ret(_) | Term::Throw(_) | Term::Unreachable => vec![],
    }
}

#[cfg(test)]
mod tests;
