//! Which list values are provably sole-owned, so a write to them may happen in place.
//!
//! # The cost this exists to remove
//!
//! `list::set` is modelled as *consume a list, produce a list*, which is faithful to the
//! semantics and ruinous for the generated code: the result is a new SSA value, so across
//! every write the C compiler must discard what it knew about the old one. Measured on the
//! brainfuck benchmark's interpreter loop, that one fact costs three ways —
//!
//!   - reloading `l->data` on every iteration           14.7% of the profile
//!   - three bounds checks that cannot be hoisted       ~11%
//!   - the `rc == 1` test inside each write             ~3%
//!
//! — and they are not three problems. They are one: nothing about the list survives the
//! call, because the call might have returned a *different* list.
//!
//! # The property
//!
//! A list value is **sole-owned** at a point if no other live reference to it exists, in
//! which case mutating it in place is indistinguishable from copying it. That is exactly
//! the condition `neon_list_ensure_unique` tests at run time, once per write.
//!
//! This pass finds where the answer is a static yes, so the test can be hoisted out of the
//! loop that asks it — or rather, so uniqueness can be *established* once and then relied
//! on, which is the part that makes it work at all. See `Candidate`.
//!
//! # Why this is a query and not yet an optimisation
//!
//! Being wrong here does not crash; it silently mutates a list somebody else is holding.
//! So the pass reports and nothing consumes it, until the report has been read against a
//! real corpus and the guard has a test that fails loudly when it mis-fires. That
//! sequencing is deliberate — three separate hypotheses about this benchmark measured at
//! zero this week, and a wrong *optimisation* is worse than a wrong prediction.

use super::ssa::{BlockId, Func, Op, Program, Term, Value};
use super::repr::Repr;
use std::collections::{HashMap, HashSet};

/// A list value whose writes could become in-place, and the evidence for it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub func: String,
    /// The block whose parameter carries the list around the loop.
    pub header: BlockId,
    /// The loop-carried parameter itself.
    pub param: Value,
    /// How many `list::set` writes are on this value's chain inside the loop.
    pub writes: usize,
}

/// Every sole-ownership candidate in the program.
pub fn candidates(program: &Program) -> Vec<Candidate> {
    program.funcs.iter().flat_map(func_candidates).collect()
}

/// The blocks a block can jump to.
fn successors(f: &Func, b: BlockId) -> Vec<BlockId> {
    let block = &f.blocks[b.0 as usize];
    match &block.term {
        Term::Jump(t) => vec![t.to],
        Term::Branch { then, els, .. } => vec![then.to, els.to],
        Term::Switch { arms, default, .. } => {
            arms.iter().map(|(_, t)| t.to).chain(std::iter::once(default.to)).collect()
        }
        Term::Ret(_) | Term::Throw(_) | Term::Unreachable => vec![],
    }
}

/// Back edges, by depth-first search: an edge to a block already on the current path. A
/// back edge's target is a loop header, and that is all the loop structure this needs —
/// the pass never asks what the loop body *is*, only which values go round it.
fn back_edges(f: &Func) -> Vec<(BlockId, BlockId)> {
    let mut out = Vec::new();
    let mut on_path = HashSet::new();
    let mut done = HashSet::new();
    let mut stack = vec![(f.entry, 0usize)];
    on_path.insert(f.entry);
    while let Some((b, i)) = stack.pop() {
        let succs = successors(f, b);
        if i < succs.len() {
            stack.push((b, i + 1));
            let s = succs[i];
            if on_path.contains(&s) {
                out.push((b, s));
            } else if !done.contains(&s) {
                on_path.insert(s);
                stack.push((s, 0));
            }
        } else {
            on_path.remove(&b);
            done.insert(b);
        }
    }
    out
}

/// The `list::set` calls in a function, as (result, list argument). Matched by the name
/// lowering gives the stdlib function, so a user function called `set` is not mistaken
/// for it.
fn set_calls(f: &Func) -> Vec<(Value, Value)> {
    let mut out = Vec::new();
    for b in &f.blocks {
        for inst in &b.insts {
            if let Op::Call { func, args } = &inst.op {
                if func.starts_with("std__collections__list__set") && args.len() == 3 {
                    if let Some(r) = inst.result {
                        out.push((r, args[0]));
                    }
                }
            }
        }
    }
    out
}

