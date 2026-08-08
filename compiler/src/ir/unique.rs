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
//! For a `try!`-shaped site — the tagged result opened by `is_err`, the error edge a
//! panic — the call becomes the no-result native `neon_list_set_inplace`, the branch
//! collapses to a jump onto the ok edge, the `try!` plumbing is deleted, and every use
//! of the unwrapped result is substituted with the list argument itself — which is what
//! makes the loop-carried value a single SSA name the C compiler can keep in registers.
//! (The in-place primitive traps on a bad index exactly where this shape panicked.)
//! A CAUGHT write is rewritten differently — see "Caught writes" below; it used to keep
//! the generic call outright. `neon_list_ensure_unique` goes on each entry edge (an
//! edge block if the predecessor branches).
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
//! # Nested writes: the path rewrite
//!
//! `m[i][j] = v`, spelled as its lowering — `set(m, i, set(m[i], j, v))` — used to be
//! structurally hopeless for this pass: `m[i]` hands back a retained element, so the
//! inner list's `rc` is 2 the moment it has a name, the runtime's own `rc == 1` fast
//! path is unreachable, and every cell write cloned the whole row. Measured on an
//! n x n grid fill, that is O(n³) for O(n²) work — 22x at n = 3200 and growing with n —
//! and a cube is O(n⁴).
//!
//! The rewrite recognises the whole GROUP by its dataflow: an outer write whose element
//! is the unwrapped result of an inner write on an `Index` read of the same list at the
//! same index value, to any depth. Each level's read becomes
//! `neon_list_ensure_unique_at(parent, i)` — the slot's element made sole-owned in
//! place, handed back borrowed — and each level's write BACK is deleted: the pointer it
//! would store is already in the slot, and its error edge is unreachable because the
//! read it replaced trapped on the same index. Only the leaf write survives, as the
//! same `set_scalar_inplace` a flat chain gets. Group-internal values are validated to
//! have no other uses, and every level obeys the order rule against the LEAF — a read
//! of any enclosing list after the innermost store would see the mutation.
//!
//! Cost per cell write after the rewrite: one pointer-compare per level plus the raw
//! store. The first touch of a shared row still clones it — `repeat` stores one row
//! pointer n times, and copy-on-write semantics REQUIRE that clone — but it amortises
//! to one clone per distinct row, O(1) per write, which is optimal under value
//! semantics.
//!
//! # Caught writes
//!
//! A write whose `IndexError` is caught (`try ... catch`) used to keep the generic call
//! outright, because the in-place primitive traps where the handler expects a value.
//! It is now rewritten by splitting on an explicit bounds check: in bounds, the
//! in-place store and a jump to the ok edge; out of bounds, the ORIGINAL `set` call in
//! a slow block, which throws the stdlib's own `IndexError` to the original handler —
//! same error value, same text, built by the same code. The slow path is only
//! reachable when the call must throw, so the in-place discipline is undisturbed. A
//! handler that uses the OLD list declines the chain through the ordinary use rules —
//! that use is a live second name for the buffer. Caught writes are rewritten flat
//! only; a nested group's deleted upper levels are justified by trap equivalence,
//! which a live handler does not provide.
//!
//! # Why this runs between the optimiser and refcounting
//!
//! Before refcounting, because that pass retains the very value the chain carries —
//! bookkeeping balanced by a matching release, indistinguishable in the IR from a real
//! second reference. After the optimiser, so the shapes matched here are the shapes
//! codegen will see. `Stage::Optimised` output is printed *before* this rewrite; the
//! rewritten IR is visible at `Stage::Final`. The refcount pass treats
//! `neon_list_ensure_unique_at`'s result as a VIEW of its list operand (`base_of`), so
//! the borrowed row is never separately released, and a consuming use of it — the
//! caught slow path's call — gets the retain the view discipline already provides.

use super::repr::Repr;
use super::ssa::{Block, BlockId, Func, Inst, Op, PrimOp, Program, Target, Term, Value};
use std::collections::{HashMap, HashSet};

/// The lowered name of the stdlib write, before the monomorphised suffix.
const SET_PREFIX: &str = "std__list__set";

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
    /// Whether every write's LEAF element repr is uncounted — the precondition of the
    /// scalar in-place store. For a nested (path) write the leaf is the innermost list;
    /// a sole-owned chain whose leaf elements are counted is true but not actionable.
    pub scalar: bool,
    /// The deepest write's nesting: 1 for a flat `set`, 2 for `m[i][j]`-shaped, and so
    /// on. Depth above 1 means the path rewrite fires (`ensure_unique_at` per level).
    pub depth: usize,
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
                depth: p.sites.iter().map(|s| s.path.len() + 1).max().unwrap_or(1),
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

