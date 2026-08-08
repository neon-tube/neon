//! Which list values are provably sole-owned round a loop, and the rewrite that makes
//! their writes happen in place.
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
//! # The property, and how it is established rather than proven
//!
//! A list value is **sole-owned** at a point if no other live reference to it exists, in
//! which case mutating it in place is indistinguishable from copying it. No
//! intraprocedural analysis can prove that for a list arriving as a parameter — the
//! caller may hold another reference. So the property is *established* instead: on every
//! entry edge to a qualifying loop, the transformation inserts `neon_list_ensure_unique`,
//! which clones at most once (and is a pointer test when the list already stands alone).
//! What the analysis must then guarantee is only this: **between that entry edge and each
//! rewritten write, the function itself creates no second reference and reads no
//! stale copy.** The runtime handles the caller; the analysis handles the loop.
//!
//! # The chain rules
//!
//! From each loop-header list parameter the analysis computes the closure of values the
//! list flows through: block-argument carries continue the chain (SSA plumbing, not a new
//! reference), and a `list::set`'s unwrapped result continues it (the write consumes one
//! link and produces the next). Every member's every use must be one of:
//!
//!   - an `Index` read or a read-only list native (`at`, `at_scalar`, `len`);
//!   - **at most one** consuming `list::set`, as the *list* argument — a value consumed by
//!     two writes is a genuine fork, two logical lists sharing a buffer;
//!   - a carry to a block parameter (followed), or a `ret`/`throw` (the single reference
//!     leaves whole);
//!   - the `try!` plumbing of its own write (`is_err`/`unwrap_err`/`unwrap_ok`), matched
//!     structurally — see below.
//!
//! Two rules exist because their absence was a miscompile, not a theory:
//!
//! **Order.** A read of the consumed value that can execute *after* its write would see
//! the new contents where clone semantics showed the old — `let next = set(acc, ..);
//! io::println("#{acc[i]}")` prints the old element today and must keep doing so. So no
//! other use of the consumed value may lie in a block reachable from the write without
//! re-entering the value's *defining block* (re-entering it rebinds the value — a
//! parameter by the edge, a result by re-execution — and the question restarts), nor
//! after the write in its own block. The stop must be the defining block and not the
//! candidate's header: a chain through nested loops binds values at inner headers, and
//! stopping only at the outer one declined every such chain.
//!
//! **Whole-chain closure.** The walk follows *every* carry, not just the one back to the
//! header. An escape behind an intermediate join — write, then `helper(acc)` on the far
//! side of an `if` — is an escape all the same; the first version of this walk stopped at
//! the first non-header carry and would have missed it.
//!
//! # What the rewrite does
//!
//! Only writes in `try!` shape are rewritten — the tagged result opened by `is_err`, the
//! error edge a panic — because the in-place primitive traps on a bad index rather than
//! producing a catchable error. A program that *catches* `IndexError` from a write keeps
//! the generic call. For a qualifying site: the call becomes the no-result native
//! `neon_list_set_inplace`, the branch collapses to a jump onto the ok edge, the `try!`
//! plumbing is deleted, and every use of the unwrapped result is substituted with the
//! list argument itself — which is what makes the loop-carried value a single SSA name
//! the C compiler can keep in registers. `neon_list_ensure_unique` goes on each entry
//! edge (an edge block if the predecessor branches).
//!
//! The element repr must be uncounted: `neon_list_set_scalar_inplace` is a raw store, so
//! a refcounted element's displaced value would leak. A sole-owned chain over counted
//! elements is reported (`Candidate::scalar == false`) but not rewritten.
//!
//! Downstream obligations, stated where they land: the refcount pass treats
//! `neon_list_set_inplace` as *borrowing* its arguments (`refcount::operand_uses`) — the
//! one owner stays on the chain — and codegen derives its element repr from the list
//! argument, since there is no result to ask (`backend/c.rs::emit_list_builder`).
//!
//! # Why this runs between the optimiser and refcounting
//!
//! Before refcounting, because that pass retains the very value the chain carries —
//! bookkeeping balanced by a matching release, indistinguishable in the IR from a real
//! second reference. After the optimiser, so the shapes matched here are the shapes
//! codegen will see. `Stage::Optimised` output is printed *before* this rewrite; the
//! rewritten IR is visible at `Stage::Final`.