/// How a value is used. The distinction that matters is between *dataflow* -- a value
/// moving to the next block, which creates no new reference -- and anything that could
/// leave a second reference behind.
#[derive(Debug, Clone)]
enum Use {
    /// Read by an instruction. Which one decides whether it escapes.
    By(Op),
    /// Passed as a block argument. This is SSA plumbing, not a new reference: the value
    /// is moving to the block parameter, and the parameter continues the same chain.
    /// Treating this as an escape is what made the first version of this pass report
    /// nothing at all -- the loop carrying a list round to the next iteration looked
    /// exactly like the list getting away.
    Carried { to: BlockId, slot: usize },
}

/// Every use of each value.
fn uses(f: &Func) -> HashMap<Value, Vec<Use>> {
    let mut out: HashMap<Value, Vec<Use>> = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            for v in operands(&inst.op) {
                out.entry(v).or_default().push(Use::By(inst.op.clone()));
            }
        }
        for (to, args) in targets_with_dest(&b.term) {
            for (slot, v) in args.iter().enumerate() {
                out.entry(*v).or_default().push(Use::Carried { to, slot });
            }
        }
    }
    out
}

/// Jump targets paired with the block they reach, so a carried value can be followed to
/// the parameter it becomes.
fn targets_with_dest(t: &Term) -> Vec<(BlockId, Vec<Value>)> {
    match t {
        Term::Jump(x) => vec![(x.to, x.args.clone())],
        Term::Branch { then, els, .. } => {
            vec![(then.to, then.args.clone()), (els.to, els.args.clone())]
        }
        Term::Switch { arms, default, .. } => arms
            .iter()
            .map(|(_, x)| (x.to, x.args.clone()))
            .chain(std::iter::once((default.to, default.args.clone())))
            .collect(),
        _ => vec![],
    }
}

/// The values an op reads. Exhaustive on purpose -- a `_` arm here would silently treat a
/// new operand-carrying op as reading nothing, which is the direction that turns a missed
/// escape into a wrong answer rather than a missed optimisation.
fn operands(op: &Op) -> Vec<Value> {
    match op {
        Op::Prim(_, vs) | Op::MakeTuple(vs) | Op::MakeList(vs) => vs.clone(),
        Op::Call { args, .. } | Op::Native { args, .. } | Op::MakeClosure { captures: args, .. } => {
            args.clone()
        }
        Op::CallClosure { callee, args } => {
            let mut v = vec![*callee];
            v.extend(args.iter().copied());
            v
        }
        Op::MakeRecord { fields, .. } => fields.iter().map(|(_, v)| *v).collect(),
        Op::Field { base, .. } | Op::Elem { base, .. } => vec![*base],
        Op::Index { base, index } => vec![*base, *index],
        Op::Cast(v)
        | Op::IsErr(v)
        | Op::UnwrapOk(v)
        | Op::UnwrapErr(v)
        | Op::IsNull(v)
        | Op::Retain(v)
        | Op::Release(v) => vec![*v],
        Op::IsVariant { value, .. } => vec![*value],
        Op::ConstI64(_)
        | Op::ConstF64(_)
        | Op::ConstBool(_)
        | Op::ConstStr(_)
        | Op::ConstNull
        | Op::ConstUnit
        | Op::ConstAtom(_) => vec![],
    }
}

fn func_candidates(f: &Func) -> Vec<Candidate> {
    let sets = set_calls(f);
    if sets.is_empty() {
        return Vec::new();
    }
    let all_uses = uses(f);
    let mut out = Vec::new();

    for (_, header) in back_edges(f) {
        for &param in &f.blocks[header.0 as usize].params {
            if !matches!(f.value_repr(param), Repr::List(_)) {
                continue;
            }
            if let Some(writes) = walk_chain(f, &sets, &all_uses, header, param) {
                if writes > 0 {
                    out.push(Candidate { func: f.name.clone(), header, param, writes });
                }
            }
        }
    }
    out
}