/// The `try` plumbing round one write. `err: None` is the `try!` shape — the error edge
/// panics — and the rewrite may delete the branch outright. `err: Some` is a CAUGHT
/// write: the error edge is a live handler, so the rewrite must keep a path to it.
struct Shape {
    /// Block holding the `set` call, whose terminator is the branch on `is_err`.
    block: BlockId,
    /// The call's tagged result, by which the call instruction is found again.
    tagged: Value,
    is_err: Value,
    unwrap_err: Option<Value>,
    /// The ok-side opening of the tagged result; absent when the result is discarded.
    unwrap_ok: Option<Value>,
    /// The non-error edge, which the in-place write jumps to.
    ok: Target,
    /// For a caught write, the error edge (the handler). `None` when it panics.
    err: Option<Target>,
}

/// One rewriteable write on a chain. A flat `set` is a `leaf` with an empty `path`; a
/// nested `m[i][j] = v`-shaped group carries one `PathLevel` per enclosing list,
/// outermost first.
struct Site {
    /// The write whose element actually lands: the innermost `set`.
    leaf: Shape,
    /// The levels above the leaf, outermost first. Each one's `write` is deleted — it
    /// can no longer fire once the leaf mutates in place — and each one's `read`
    /// becomes `neon_list_ensure_unique_at`.
    path: Vec<PathLevel>,
}

