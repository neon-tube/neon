//! Reference-count insertion, Perceus-style: last-use-driven. See `docs/design/ir.md`.
//!
//! Every counted value is one of two kinds, and the whole pass follows from the split:
//!
//! - an **owner** holds exactly one reference from the moment it is produced — a call or
//!   native result, an aggregate, an `Index` read (codegen retains what it hands back), a
//!   block parameter (ownership moves in with the argument);
//! - a **view** holds nothing. `Field`, `Elem`, `Cast`, `UnwrapOk` and `UnwrapErr` hand
//!   back a look into what their operand owns; `base_of` records the derivation, and
//!   `root` follows it to the owner at the bottom of the chain. The single exception is
//!   a `Cast` that erases into `any`: it allocates a box and takes its operand's
//!   reference, so it is an owner and a consuming use — see `erasing_casts`.
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

use super::repr::Repr;
use super::ssa::{Block, BlockId, Func, Inst, Op, Program, Target, Term, Value};
use std::collections::{HashMap, HashSet};

/// Insert retains and releases across the whole program, in place.
///
/// This is the last IR pass and has to be: the placement is computed against a specific
/// CFG, and it adds blocks of its own to split edges. Anything that reshapes control flow
/// afterwards — threading a jump, merging a block into its predecessor — would move a
/// release onto a path that does not own the value, or off the path that does. Running it
/// twice would likewise double every count; it is not idempotent.
///
/// Functions are independent, since ownership is transferred at call boundaries by
/// convention rather than inferred across them.
pub fn insert(program: &mut Program) {
    for f in &mut program.funcs {
        insert_fn(f);
    }
}

/// The results of the one `Op::Cast` that is not a projection: erasing into `any`.
///
/// Every other cast reinterprets storage that already exists — narrowing a union reads a
/// payload, widening injects into a tagged struct, an `any` -> concrete recovery points
/// into the box. Erasure alone *allocates*: `coerce_expr` emits `neon_box_new`, which
/// `neon_alloc`s a fresh header and `memcpy`s the payload in without retaining it. So the
/// result is an owner, and the operand's reference **moves into** the box, whose
/// `neon_box_drop` releases it via the witness.
///
/// Treating erasure as a view (which is what listing `Cast` unconditionally in `base_of`
/// did) made the pass release the *record* and never the box: a leak of the box and
/// everything it transitively owned. It was invisible for a flat record because an inline
/// `Repr::Record` with no counted field is not in `ptr` at all, so `base_of`'s
/// `ptr.contains(&base)` guard already rejected the edge and the cast was an owner by
/// accident. A recursive record is a `Repr::BoxedRec` pointer, is counted, and took the
/// view path.
///
/// `Any -> Any` is the identity in `coerce_expr` and stays a view; so does a `Never`
/// source, which never carries a live reference.
///
/// Two edges of this test are worth stating because they are the ones a future change
/// would get wrong.
///
/// **`Never -> Any` is classified neither way, and that is only safe because it cannot
/// run.** Excluded here, it is not an erasing owner; and `base_of` will not make it a view
/// either, because `Repr::Never` is not `is_counted` and so the operand is not in `ptr`.
/// The result is therefore treated as a *fresh owner* while `coerce_expr` emits the
/// identity — no box — so the pass would release a value nothing allocated. It is
/// unreachable rather than fixed: a value of repr `Never` is one an expression never
/// produces, so the only shape that mints one is `unwrap_ok` of a call to a
/// `-> never throws E` function, in the block reached when that call returned *Ok*. It
/// never did. `neon ir` shows the block; nothing reaches it. Anything that gives `Never`
/// a reachable value has to revisit this.
///
/// **This test reads reprs raw where `coerce_expr` reads them through `types.resolve`.**
/// That is the file's one hand-kept agreement with the backend, and it cannot be closed
/// here — resolving a `Repr::Recursive` back-edge needs the `TypeTable`, which is the
/// emitter's and not the IR's. The gap is exactly a `mu` type whose unfolding *is* `any`,
/// where the backend would box and this test would call it a view: the original bug,
/// re-entered through the back door. No such type is constructible today (a `mu` alias for
/// `any` is not contractive and the checker rejects it), which is why this is a comment and
/// not a guard. If `Repr::Recursive` ever reaches here for a type that unfolds to `Any`,
/// the leak is back.
fn erasing_casts(f: &Func) -> HashSet<Value> {
    let mut out = HashSet::new();
    for b in &f.blocks {
        for inst in &b.insts {
            if let (Some(v), Op::Cast(base)) = (inst.result, &inst.op) {
                let src = f.value_repr(*base);
                if matches!(f.value_repr(v), Repr::Any)
                    && !matches!(src, Repr::Any | Repr::Never)
                {
                    out.insert(v);
                }
            }
        }
    }
    out
}

/// Where a view was read out of: `Field`/`Elem`/`Cast`/`UnwrapOk`/`UnwrapErr` results
/// alias what their operand owns rather than holding a reference of their own. `Index` is
/// not here — `emit_index` retains what it reads, so that result owns itself. Nor is an
/// erasing cast, which allocates a box rather than aliasing (see `erasing_casts`).
fn base_of(f: &Func, ptr: &HashSet<Value>, erasing: &HashSet<Value>) -> HashMap<Value, Value> {
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
                if ptr.contains(&v) && ptr.contains(&base) && !erasing.contains(&v) {
                    out.insert(v, base);
                }
            }
        }
    }
    out
}