/// Follow one list value round its loop, counting the writes on it, and give up the moment
/// anything could leave a second reference behind.
///
/// The walk moves through three kinds of link, and keeping them apart is the whole
/// difficulty: a *write* consumes the value and produces the next one; a *carry* moves it
/// to a block parameter, which continues the same chain and is not a new reference; and
/// the throwing wrapper's tagged result has to be stepped over, since `set` produces a
/// union that `unwrap_ok` opens rather than the list directly.
///
/// `None` means the value escapes. Returning `Some(0)` -- reachable, since a loop may
/// carry a list it only reads -- is not a candidate, and the caller drops it.
fn walk_chain(
    f: &Func,
    sets: &[(Value, Value)],
    all_uses: &HashMap<Value, Vec<Use>>,
    header: BlockId,
    param: Value,
) -> Option<usize> {
    let mut cur = param;
    let mut writes = 0usize;
    let mut seen = HashSet::new();

    loop {
        if !seen.insert(cur) {
            // Back at a value already walked: the chain closes, which is what a loop
            // carrying a list looks like.
            return Some(writes);
        }
        for u in all_uses.get(&cur).into_iter().flatten() {
            match u {
                // Reading an element or a length leaves no reference behind.
                Use::By(Op::Index { .. }) => {}
                Use::By(Op::Native { symbol, .. }) if is_read_only_list_native(symbol) => {}
                // The consuming write is the chain's next link, handled below.
                Use::By(Op::Call { func, .. })
                    if func.starts_with("std__collections__list__set") => {}
                // Opening the write's tagged result, likewise.
                Use::By(Op::IsErr(_)) | Use::By(Op::UnwrapOk(_)) | Use::By(Op::UnwrapErr(_)) => {}
                // A retain is exactly a second reference. So is anything else that takes
                // the value: another call, a closure capture, a record field, a cast.
                Use::By(_) => return None,
                // Moving to a block parameter is dataflow. It continues the chain rather
                // than duplicating it, so it is allowed here and followed below.
                Use::Carried { .. } => {}
            }
        }

        // The next link: the write that consumes this value, stepping over the tagged
        // result it produces.
        if let Some((result, _)) = sets.iter().find(|(_, arg)| *arg == cur) {
            writes += 1;
            cur = unwrapped(f, *result).unwrap_or(*result);
            continue;
        }
        // Otherwise follow the carry back to the loop header, if that is where it goes.
        match carried_to_header(all_uses, cur, header, f) {
            Some(next) if next != cur => cur = next,
            _ => return Some(writes),
        }
    }
}

/// The list a throwing `set`'s tagged result yields, i.e. what `unwrap_ok` opens it to.
fn unwrapped(f: &Func, tagged: Value) -> Option<Value> {
    for b in &f.blocks {
        for inst in &b.insts {
            if let (Some(r), Op::UnwrapOk(v)) = (inst.result, &inst.op) {
                if *v == tagged {
                    return Some(r);
                }
            }
        }
    }
    None
}

/// If this value is carried into the loop header, the parameter it becomes there.
fn carried_to_header(
    all_uses: &HashMap<Value, Vec<Use>>,
    v: Value,
    header: BlockId,
    f: &Func,
) -> Option<Value> {
    for u in all_uses.get(&v).into_iter().flatten() {
        if let Use::Carried { to, slot } = u {
            if *to == header {
                return f.blocks[to.0 as usize].params.get(*slot).copied();
            }
        }
    }
    None
}

/// List natives that only read. Everything else is treated as possibly retaining.
fn is_read_only_list_native(symbol: &str) -> bool {
    matches!(symbol, "neon_list_at" | "neon_list_at_scalar" | "neon_list_len")
}

// ---- diagnostics, for the reporting harness ----

/// The `list::set` calls this pass recognises, for `unique_report`'s debug mode.
pub fn debug_sets(f: &Func) -> Vec<(Value, Value)> {
    set_calls(f)
}

/// The back edges this pass finds, for `unique_report`'s debug mode.
pub fn debug_back_edges(f: &Func) -> Vec<(BlockId, BlockId)> {
    back_edges(f)
}