/// One enclosing level of a nested write: the element read that walks down, and the
/// store back up that the rewrite deletes.
struct PathLevel {
    /// The `Op::Index` result — the inner list this level reads out. The rewrite turns
    /// the read into `ensure_unique_at(parent, index)`, keeping the same result value.
    read: Value,
    /// The list the read indexes into: the chain value at level 0, the level above's
    /// `read` otherwise.
    parent: Value,
    /// The index, one SSA value used by both the read and the write back.
    index: Value,
    /// This level's own `try!`-shaped `set(parent, index, ..)`, deleted by the rewrite:
    /// the preceding read proved the index in bounds, so it can never throw.
    write: Shape,
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
            let Repr::List(_) = f.value_repr(param) else {
                continue;
            };
            if let Some(sites) = chain(f, &sets, &all_uses, param) {
                if !sites.is_empty() {
                    // Actionable when every site's LEAF list has uncounted elements —
                    // for a flat site that is the chain's own element repr, for a
                    // nested one the innermost list's.
                    let scalar = sites.iter().all(|s| leaf_scalar(f, s));
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

/// Whether a site's innermost write stores an uncounted element — the precondition of
/// `neon_list_set_scalar_inplace`'s raw store.
fn leaf_scalar(f: &Func, site: &Site) -> bool {
    let b = &f.blocks[site.leaf.block.0 as usize];
    let Some(inst) = b.insts.iter().find(|i| i.result == Some(site.leaf.tagged)) else {
        return false;
    };
    let Op::Call { args, .. } = &inst.op else {
        return false;
    };
    matches!(f.value_repr(args[0]), Repr::List(e) if !e.is_counted())
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
            if !write_is_last(f, all_uses, defining_block(f, v), v, s, &[]) {
                return None;
            }
            let outer = write_shape(f, all_uses, s)?;
            let site = deepen(f, sets, all_uses, v, s, outer)?;
            // The chain continues past the OUTER write's unwrapped result — for a
            // nested site that is the top of the path, not the leaf.
            let outer_shape = site.path.first().map(|l| &l.write).unwrap_or(&site.leaf);
            if let Some(ok) = outer_shape.unwrap_ok {
                if members.insert(ok) {
                    queue.push(ok);
                }
            }
            // For a nested site the leaf mutation lands while the chain value is still
            // live (its own write is above), so the order rule must ALSO hold against
            // the leaf: no read of `v` may execute after the leaf write — its slot may
            // have been swapped by `ensure_unique_at` and its contents mutated. The
            // site's own upper writes are after the leaf by construction and allowed.
            if !site.path.is_empty() {
                let leaf_set = SetCall {
                    block: site.leaf.block,
                    idx: instruction_index(f, site.leaf.block, site.leaf.tagged)?,
                    tagged: site.leaf.tagged,
                };
                let allowed: Vec<(BlockId, usize)> = site
                    .path
                    .iter()
                    .map(|l| {
                        Some((
                            l.write.block,
                            instruction_index(f, l.write.block, l.write.tagged)?,
                        ))
                    })
                    .collect::<Option<_>>()?;
                if !write_is_last(f, all_uses, defining_block(f, v), v, &leaf_set, &allowed) {
                    return None;
                }
            }
            sites.push(site);
        }
    }
    Some(sites)
}

/// The position of the instruction producing `tagged` inside `block`.
fn instruction_index(f: &Func, block: BlockId, tagged: Value) -> Option<usize> {
    f.blocks[block.0 as usize]
        .insts
        .iter()
        .position(|i| i.result == Some(tagged))
}

/// Descend from an outer write into a nested path group, or return the write as a flat
/// leaf. The group's shape is the source pattern `set(c, i, set(c[i], j, ..))` after
/// lowering: the outer write's ELEMENT is the unwrapped result of an inner write whose
/// list is an `Index` read of the same list at the same index value.
///
/// Everything internal to the group is validated to be used by the group alone:
///
/// - the inner unwrapped result feeds exactly the enclosing write's element slot;
/// - the `Index` read feeds exactly this level's write (as its list) and at most one
///   further read down — no carries, no escapes, nothing else, so once the leaf
///   mutates in place there is no name left through which the old contents could be
///   observed (the levels have no other uses AT ALL, which subsumes the order rule);
/// - every level above the leaf is `try!`-shaped. Its deletion is what the preceding
///   read licenses: the read trapped on the same index the write stores back to, and
///   no length changed in between, so the write's error edge is unreachable.
fn deepen(
    f: &Func,
    sets: &[SetCall],
    all_uses: &HashMap<Value, Vec<Use>>,
    list: Value,
    set: &SetCall,
    shape: Shape,
) -> Option<Site> {
    let mut path: Vec<PathLevel> = Vec::new();
    let mut cur_list = list;
    let mut cur_set = set;
    let mut cur_shape = shape;

    loop {
        let args = call_args(f, cur_set)?;
        let (index, elem) = (args[1], args[2]);

        // Is the element the unwrapped result of an inner set on a read of this list?
        let Some((inner_set, inner_read)) = inner_write(f, sets, all_uses, cur_list, index, elem)
        else {
            // No: `cur` is the leaf. A caught leaf is only supported flat — the nested
            // rewrite deletes the levels above, and their deletion is argued through
            // `try!`'s trap equivalence, not through a handler.
            if cur_shape.err.is_some() && !path.is_empty() {
                return None;
            }
            // The order rule, aimed at the leaf, for every level's list: a read of the
            // row or of any enclosing list positioned after the in-place write would
            // observe the mutation where clone semantics showed the old element. Reads
            // BEFORE the write stay legal — `set(row, k, row[k] + 1)` reads its own
            // slot to compute the element — because `ensure_unique_at` preserves
            // contents and nothing has mutated yet. Each level's own write-back is
            // after the leaf by construction and is the one allowed exception.
            // `path[k].read` is consumed by `path[k + 1].write` — or by the leaf for
            // the innermost — so that write-back is each read's one allowed use after
            // the leaf; the leaf call itself is excluded by `write_is_last` directly.
            for (k, level) in path.iter().enumerate() {
                let allowed: Vec<(BlockId, usize)> = match path.get(k + 1) {
                    Some(next) => vec![(
                        next.write.block,
                        instruction_index(f, next.write.block, next.write.tagged)?,
                    )],
                    None => vec![],
                };
                if !write_is_last(
                    f,
                    all_uses,
                    defining_block(f, level.read),
                    level.read,
                    cur_set,
                    &allowed,
                ) {
                    return None;
                }
            }
            return Some(Site {
                leaf: cur_shape,
                path,
            });
        };

        // Levels above a leaf must be try!-shaped; their branches are deleted.
        if cur_shape.err.is_some() {
            return None;
        }
        let inner_shape = write_shape(f, all_uses, inner_set)?;
        path.push(PathLevel {
            read: inner_read,
            parent: cur_list,
            index,
            write: cur_shape,
        });
        cur_list = inner_read;
        cur_set = inner_set;
        cur_shape = inner_shape;
    }
}

/// Match `elem` as the unwrapped result of an inner `set` whose list argument is an
/// `Index` read of `list` at exactly `index`, with every group-internal value used by
/// the group alone. Returns the inner call and the read's result.
fn inner_write<'a>(
    f: &Func,
    sets: &'a [SetCall],
    all_uses: &HashMap<Value, Vec<Use>>,
    list: Value,
    index: Value,
    elem: Value,
) -> Option<(&'a SetCall, Value)> {
    // `elem` is `unwrap_ok` of an inner tagged result, and feeds ONLY the outer call.
    let (eb, ei) = find_def(f, elem)?;
    let Op::UnwrapOk(inner_tagged) = f.blocks[eb.0 as usize].insts[ei].op else {
        return None;
    };
    if all_uses.get(&elem).map(|u| u.len()) != Some(1) {
        return None;
    }

    let inner = sets.iter().find(|s| s.tagged == inner_tagged)?;
    let iargs = call_args(f, inner)?;
    let inner_list = iargs[0];

    // The inner list is an `Index` read of `list` at the same index VALUE the outer
    // write stores back to — the same slot, definitionally, not coincidentally.
    let (rb, ri) = find_def(f, inner_list)?;
    let Op::Index { base, index: ridx } = f.blocks[rb.0 as usize].insts[ri].op else {
        return None;
    };
    if base != list || ridx != index {
        return None;
    }

    // The read's uses: this write (as its list), plus at most one deeper read — the
    // next level's `Index`. Anything else is a second name for the buffer.
    for u in all_uses.get(&inner_list).into_iter().flatten() {
        match u {
            Use::By {
                at,
                op: Op::Call { func, args },
            } if func.starts_with(SET_PREFIX)
                && args.first() == Some(&inner_list)
                && *at == (inner.block, inner.idx) => {}
            Use::By {
                op: Op::Index { base, .. },
                ..
            } if *base == inner_list => {}
            _ => return None,
        }
    }
    Some((inner, inner_list))
}