use super::repr::Repr;
use super::ssa::{Block, BlockId, Func, Inst, Op, Program, Target, Term, Value};
use std::collections::{HashMap, HashSet};

/// The lowered name of the stdlib write, before the monomorphised suffix.
const SET_PREFIX: &str = "std__collections__list__set";

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
    /// Whether the element repr is uncounted — the precondition of the scalar in-place
    /// write. A sole-owned chain over counted elements is true but not (yet) actionable.
    pub scalar: bool,
}

/// Every sole-ownership candidate in the program. The query behind `apply`, exposed so
/// the report harness can print what would be rewritten and what was declined.
pub fn candidates(program: &Program) -> Vec<Candidate> {
    let mut out = Vec::new();
    for f in &program.funcs {
        for p in plans(f) {
            out.push(Candidate {
                func: f.name.clone(),
                header: p.header,
                param: p.param,
                writes: p.sites.len(),
                scalar: p.scalar,
            });
        }
    }
    out
}

/// Rewrite every qualifying chain's writes to in-place stores, establishing uniqueness on
/// the loop's entry edges. Runs after the optimiser and before refcounting.
pub fn apply(program: &mut Program) {
    for f in &mut program.funcs {
        // One plan per round, then re-derive: a rewrite changes the IR the other plans
        // were computed against (values substituted, blocks appended), and re-asking is
        // cheaper than proving staleness impossible. Each round converts at least one
        // `set` call, so this terminates; the bound is a backstop, not a schedule.
        for _ in 0..64 {
            let Some(plan) = plans(f).into_iter().find(|p| p.scalar) else {
                break;
            };
            rewrite(f, &plan);
        }
    }
}

/// One qualifying chain: the loop parameter it hangs off and the write sites on it.
struct Plan {
    header: BlockId,
    param: Value,
    scalar: bool,
    sites: Vec<Site>,
}

/// One `try!`-shaped write on a chain, with everything the rewrite touches.
struct Site {
    /// Block holding the `set` call, whose terminator is the `try!` branch.
    block: BlockId,
    /// The call's tagged result, by which the call instruction is found again.
    tagged: Value,
    is_err: Value,
    unwrap_err: Option<Value>,
    /// The ok-side opening of the tagged result; absent when the result is discarded.
    unwrap_ok: Option<Value>,
    /// The non-error edge, which the rewrite jumps to unconditionally.
    ok: Target,
}

/// A located `list::set` call.
struct SetCall {
    block: BlockId,
    idx: usize,
    tagged: Value,
}

/// How a value is used, and where. The *where* is load-bearing: the order rule compares
/// use sites against write sites, so a use without a location could not be judged.
enum Use {
    /// Read by the instruction at `at` (block, instruction index).
    By { at: (BlockId, usize), op: Op },
    /// Passed as a block argument on an edge leaving `from`. This is SSA plumbing, not a
    /// new reference: the value moves to the parameter, which continues the same chain.
    Carried {
        from: BlockId,
        to: BlockId,
        slot: usize,
    },
    /// Read by the terminator of `from` itself.
    Term { from: BlockId, kind: TermUse },
}

enum TermUse {
    /// Returned or thrown: the single reference leaves the function whole.
    Handoff,
    /// Scrutinised by a branch or switch. A list cannot be, so this fails the chain.
    Scrutinee,
}

