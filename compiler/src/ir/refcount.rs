//! Reference-count insertion, Perceus-style: last-use-driven. See `docs/design/ir.md`.
//!
//! Every counted value is one of two kinds, and the whole pass follows from the split:
//!
//! - an **owner** holds exactly one reference from the moment it is produced — a call or
//!   native result, an aggregate, an `Index` read (codegen retains what it hands back), a
//!   block parameter (ownership moves in with the argument);
//! - a **view** holds nothing. `Field`, `Elem`, `Cast`, `UnwrapOk` and `UnwrapErr` hand
//!   back a look into what their operand owns; `base_of` records the derivation, and
//!   `root` follows it to the owner at the bottom of the chain.
//!
//! Liveness is computed **over roots**: a use of a view is a use of its root, and views
//! never appear in a live set. That one collapse is what lets a single analysis place
//! every retain and release — the previous design tracked views and roots separately and
//! needed a base-extension step (`with_bases`) that marked a root live wherever its views
//! were, which made "release the root once the last view dies" unreachable exactly when
//! a view was consumed at a terminator.
//!
//! Placement, in full:
//!
//! 1. A consuming use (call/native argument, a value stored into an aggregate or closure)
//!    of an owner that is live afterwards is preceded by `retain`; at its last use the
//!    reference **moves** instead. A consuming use of a view is always preceded by
//!    `retain` — it must materialise a reference for whoever takes it.
//! 2. An owner is released immediately after its last use; a dead result immediately.
//! 3. Terminators consume, and their bookkeeping sits **on the edge**: for each CFG edge,
//!    retain the views passed as block arguments, retain an owner argument once per use
//!    beyond the reference it moves, then release every owner live at the terminator that
//!    is neither moved along the edge nor live into the successor. A `jump`'s single edge
//!    is the end of its block; a `branch`/`switch` edge that needs code gets a fresh
//!    block on that edge, so nothing fires on a path that was not taken. `ret`/`throw`
//!    place the same code before the terminator — which is where the root of a returned
//!    view dies.
//! 4. A block parameter never used is released at the top of its block.
//! 5. A lambda's environment parameter is **borrowed** — the closure value owns it and
//!    may be called again — so it is excluded from every release. `CallClosure` likewise
//!    borrows its callee: calling a closure reads it, it does not destroy it.
//!
//! Because the language is immutable, values are acyclic and this is complete: the last
//! release always runs, and nothing leaks. Moves at last use and `rc == 1` reuse are the
//! refinements the optimiser adds on top; this establishes the balanced baseline.

use super::ssa::{Block, BlockId, Func, Inst, Op, Program, Target, Term, Value};
use std::collections::{HashMap, HashSet};

pub fn insert(program: &mut Program) {
    for f in &mut program.funcs {
        insert_fn(f);
    }
}