/// The instruction position defining `v`, if `v` is an instruction result.
fn find_def(f: &Func, v: Value) -> Option<(BlockId, usize)> {
    for b in &f.blocks {
        if let Some(i) = b.insts.iter().position(|i| i.result == Some(v)) {
            return Some((b.id, i));
        }
    }
    None
}

/// A located call's arguments.
fn call_args<'a>(f: &'a Func, s: &SetCall) -> Option<&'a [Value]> {
    match &f.blocks[s.block.0 as usize].insts.get(s.idx)?.op {
        Op::Call { args, .. } => Some(args),
        _ => None,
    }
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
    allowed: &[(BlockId, usize)],
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
            // A nested group's own write-back one level up: after the leaf by
            // construction, and exactly what the group is for.
            Use::By { at, .. } if allowed.contains(at) => false,
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

/// Match a write's surroundings against the `try!` or caught shape, or decline the site.
///
/// Common to both: the tagged result is read only by one `is_err`, at most one
/// `unwrap_err`, and at most one `unwrap_ok`; the call's block ends in a branch on the
/// `is_err`; the `unwrap_err` may ride the error edge and go nowhere else; the
/// `unwrap_ok` sits in the ok-edge block. A stored or multiply-read tagged result keeps
/// the generic call.
///
/// The error edge decides the kind. A panic block (ends `unreachable`, contains
/// `neon_panic`) is the `try!` shape (`err: None`): the in-place primitive traps where
/// the shape panics, and the rewrite may delete the branch outright. Anything else is a
/// CAUGHT write (`err: Some`), rewriteable only by keeping a slow path to the handler —
/// and only when the instructions between the call and its branch are exactly the
/// tagged-result plumbing, because the caught rewrite moves the call to a slow block
/// and anything else moved with it would be skipped on the fast path.
fn write_shape(f: &Func, all_uses: &HashMap<Value, Vec<Use>>, s: &SetCall) -> Option<Shape> {
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
    if *cond != is_err {
        return None;
    }
    let caught = !is_panic_block(f, then.to);
    if caught {
        // Everything between the call and the branch must be plumbing of this tagged
        // result: the caught rewrite moves the call (and that plumbing) into the slow
        // block, and an unrelated instruction moved there would not run on the fast
        // path while its result might still be read past the join.
        let b = &f.blocks[s.block.0 as usize];
        for inst in &b.insts[s.idx + 1..] {
            match &inst.op {
                Op::IsErr(t) | Op::UnwrapErr(t) if *t == s.tagged => {}
                _ => return None,
            }
        }
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
    Some(Shape {
        block: s.block,
        tagged: s.tagged,
        is_err,
        unwrap_err,
        unwrap_ok: unwrap_ok.map(|(_, r)| r),
        ok: els.clone(),
        err: caught.then(|| then.clone()),
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

    for site in &plan.sites {
        // The levels above a leaf, first: each element read becomes the per-level
        // uniqueness step — same result value, so downstream uses need no patching —
        // and each write back is deleted outright, its block jumping straight onto the
        // ok edge. The preceding read proved the index; the write cannot throw.
        for level in &site.path {
            let (rb, ri) =
                find_def(f, level.read).expect("a validated level's read is an instruction");
            let inst = &mut f.blocks[rb.0 as usize].insts[ri];
            inst.op = Op::Native {
                symbol: "neon_list_ensure_unique_at".into(),
                args: vec![level.parent, level.index],
            };
            let wb = &mut f.blocks[level.write.block.0 as usize];
            wb.term = Term::Jump(level.write.ok.clone());
            dead.insert(level.write.tagged); // deletes the call instruction itself
            dead.insert(level.write.is_err);
            dead.extend(level.write.unwrap_err);
            if let Some(ok) = level.write.unwrap_ok {
                subst.insert(ok, level.parent);
                dead.insert(ok);
            }
        }

        // The leaf. Its list argument is read out of the (already rewritten) call
        // before the op is replaced.
        let b = &mut f.blocks[site.leaf.block.0 as usize];
        let call_pos = b
            .insts
            .iter()
            .position(|i| i.result == Some(site.leaf.tagged))
            .expect("a validated site's call is present");
        let Op::Call { args, .. } = &b.insts[call_pos].op else {
            unreachable!("a validated site's tagged value is a call result")
        };
        let (list, index, elem) = (args[0], args[1], args[2]);

        match &site.leaf.err {
            // `try!`: the in-place store replaces the call and the branch collapses
            // onto the ok edge — the store traps where the shape panicked.
            None => {
                let inst = &mut b.insts[call_pos];
                inst.result = None;
                inst.op = Op::Native {
                    symbol: "neon_list_set_inplace".into(),
                    args: vec![list, index, elem],
                };
                b.term = Term::Jump(site.leaf.ok.clone());
                dead.insert(site.leaf.is_err);
                dead.extend(site.leaf.unwrap_err);
            }
            // Caught: the handler must stay reachable, so the write splits on an
            // explicit bounds check. In bounds — the only path that can occur more
            // than once per loop — the store goes in place and jumps to the ok edge.
            // Out of bounds, the ORIGINAL call runs in a slow block and throws the
            // stdlib's own IndexError to the original branch, so the handler sees
            // exactly the error text and value it always saw. The slow path's ok edge
            // survives as dead-but-well-formed IR: the call only runs when it must
            // throw.
            Some(_) => {
                let tail = b.insts.split_off(call_pos);
                let i64_ty = f.value_ty(index);
                let bool_ty = f.value_ty(site.leaf.is_err);
                let len = f.new_value(Repr::I64, i64_ty);
                let zero = f.new_value(Repr::I64, i64_ty);
                let ge = f.new_value(Repr::Bool, bool_ty);
                let lt = f.new_value(Repr::Bool, bool_ty);
                let inb = f.new_value(Repr::Bool, bool_ty);
                let fast = BlockId(f.blocks.len() as u32);
                let slow = BlockId(f.blocks.len() as u32 + 1);

                let b = &mut f.blocks[site.leaf.block.0 as usize];
                let old_term = std::mem::replace(
                    &mut b.term,
                    Term::Branch {
                        cond: inb,
                        then: Target {
                            to: fast,
                            args: vec![],
                        },
                        els: Target {
                            to: slow,
                            args: vec![],
                        },
                    },
                );
                b.insts.push(Inst {
                    result: Some(len),
                    op: Op::Native {
                        symbol: "neon_list_len".into(),
                        args: vec![list],
                    },
                });
                b.insts.push(Inst {
                    result: Some(zero),
                    op: Op::ConstI64(0),
                });
                b.insts.push(Inst {
                    result: Some(ge),
                    op: Op::Prim(PrimOp::Ge, vec![index, zero]),
                });
                b.insts.push(Inst {
                    result: Some(lt),
                    op: Op::Prim(PrimOp::Lt, vec![index, len]),
                });
                b.insts.push(Inst {
                    result: Some(inb),
                    op: Op::Prim(PrimOp::And, vec![ge, lt]),
                });
                f.blocks.push(Block {
                    id: fast,
                    params: vec![],
                    insts: vec![Inst {
                        result: None,
                        op: Op::Native {
                            symbol: "neon_list_set_inplace".into(),
                            args: vec![list, index, elem],
                        },
                    }],
                    term: Term::Jump(site.leaf.ok.clone()),
                });
                f.blocks.push(Block {
                    id: slow,
                    params: vec![],
                    insts: tail,
                    term: old_term,
                });
            }
        }
        if let Some(ok) = site.leaf.unwrap_ok {
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