/// Every use of each value, located.
fn uses(f: &Func) -> HashMap<Value, Vec<Use>> {
    let mut out: HashMap<Value, Vec<Use>> = HashMap::new();
    for b in &f.blocks {
        for (i, inst) in b.insts.iter().enumerate() {
            for v in operands(&inst.op) {
                out.entry(v).or_default().push(Use::By {
                    at: (b.id, i),
                    op: inst.op.clone(),
                });
            }
        }
        match &b.term {
            Term::Ret(Some(v)) | Term::Throw(v) => out.entry(*v).or_default().push(Use::Term {
                from: b.id,
                kind: TermUse::Handoff,
            }),
            Term::Branch { cond, .. } => out.entry(*cond).or_default().push(Use::Term {
                from: b.id,
                kind: TermUse::Scrutinee,
            }),
            Term::Switch { on, .. } => out.entry(*on).or_default().push(Use::Term {
                from: b.id,
                kind: TermUse::Scrutinee,
            }),
            _ => {}
        }
        for (to, args) in targets_with_dest(&b.term) {
            for (slot, v) in args.iter().enumerate() {
                out.entry(*v).or_default().push(Use::Carried {
                    from: b.id,
                    to,
                    slot,
                });
            }
        }
    }
    out
}

/// The blocks a block can jump to.
fn successors(f: &Func, b: BlockId) -> Vec<BlockId> {
    let block = &f.blocks[b.0 as usize];
    match &block.term {
        Term::Jump(t) => vec![t.to],
        Term::Branch { then, els, .. } => vec![then.to, els.to],
        Term::Switch { arms, default, .. } => arms
            .iter()
            .map(|(_, t)| t.to)
            .chain(std::iter::once(default.to))
            .collect(),
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
        Op::Call { args, .. }
        | Op::Native { args, .. }
        | Op::MakeClosure { captures: args, .. } => args.clone(),
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

/// List natives that only read. Everything else is treated as possibly retaining.
fn is_read_only_list_native(symbol: &str) -> bool {
    matches!(
        symbol,
        "neon_list_at" | "neon_list_at_scalar" | "neon_list_len"
    )
}

fn plans(f: &Func) -> Vec<Plan> {
    let sets = set_calls(f);
    if sets.is_empty() {
        return Vec::new();
    }
    let all_uses = uses(f);
    let mut headers: Vec<BlockId> = back_edges(f).into_iter().map(|(_, h)| h).collect();
    headers.sort_by_key(|b| b.0);
    headers.dedup();

    let mut out = Vec::new();
    for header in headers {
        for &param in &f.blocks[header.0 as usize].params {
            let Repr::List(elem) = f.value_repr(param) else {
                continue;
            };
            let scalar = !elem.is_counted();
            if let Some(sites) = chain(f, &sets, &all_uses, param) {
                if !sites.is_empty() {
                    out.push(Plan {
                        header,
                        param,
                        scalar,
                        sites,
                    });
                }
            }
        }
    }
    out
}

/// The `list::set` calls in a function. Matched by the name lowering gives the stdlib
/// function, so a user function called `set` is not mistaken for it.
fn set_calls(f: &Func) -> Vec<SetCall> {
    let mut out = Vec::new();
    for b in &f.blocks {
        for (idx, inst) in b.insts.iter().enumerate() {
            if let Op::Call { func, args } = &inst.op {
                if func.starts_with(SET_PREFIX) && args.len() == 3 {
                    if let Some(tagged) = inst.result {
                        out.push(SetCall {
                            block: b.id,
                            idx,
                            tagged,
                        });
                    }
                }
            }
        }
    }
    out
}

/// The closure of values a header parameter's list flows through, validated against the
/// chain rules. `None` means something on the chain escapes, forks, or reads stale.
fn chain(
    f: &Func,
    sets: &[SetCall],
    all_uses: &HashMap<Value, Vec<Use>>,
    param: Value,
) -> Option<Vec<Site>> {
    let mut members = HashSet::from([param]);
    let mut queue = vec![param];
    let mut sites = Vec::new();

    while let Some(v) = queue.pop() {
        let mut consumed_by: Option<&SetCall> = None;
        for u in all_uses.get(&v).into_iter().flatten() {
            match u {
                // Reading an element or a length leaves no reference behind.
                Use::By {
                    op: Op::Index { .. },
                    ..
                } => {}
                Use::By {
                    op: Op::Native { symbol, .. },
                    ..
                } if is_read_only_list_native(symbol) => {}
                // The consuming write, as the *list* argument. As any other argument —
                // the element of a list-of-lists, say — it falls through to the escape
                // arm below.
                Use::By {
                    at,
                    op: Op::Call { func, args },
                } if func.starts_with(SET_PREFIX) && args.first() == Some(&v) => {
                    let s = sets.iter().find(|s| (s.block, s.idx) == *at)?;
                    if consumed_by.replace(s).is_some() {
                        // Two writes consuming one value is a genuine fork: two logical
                        // lists that must not share a buffer.
                        return None;
                    }
                }
                // Anything else that takes the value — another call, a closure capture, a
                // record field, a cast, a retain — is exactly a second reference.
                Use::By { .. } => return None,
                // Moving to a block parameter continues the chain; follow it. Following
                // *every* carry is the whole-chain rule — see the module doc.
                Use::Carried { to, slot, .. } => {
                    let p = *f.blocks[to.0 as usize].params.get(*slot)?;
                    if members.insert(p) {
                        queue.push(p);
                    }
                }
                // Returned or thrown: the single reference leaves whole.
                Use::Term {
                    kind: TermUse::Handoff,
                    ..
                } => {}
                Use::Term {
                    kind: TermUse::Scrutinee,
                    ..
                } => return None,
            }
        }
        if let Some(s) = consumed_by {
            if !write_is_last(f, all_uses, defining_block(f, v), v, s) {
                return None;
            }
            let site = try_shape(f, all_uses, s)?;
            if let Some(ok) = site.unwrap_ok {
                if members.insert(ok) {
                    queue.push(ok);
                }
            }
            sites.push(site);
        }
    }
    Some(sites)
}

/// The order rule: no other use of `v` may execute after its consuming write. In place,
/// the old and new lists are one buffer, so a read ordered after the write would see the
/// new contents where clone semantics showed the old.
///
/// "After" is: in a block reachable from the write's block without re-entering the block
/// that *defines* `v`, or after the write inside its own block. The defining block is
/// the stop because re-entering it rebinds `v` before any use in it can run — a
/// parameter is rebound by the edge, an instruction result by re-execution — and the
/// question restarts for the new binding. (An earlier version stopped at the candidate's
/// loop header instead, which is the same thing for a single-loop chain and wrong for a
/// nested one: a chain value bound at an *inner* header looked readable-after-write from
/// the outer loop, so no multi-loop chain ever qualified.) A terminator's reads happen
/// after every instruction, so a carry or handoff *from* the write's block counts.
fn write_is_last(
    f: &Func,
    all_uses: &HashMap<Value, Vec<Use>>,
    def_block: BlockId,
    v: Value,
    s: &SetCall,
) -> bool {
    let mut reach = HashSet::new();
    let mut stack: Vec<BlockId> = successors(f, s.block)
        .into_iter()
        .filter(|b| *b != def_block)
        .collect();
    while let Some(b) = stack.pop() {
        if reach.insert(b) {
            stack.extend(successors(f, b).into_iter().filter(|x| *x != def_block));
        }
    }
    for u in all_uses.get(&v).into_iter().flatten() {
        let after = match u {
            Use::By { at, .. } if *at == (s.block, s.idx) => false, // the write itself
            Use::By { at, .. } => reach.contains(&at.0) || (at.0 == s.block && at.1 > s.idx),
            Use::Carried { from, .. } | Use::Term { from, .. } => {
                reach.contains(from) || *from == s.block
            }
        };
        if after {
            return false;
        }
    }
    true
}

/// Match a write's surroundings against the `try!` shape, or decline the site.
///
/// The shape: the tagged result is read only by one `is_err`, at most one `unwrap_err`,
/// and at most one `unwrap_ok`; the call's block ends in a branch on the `is_err`, whose
/// true edge leads to a panic block (ends `unreachable`, contains `neon_panic`) and may
/// carry the `unwrap_err` result there — and carries it nowhere else; the `unwrap_ok`
/// sits in the ok-edge block. Anything else — a caught error, a stored tagged result —
/// keeps the generic call, because the in-place primitive traps where this shape panics
/// and a program observing the difference must not be rewritten.
fn try_shape(f: &Func, all_uses: &HashMap<Value, Vec<Use>>, s: &SetCall) -> Option<Site> {
    let mut is_err: Option<Value> = None;
    let mut unwrap_ok: Option<(BlockId, Value)> = None;
    let mut unwrap_err: Option<Value> = None;
    for u in all_uses.get(&s.tagged).into_iter().flatten() {
        match u {
            Use::By {
                at,
                op: Op::IsErr(_),
            } => {
                let r = result_at(f, *at)?;
                if is_err.replace(r).is_some() {
                    return None;
                }
            }
            Use::By {
                at,
                op: Op::UnwrapOk(_),
            } => {
                let r = result_at(f, *at)?;
                if unwrap_ok.replace((at.0, r)).is_some() {
                    return None;
                }
            }
            Use::By {
                at,
                op: Op::UnwrapErr(_),
            } => {
                let r = result_at(f, *at)?;
                if unwrap_err.replace(r).is_some() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    let is_err = is_err?;

    let Term::Branch { cond, then, els } = &f.blocks[s.block.0 as usize].term else {
        return None;
    };
    if *cond != is_err || !is_panic_block(f, then.to) {
        return None;
    }
    // The `is_err` must feed that branch and nothing else.
    for u in all_uses.get(&is_err).into_iter().flatten() {
        match u {
            Use::Term {
                from,
                kind: TermUse::Scrutinee,
            } if *from == s.block => {}
            _ => return None,
        }
    }
    // The `unwrap_err` may ride the error edge and go nowhere else.
    if let Some(e) = unwrap_err {
        for u in all_uses.get(&e).into_iter().flatten() {
            match u {
                Use::Carried { from, to, .. } if *from == s.block && *to == then.to => {}
                _ => return None,
            }
        }
    }
    // The ok-side opening must sit in the ok block, where the collapse-to-jump keeps it
    // dominated by the write.
    if let Some((b, _)) = unwrap_ok {
        if b != els.to {
            return None;
        }
    }
    Some(Site {
        block: s.block,
        tagged: s.tagged,
        is_err,
        unwrap_err,
        unwrap_ok: unwrap_ok.map(|(_, r)| r),
        ok: els.clone(),
    })
}

fn result_at(f: &Func, at: (BlockId, usize)) -> Option<Value> {
    f.blocks[at.0 .0 as usize].insts.get(at.1)?.result
}

/// The block that binds `v`: as one of its parameters, or as an instruction result.
/// Every chain value is one or the other — chains are made of parameters and
/// `unwrap_ok` results.
fn defining_block(f: &Func, v: Value) -> BlockId {
    for b in &f.blocks {
        if b.params.contains(&v) || b.insts.iter().any(|i| i.result == Some(v)) {
            return b.id;
        }
    }
    unreachable!("a chain value is defined somewhere")
}

/// A block that only panics: ends `unreachable` and calls `neon_panic` on the way.
fn is_panic_block(f: &Func, b: BlockId) -> bool {
    let blk = &f.blocks[b.0 as usize];
    matches!(blk.term, Term::Unreachable)
        && blk
            .insts
            .iter()
            .any(|i| matches!(&i.op, Op::Native { symbol, .. } if symbol == "neon_panic"))
}

/// Apply one plan: rewrite its write sites, delete the `try!` plumbing, substitute the
/// unwrapped results away, and establish uniqueness on the loop's entry edges.
fn rewrite(f: &mut Func, plan: &Plan) {
    let mut subst: HashMap<Value, Value> = HashMap::new();
    let mut dead: HashSet<Value> = HashSet::new();

    // The write itself: call → no-result native, branch → jump onto the ok edge. The
    // call is found by its tagged result, not a stored index — earlier rewrites in this
    // plan may have shifted instruction positions.
    for site in &plan.sites {
        let b = &mut f.blocks[site.block.0 as usize];
        let inst = b
            .insts
            .iter_mut()
            .find(|i| i.result == Some(site.tagged))
            .expect("a validated site's call is present");
        let Op::Call { args, .. } = &inst.op else {
            unreachable!("a validated site's tagged value is a call result")
        };
        let (list, index, elem) = (args[0], args[1], args[2]);
        inst.result = None;
        inst.op = Op::Native {
            symbol: "neon_list_set_inplace".into(),
            args: vec![list, index, elem],
        };
        b.term = Term::Jump(site.ok.clone());
        dead.insert(site.is_err);
        dead.extend(site.unwrap_err);
        if let Some(ok) = site.unwrap_ok {
            subst.insert(ok, list);
            dead.insert(ok);
        }
    }

    // Chained sites substitute through each other (%43 → %36 → %10): resolve to fixpoint
    // before touching the IR. Acyclic by construction — each maps to an earlier link,
    // bottoming out at the parameter, which is never a key.
    let resolved: HashMap<Value, Value> = subst
        .keys()
        .map(|&k| {
            let mut v = k;
            while let Some(&n) = subst.get(&v) {
                v = n;
            }
            (k, v)
        })
        .collect();

    for b in &mut f.blocks {
        b.insts
            .retain(|i| !i.result.is_some_and(|r| dead.contains(&r)));
        for inst in &mut b.insts {
            map_operands(&mut inst.op, &resolved);
        }
        map_term(&mut b.term, &resolved);
    }

    establish(f, plan);
}

/// Insert `neon_list_ensure_unique` on every entry edge to the plan's header, feeding the
/// candidate parameter's slot. A jumping predecessor takes the call inline; a branching
/// one gets an edge block, so its other successors do not pay for a clone they must not
/// see.
fn establish(f: &mut Func, plan: &Plan) {
    let header = plan.header;
    let slot = f.blocks[header.0 as usize]
        .params
        .iter()
        .position(|&p| p == plan.param)
        .expect("the plan's parameter is on its header");
    let backs: HashSet<(BlockId, BlockId)> = back_edges(f).into_iter().collect();
    let repr = f.value_repr(plan.param).clone();
    let ty = f.value_ty(plan.param);

    let ensure = |arg: Value| Op::Native {
        symbol: "neon_list_ensure_unique".into(),
        args: vec![arg],
    };

    // The range is taken once: the edge blocks appended below also jump to the header,
    // and revisiting them would establish uniqueness twice.
    for bi in 0..f.blocks.len() {
        let from = BlockId(bi as u32);
        if backs.contains(&(from, header)) {
            continue;
        }
        if matches!(&f.blocks[bi].term, Term::Jump(t) if t.to == header) {
            let old = {
                let Term::Jump(t) = &f.blocks[bi].term else {
                    unreachable!()
                };
                t.args[slot]
            };
            let nv = f.new_value(repr.clone(), ty);
            let b = &mut f.blocks[bi];
            b.insts.push(Inst {
                result: Some(nv),
                op: ensure(old),
            });
            let Term::Jump(t) = &mut b.term else {
                unreachable!()
            };
            t.args[slot] = nv;
            continue;
        }
        // A branch or switch cannot take the call in its own block; give each
        // header-bound edge its own block. The edge's arguments move into it — they
        // dominated the edge, so they dominate the block that *is* the edge.
        let base = f.blocks.len();
        let mut edge_args: Vec<(BlockId, Vec<Value>)> = Vec::new();
        for t in term_targets_mut(&mut f.blocks[bi].term) {
            if t.to != header {
                continue;
            }
            let nb = BlockId((base + edge_args.len()) as u32);
            edge_args.push((nb, std::mem::take(&mut t.args)));
            *t = Target {
                to: nb,
                args: vec![],
            };
        }
        for (nb, mut args) in edge_args {
            let old = args[slot];
            let nv = f.new_value(repr.clone(), ty);
            args[slot] = nv;
            f.blocks.push(Block {
                id: nb,
                params: vec![],
                insts: vec![Inst {
                    result: Some(nv),
                    op: ensure(old),
                }],
                term: Term::Jump(Target { to: header, args }),
            });
        }
    }
}

/// Every target of a terminator, mutably.
fn term_targets_mut(term: &mut Term) -> Vec<&mut Target> {
    match term {
        Term::Jump(t) => vec![t],
        Term::Branch { then, els, .. } => vec![then, els],
        Term::Switch { arms, default, .. } => arms
            .iter_mut()
            .map(|(_, t)| t)
            .chain(std::iter::once(default))
            .collect(),
        Term::Ret(_) | Term::Throw(_) | Term::Unreachable => vec![],
    }
}

/// Substitute values through an op's operands. Exhaustive for the same reason `operands`
/// is: a missed operand here leaves a use of a deleted value in the IR.
fn map_operands(op: &mut Op, m: &HashMap<Value, Value>) {
    let r = |v: &mut Value| {
        if let Some(&n) = m.get(v) {
            *v = n;
        }
    };
    match op {
        Op::Prim(_, vs) | Op::MakeTuple(vs) | Op::MakeList(vs) => vs.iter_mut().for_each(r),
        Op::Call { args, .. }
        | Op::Native { args, .. }
        | Op::MakeClosure { captures: args, .. } => args.iter_mut().for_each(r),
        Op::CallClosure { callee, args } => {
            r(callee);
            args.iter_mut().for_each(r);
        }
        Op::MakeRecord { fields, .. } => fields.iter_mut().for_each(|(_, v)| r(v)),
        Op::Field { base, .. } | Op::Elem { base, .. } => r(base),
        Op::Index { base, index } => {
            r(base);
            r(index);
        }
        Op::Cast(v)
        | Op::IsErr(v)
        | Op::UnwrapOk(v)
        | Op::UnwrapErr(v)
        | Op::IsNull(v)
        | Op::Retain(v)
        | Op::Release(v) => r(v),
        Op::IsVariant { value, .. } => r(value),
        Op::ConstI64(_)
        | Op::ConstF64(_)
        | Op::ConstBool(_)
        | Op::ConstStr(_)
        | Op::ConstNull
        | Op::ConstUnit
        | Op::ConstAtom(_) => {}
    }
}

/// Substitute values through a terminator's reads and edge arguments.
fn map_term(term: &mut Term, m: &HashMap<Value, Value>) {
    let r = |v: &mut Value| {
        if let Some(&n) = m.get(v) {
            *v = n;
        }
    };
    match term {
        Term::Ret(Some(v)) | Term::Throw(v) => r(v),
        Term::Ret(None) | Term::Unreachable => {}
        Term::Jump(t) => t.args.iter_mut().for_each(r),
        Term::Branch { cond, then, els } => {
            r(cond);
            then.args.iter_mut().for_each(r);
            els.args.iter_mut().for_each(r);
        }
        Term::Switch { on, arms, default } => {
            r(on);
            arms.iter_mut()
                .for_each(|(_, t)| t.args.iter_mut().for_each(r));
            default.args.iter_mut().for_each(r);
        }
    }
}

// ---- diagnostics, for the reporting harness ----

/// The `list::set` calls this pass recognises, for `unique_report`'s debug mode.
pub fn debug_sets(f: &Func) -> Vec<(Value, Value)> {
    let mut out = Vec::new();
    for b in &f.blocks {
        for inst in &b.insts {
            if let Op::Call { func, args } = &inst.op {
                if func.starts_with(SET_PREFIX) && args.len() == 3 {
                    if let Some(r) = inst.result {
                        out.push((r, args[0]));
                    }
                }
            }
        }
    }
    out
}

/// The back edges this pass finds, for `unique_report`'s debug mode.
pub fn debug_back_edges(f: &Func) -> Vec<(BlockId, BlockId)> {
    back_edges(f)
}