/// Where a view was read out of: `Field`/`Elem`/`Cast`/`UnwrapOk`/`UnwrapErr` results
/// alias what their operand owns rather than holding a reference of their own. `Index` is
/// not here — `emit_index` retains what it reads, so that result owns itself.
fn base_of(f: &Func, ptr: &HashSet<Value>) -> HashMap<Value, Value> {
    let mut out = HashMap::new();
    for b in &f.blocks {
        for inst in &b.insts {
            let projected = match (inst.result, &inst.op) {
                (Some(v), Op::Field { base, .. } | Op::Elem { base, .. }) => Some((v, *base)),
                (Some(v), Op::Cast(base) | Op::UnwrapOk(base) | Op::UnwrapErr(base)) => {
                    Some((v, *base))
                }
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

/// The owner at the bottom of a projection chain — what actually holds the storage.
fn root_base(base_of: &HashMap<Value, Value>, v: Value) -> Value {
    let mut cur = v;
    while let Some(&b) = base_of.get(&cur) {
        cur = b;
    }
    cur
}

/// Everything a block's instructions and terminator will need done, computed against the
/// immutable function before any block is rewritten. `edges` parallels
/// `successor_edges`; for `ret`/`throw` it is a single entry of pre-terminator code.
struct Plan {
    body: Vec<Inst>,
    edges: Vec<Vec<Inst>>,
}

fn insert_fn(f: &mut Func) {
    let ptr: HashSet<Value> = f.values().filter(|&v| f.value_repr(v).is_counted()).collect();
    if ptr.is_empty() {
        return;
    }
    let bases = base_of(f, &ptr);
    // A lifted lambda's environment parameter is borrowed: the closure value owns it and
    // may be called again. It stays in `ptr` so reads out of it are still views.
    let env_param: Option<Value> = f.env.is_some().then(|| f.params[0]);
    let (live_in, live_out) = liveness(f, &ptr, &bases);

    let release = |v: Value| Inst { result: None, op: Op::Release(v) };
    let retain = |v: Value| Inst { result: None, op: Op::Retain(v) };

    let mut plans: Vec<Plan> = Vec::with_capacity(f.blocks.len());
    for b in &f.blocks {
        // Owners the terminator itself needs alive: everything live out, plus the roots
        // of every operand the terminator reads or hands on.
        let mut live: HashSet<Value> = live_out[&b.id].clone();
        term_operands(&b.term, &mut |v| {
            if ptr.contains(&v) {
                live.insert(root_base(&bases, v));
            }
        });
        let mut term_live: Vec<Value> = live.iter().copied().collect();
        term_live.sort();

        // The edge code. Retains strictly before releases: a release may free the root
        // of the view a retain just materialised from.
        let edges: Vec<Vec<Inst>> = match &b.term {
            Term::Ret(_) | Term::Throw(_) => {
                let v = match &b.term {
                    Term::Ret(v) => *v,
                    Term::Throw(v) => Some(*v),
                    _ => unreachable!(),
                };
                let mut code = Vec::new();
                let mut moved = HashSet::new();
                if let Some(v) = v.filter(|v| ptr.contains(v)) {
                    if bases.contains_key(&v) {
                        code.push(retain(v));
                    } else {
                        moved.insert(v);
                    }
                }
                code.extend(
                    term_live
                        .iter()
                        .filter(|w| !moved.contains(w) && Some(**w) != env_param)
                        .map(|&w| release(w)),
                );
                vec![code]
            }
            Term::Unreachable => vec![],
            _ => successor_edges(&b.term)
                .into_iter()
                .map(|(succ, args)| {
                    let succ_live = &live_in[&succ];
                    let mut code = Vec::new();
                    // Each owner argument moves one reference; every use past that, and
                    // surviving into the successor besides, needs a retain. A view
                    // argument always materialises its own.
                    let mut owner_uses: HashMap<Value, usize> = HashMap::new();
                    for &a in args.iter().filter(|a| ptr.contains(a)) {
                        if bases.contains_key(&a) {
                            code.push(retain(a));
                        } else {
                            *owner_uses.entry(a).or_insert(0) += 1;
                        }
                    }
                    let mut moved: Vec<Value> = owner_uses.keys().copied().collect();
                    moved.sort();
                    for &w in &moved {
                        let needs = owner_uses[&w] + usize::from(succ_live.contains(&w));
                        code.extend((1..needs).map(|_| retain(w)));
                    }
                    code.extend(
                        term_live
                            .iter()
                            .filter(|w| {
                                !owner_uses.contains_key(w)
                                    && !succ_live.contains(w)
                                    && Some(**w) != env_param
                            })
                            .map(|&w| release(w)),
                    );
                    code
                })
                .collect(),
        };

        // The backward walk over the body, against liveness at the terminator.
        let mut rev: Vec<Inst> = Vec::new();
        for inst in b.insts.iter().rev() {
            let mut retains: Vec<Value> = Vec::new();
            let mut releases: Vec<Value> = Vec::new();

            if let Some(v) = inst.result {
                // A dead owner result is dropped immediately. A view result owns
                // nothing and needs nothing — its root's liveness is handled below,
                // where the projection borrows it.
                if ptr.contains(&v) && !bases.contains_key(&v) && !live.remove(&v) {
                    releases.push(v);
                }
            }

            let (consuming, borrowing) = operand_uses(&inst.op, &ptr);
            for w in consuming {
                if bases.contains_key(&w) {
                    // A consumed view materialises a reference, and counts as a use of
                    // its root — if this was the root's last use, it dies here.
                    retains.push(w);
                    let r = root_base(&bases, w);
                    if live.insert(r) && Some(r) != env_param {
                        releases.push(r);
                    }
                } else if live.contains(&w) {
                    retains.push(w);
                } else {
                    live.insert(w);
                }
            }
            for w in borrowing {
                let r = root_base(&bases, w);
                if live.insert(r) && Some(r) != env_param {
                    releases.push(r);
                }
            }

            // Emit in reverse-of-forward order; reversed below to `retains, inst, releases`.
            rev.extend(releases.into_iter().map(release));
            rev.push(inst.clone());
            rev.extend(retains.into_iter().map(retain));
        }
        rev.reverse();

        // A parameter is an owned reference; never used means released at the top.
        let mut body: Vec<Inst> = b
            .params
            .iter()
            .filter(|p| ptr.contains(p) && !live.contains(p) && Some(**p) != env_param)
            .map(|&p| release(p))
            .collect();
        body.extend(rev);
        plans.push(Plan { body, edges });
    }

    // Apply: bodies in place; jump/ret/throw code at the end of the block; a
    // branch/switch edge that needs code gets its own block, so the retain or release
    // sits on the path actually taken.
    let mut next_id = f.blocks.len() as u32;
    let mut edge_blocks: Vec<Block> = Vec::new();
    let mut split = |target: &mut Target, code: Vec<Inst>| {
        if code.is_empty() {
            return;
        }
        let id = BlockId(next_id);
        next_id += 1;
        edge_blocks.push(Block {
            id,
            params: vec![],
            insts: code,
            term: Term::Jump(Target { to: target.to, args: std::mem::take(&mut target.args) }),
        });
        *target = Target { to: id, args: vec![] };
    };
    for (b, plan) in f.blocks.iter_mut().zip(plans) {
        b.insts = plan.body;
        let mut edges = plan.edges.into_iter();
        match &mut b.term {
            Term::Ret(_) | Term::Throw(_) | Term::Jump(_) => {
                b.insts.extend(edges.next().unwrap_or_default());
            }
            Term::Branch { then, els, .. } => {
                split(then, edges.next().unwrap_or_default());
                split(els, edges.next().unwrap_or_default());
            }
            Term::Switch { arms, default, .. } => {
                for (_, t) in arms.iter_mut() {
                    split(t, edges.next().unwrap_or_default());
                }
                split(default, edges.next().unwrap_or_default());
            }
            Term::Unreachable => {}
        }
    }
    f.blocks.extend(edge_blocks);
}

/// Root-collapsed liveness: live sets hold owners only, and a use of a view marks its
/// root. Standard backward dataflow; block parameters are definitions, not live-in.
#[allow(clippy::type_complexity)]
fn liveness(
    f: &Func,
    ptr: &HashSet<Value>,
    bases: &HashMap<Value, Value>,
) -> (HashMap<BlockId, HashSet<Value>>, HashMap<BlockId, HashSet<Value>>) {
    let mut live_in: HashMap<_, HashSet<Value>> =
        f.blocks.iter().map(|b| (b.id, HashSet::new())).collect();
    let mut live_out: HashMap<_, HashSet<Value>> = live_in.clone();

    loop {
        let mut changed = false;
        for b in f.blocks.iter().rev() {
            let mut out: HashSet<Value> = HashSet::new();
            for (succ, args) in successor_edges(&b.term) {
                out.extend(live_in[&succ].iter().copied());
                out.extend(
                    args.iter().filter(|v| ptr.contains(v)).map(|&v| root_base(bases, v)),
                );
            }

            let mut live = out.clone();
            term_operands(&b.term, &mut |v| {
                if ptr.contains(&v) {
                    live.insert(root_base(bases, v));
                }
            });
            for inst in b.insts.iter().rev() {
                if let Some(v) = inst.result {
                    if !bases.contains_key(&v) {
                        live.remove(&v);
                    }
                }
                let (c, br) = operand_uses(&inst.op, ptr);
                for w in c.into_iter().chain(br) {
                    live.insert(root_base(bases, w));
                }
            }
            for p in &b.params {
                live.remove(p);
            }

            if out != live_out[&b.id] {
                live_out.insert(b.id, out);
                changed = true;
            }
            if live != live_in[&b.id] {
                live_in.insert(b.id, live);
                changed = true;
            }
        }
        if !changed {
            return (live_in, live_out);
        }
    }
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
            borrowing.push(*callee);
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

/// Every value a terminator reads or hands on: block arguments, a returned or thrown
/// value, and the scrutinee of a `branch` or `switch` (a borrow — it must survive to the
/// terminator, and each edge decides whether it survives past it).
fn term_operands(term: &Term, f: &mut impl FnMut(Value)) {
    match term {
        Term::Ret(Some(v)) | Term::Throw(v) => f(*v),
        Term::Jump(t) => t.args.iter().for_each(|&v| f(v)),
        Term::Branch { cond, then, els } => {
            f(*cond);
            then.args.iter().for_each(|&v| f(v));
            els.args.iter().for_each(|&v| f(v));
        }
        Term::Switch { on, arms, default } => {
            f(*on);
            for (_, t) in arms {
                t.args.iter().for_each(|&v| f(v));
            }
            default.args.iter().for_each(|&v| f(v));
        }
        Term::Ret(None) | Term::Unreachable => {}
    }
}

fn successor_edges(term: &Term) -> Vec<(BlockId, Vec<Value>)> {
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
