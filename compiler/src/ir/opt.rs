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
///
/// The purity analysis is run once, over the *unoptimised* program, and reused for every
/// function. That is sound because the passes here only remove work: a function that was
/// pure before cannot become effectful, and one that was effectful can only lose effects,
/// so the answer stays conservative in the safe direction (see `effects`).
///
/// Each function is driven to its own fixpoint rather than each pass to the program's,
/// because the passes feed one another: folding a branch condition orphans blocks, which
/// makes a block single-predecessor, which exposes more constants.
pub fn optimize(program: &mut Program) {
    let pure = effects::analyze(program);
    let pure_natives = program.pure_natives.clone();
    for f in &mut program.funcs {
        loop {
            let a = const_fold(f);
            let b = dead_code(f, &pure, &pure_natives);
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

/// The constant, if both operands are known constants of one kind. Integers are tried
/// before booleans, which is unambiguous only because no value is in both maps: a `Value`
/// is defined once, by either a `ConstI64` or a `ConstBool`.
///
/// A missing entry means "not known constant", not "not foldable", so partially-constant
/// ops fall through untouched and get another chance on the next fixpoint round once the
/// other operand has folded.
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

/// Fold an integer op, or decline.
///
/// The arithmetic arms use `checked_*` and propagate the `None` with `?`. What that buys
/// differs per op, and the two cases must not be conflated:
///
/// - `Div`/`Rem` genuinely **trap** at runtime, on a zero divisor and on `i64::MIN / -1`
///   (`runtime/src/arith.c`). Declining is load-bearing: folding would replace a trap with
///   a value, or abort the *compiler* on code that never runs. `checked_div`/`checked_rem`
///   cover both cases, not just the zero divisor.
/// - `Add`/`Sub`/`Mul` **wrap** at runtime — two's complement, no trap, per
///   `docs/decisions.md` and the `neon_i64_*` unsigned round-trip. So declining on overflow
///   changes nothing observable; the unfolded instruction computes the same wrapped value.
///   It is a missed fold, not a preserved trap.
///
/// This distinction is written out because the comment that used to sit here said the
/// unfolded instruction survives "where the trapping semantics apply", which is true only
/// of `Div`/`Rem`. `effects::op_is_effectful` carried the mirror-image error. Two passes
/// disagreeing about which ops trap is how `xs[10]` and `1 / 0` got deleted in the first
/// place, so both sides now name the runtime file that settles it.
///
/// `Neg` does not come through here: `fold_prim` handles it with `wrapping_neg`, which is
/// exactly what `neon_i64_neg` does, so `-i64::MIN` folds to `i64::MIN` and agrees with the
/// runtime. Were `Neg` ever made to trap, that fold would start eliminating the trap and
/// would have to become `checked_neg`.
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

/// Fold a boolean op, or decline.
///
/// `And`/`Or` here are the strict bitwise-style primitives on two already-computed
/// operands. Source-level `and`/`or` short-circuit and are lowered to control flow, not to
/// these, so folding both operands eagerly cannot evaluate something the program would
/// have skipped.
fn fold_bool(op: PrimOp, x: bool, y: bool) -> Option<Op> {
    Some(match op {
        PrimOp::And => Op::ConstBool(x && y),
        PrimOp::Or => Op::ConstBool(x || y),
        PrimOp::Eq => Op::ConstBool(x == y),
        PrimOp::Ne => Op::ConstBool(x != y),
        _ => return None,
    })
}

/// The folding helpers, exposed for the Kani proofs in `verify/`.
///
/// Not API: these are private to the pass, and the only reason they are reachable at all is
/// that a proof harness lives outside `src/` and so can only see `pub` items. `fold_int` is
/// where the claim in its own doc comment — that a fold agrees with `runtime/src/arith.c`
/// for *every* operand pair — is actually discharged, and that is worth three lines of
/// widened surface. Nothing in the compiler should call through here.
///
/// Thin wrappers rather than a `pub use`, so the folders themselves stay private: a
/// re-export would have to widen them to `pub`, and then anything in the compiler could
/// reach them. These forward and nothing else.
#[doc(hidden)]
pub mod verify {
    use super::{Op, PrimOp};

    pub fn fold_int(op: PrimOp, x: i64, y: i64) -> Option<Op> {
        super::fold_int(op, x, y)
    }

    pub fn fold_bool(op: PrimOp, x: bool, y: bool) -> Option<Op> {
        super::fold_bool(op, x, y)
    }
}

// ---- dead-code elimination ----

/// Remove instructions whose result is unused and whose op is pure. Effectful
/// instructions stay even when their result is dead. Returns whether anything changed.
fn dead_code(
    f: &mut Func,
    pure: &HashMap<String, bool>,
    pure_natives: &HashSet<String>,
) -> bool {
    let used = used_values(f);

    // Decide first, then mutate. The effect test reads operand reprs out of `f` — an i64
    // `+` can trap where the f64 one cannot — and `retain` holds a mutable borrow, so the
    // two cannot overlap.
    let keep: Vec<Vec<bool>> = f
        .blocks
        .iter()
        .map(|b| {
            b.insts
                .iter()
                .map(|inst| {
                    let dead = inst.result.is_some_and(|v| !used.contains(&v));
                    !(dead && !effects::op_is_effectful(f, &inst.op, pure, pure_natives))
                })
                .collect()
        })
        .collect();

    let mut changed = false;
    for (b, keep) in f.blocks.iter_mut().zip(keep) {
        let before = b.insts.len();
        let mut keep = keep.into_iter();
        b.insts.retain(|_| keep.next().unwrap_or(true));
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

/// Call `f` on every value an op reads.
///
/// Deliberately exhaustive — the constant ops are spelled out instead of falling into a
/// wildcard — because this is the input to DCE's liveness. An operand missing from here
/// reads as unused, and a still-live pure instruction gets deleted out from under its
/// consumer. A new `Op` should therefore fail to compile until it is listed.
///
/// `rewrite_op` walks the same shape for substitution but does have a wildcard, so the two
/// are not interchangeable: adding an operand-carrying op needs both updated by hand.
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

/// Call `f` on every value a terminator reads: the returned or thrown value, a branch or
/// switch scrutinee, and the arguments on every outgoing edge.
///
/// Block arguments count as uses even though the parameter they feed may itself be dead —
/// dropping an argument would desynchronise the edge from the successor's parameter list,
/// which is an arity mismatch rather than a missed optimisation.
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
        //
        // The substitution has to run over the *whole function*, not just the code being
        // spliced: B's parameters stop existing once B is merged away, and any block after
        // it that still reads one would refer to a value nothing defines. That produced a
        // program that silently computed with an uninitialised local.
        let subst: HashMap<Value, Value> =
            f.blocks[bi].params.iter().copied().zip(args).collect();
        let insts = f.blocks[bi].insts.clone();
        let term = f.blocks[bi].term.clone();
        f.blocks[ai].insts.extend(insts);
        f.blocks[ai].term = term;
        f.blocks[bi].params.clear();
        for b in &mut f.blocks {
            for inst in &mut b.insts {
                rewrite_op(&mut inst.op, &subst);
            }
            rewrite_term(&mut b.term, &subst);
        }
        changed = true;
    }
    changed
}

/// How many CFG *edges* target each block — not how many distinct predecessors it has.
///
/// The distinction is what makes `merge_single_pred` safe. A `branch` or `switch` whose
/// arms land on the same block has one predecessor but two or more edges; counting
/// predecessors would call that block mergeable, and splicing it into a terminator that is
/// not an unconditional jump would drop the other paths. A block with no
/// in-edges is still present in the map, at 0, because the map is seeded from `f.blocks`.
fn predecessor_counts(f: &Func) -> HashMap<BlockId, usize> {
    let mut count: HashMap<BlockId, usize> = f.blocks.iter().map(|b| (b.id, 0)).collect();
    for b in &f.blocks {
        for s in successors(&b.term) {
            *count.entry(s).or_insert(0) += 1;
        }
    }
    count
}

/// Every outgoing edge of a terminator, mutably — the write counterpart of `successors`,
/// for passes that redirect an edge in place rather than rebuild the terminator. It yields
/// one entry per edge, so a `branch` with both arms on one block appears twice and both
/// copies get rewritten.
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

/// Substitute operands in place, for the parameter-to-argument mapping `merge_single_pred`
/// builds. One pass, no chasing: `m` maps a merged block's parameters to the arguments
/// that flowed in, and `merge_single_pred` applies it across the whole function before
/// looking for the next merge, so a chain never needs a transitive lookup here.
///
/// Exhaustive, like `op_operands`, and for a sharper reason than that one. An `Op` missing
/// an arm here is left *unsubstituted*: it keeps naming a parameter of the block that was
/// just merged away, which nothing defines any more. That is the "silently computed with an
/// uninitialised local" failure `merge_single_pred` documents, reintroduced by an omission
/// no reader would notice. It used to end in a wildcard, and the doc comment said so
/// instead of fixing it.
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
        Op::ConstI64(_)
        | Op::ConstF64(_)
        | Op::ConstBool(_)
        | Op::ConstStr(_)
        | Op::ConstNull
        | Op::ConstUnit
        | Op::ConstAtom(_) => {}
    }
}

/// `rewrite_op`'s counterpart for terminators. Block arguments are substituted as well as
/// the scrutinee: a merged block's parameter can be passed straight on down an edge, and
/// leaving that reference behind is exactly the dangling-value bug `merge_single_pred`
/// documents.
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

/// The blocks a terminator can transfer to. `ret`, `throw` and `unreachable` have none,
/// which is what bounds the reachability walk in `drop_unreachable_blocks`. Duplicates are
/// not removed — `predecessor_counts` relies on one entry per edge.
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