/// The owner at the bottom of a projection chain — what actually holds the storage.
/// An owner is its own root, so this is the identity on anything not in `base_of`, which
/// is why callers can hand it any value without checking first. The walk terminates
/// because `base_of`'s edges always point at an operand, and SSA operands are defined
/// before their uses.
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

/// Plan one function, then rewrite it. The two phases are kept apart on purpose: every
/// `Plan` is computed against the untouched function, so no block's analysis can observe
/// the retains and releases another block's rewrite already inserted.
///
/// `ptr` is the value set the pass tracks, and it is gated on `Repr::is_counted`, not
/// `is_pointer` — an aggregate holding a string is counted even though the aggregate
/// itself is inline (see `repr`). An empty `ptr` is the common case for numeric code and
/// exits before any dataflow runs.
///
/// The rewrite appends edge blocks numbered from `f.blocks.len()` upward, in the order
/// they are created, so a block's id remains its index in `f.blocks` — the invariant
/// `Func::block` indexes on.
fn insert_fn(f: &mut Func) {
    let ptr: HashSet<Value> = f.values().filter(|&v| f.value_repr(v).is_counted()).collect();
    if ptr.is_empty() {
        return;
    }
    let erasing = erasing_casts(f);
    let bases = base_of(f, &ptr, &erasing);
    // A lifted lambda's environment parameter is borrowed: the closure value owns it and
    // may be called again. It stays in `ptr` so reads out of it are still views.
    let env_param: Option<Value> = f.env.is_some().then(|| f.params[0]);
    let (live_in, live_out) = liveness(f, &ptr, &bases, &erasing);

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

            let (consuming, borrowing) = operand_uses(inst, &ptr, &erasing);
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
///
/// Only the block boundaries are returned. `insert_fn` redoes the intra-block backward
/// walk itself, because it needs the liveness *at each instruction* to decide move-versus-
/// retain, and materialising that for every program point would be far more state than
/// the one pass that wants it justifies.
///
/// The edge contribution folds in the block arguments as well as the successor's live-in:
/// an owner passed along an edge is live at the terminator even if the successor never
/// mentions it under that name, since the successor sees it as a parameter.
#[allow(clippy::type_complexity)]
fn liveness(
    f: &Func,
    ptr: &HashSet<Value>,
    bases: &HashMap<Value, Value>,
    erasing: &HashSet<Value>,
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
                let (c, br) = operand_uses(inst, ptr, erasing);
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

/// A pointer op's operands split into consuming and borrowing uses — the input to every
/// retain decision, so the split *is* the ownership convention.
///
/// Consuming means the reference goes somewhere that will release it later: a callee's
/// parameter, or a slot in an aggregate or closure environment. Borrowing means the op
/// only reads: projections, tests, `Prim` comparisons, and a `CallClosure`'s callee.
/// `Index` borrows both operands even though its *result* is owned — `emit_index` retains
/// what it hands back, which is why `base_of` deliberately omits `Index`.
///
/// `Retain`/`Release` report no uses at all. They are the pass's own output; counting them
/// as uses would make the analysis depend on its own results.
///
/// Deliberately **exhaustive** — the constant ops are spelled out rather than swept into a
/// wildcard — for the same reason `opt::op_operands` is, but with a worse failure. An
/// operand missing here is not seen as a use, so its root's last use is placed at some
/// earlier instruction and the release fires while this op still reads it: a
/// use-after-free, not a missed optimisation. A new `Op` must therefore fail to compile
/// until someone has decided whether it consumes or borrows.
fn operand_uses(
    inst: &Inst,
    ptr: &HashSet<Value>,
    erasing: &HashSet<Value>,
) -> (Vec<Value>, Vec<Value>) {
    let mut consuming = Vec::new();
    let mut borrowing = Vec::new();
    match &inst.op {
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
        // An erasing cast is the one `Cast` that takes ownership: `neon_box_new` copies
        // the payload into a box whose drop releases it, so the reference moves in.
        Op::Cast(v) if inst.result.is_some_and(|r| erasing.contains(&r)) => consuming.push(*v),
        Op::Cast(v)
        | Op::IsNull(v)
        | Op::IsErr(v)
        | Op::UnwrapOk(v)
        | Op::UnwrapErr(v)
        | Op::IsVariant { value: v, .. } => borrowing.push(*v),
        Op::Prim(_, vs) => borrowing.extend(vs.iter().copied()),
        Op::Retain(_) | Op::Release(_) => {}
        Op::ConstI64(_)
        | Op::ConstF64(_)
        | Op::ConstBool(_)
        | Op::ConstStr(_)
        | Op::ConstNull
        | Op::ConstUnit
        | Op::ConstAtom(_) => {}
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

/// Each outgoing edge as its target and the arguments passed along it, one entry per edge
/// even when two arms share a target — the two edges get separate bookkeeping, and
/// collapsing them would attribute one edge's retains to both.
///
/// The order here is the contract that ties the three loops together: `liveness` reads it,
/// `insert_fn` builds `Plan::edges` from it, and the rewrite consumes that list positionally
/// against `Term`'s own arms (`then` then `els`; the switch arms in order, then `default`).
/// Reordering any one of those without the others silently attaches edge code to the wrong
/// successor. `ret`/`throw`/`unreachable` return nothing, and `insert_fn` special-cases the
/// first two into a single pre-terminator entry rather than an edge.
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
