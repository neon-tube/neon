//! Covered bounds checks: when a loop indexes two lists with the same subscript, one
//! check can serve both — conditionally, so nothing about where a program traps ever
//! changes.
//!
//! The shape this pays for is brainfuck's dispatch loop (TODO.md, "what the brainfuck
//! profile says to build"): `kinds[i]` runs every iteration and `args[i]` runs on most
//! paths after it. Each access carries its own check, and gcc cannot merge them — they
//! guard DIFFERENT lists, and `len(args) >= len(kinds)` is a fact it has no way to
//! learn. Measured there, the second check is the whole remaining gap to C that any
//! sound pass can claim: ~11%.
//!
//! What gets emitted. For a covered access `b2[i]` dominated by a checked `b1[i]` in
//! the same loop, the preheader computes `covered = len(b2) >= len(b1)` once, and the
//! access becomes `neon_list_at_scalar_covered(b2, i, sz, covered)` — checked when
//! `covered` is false, checkless when it is true. The check it elides is provably
//! redundant there: `b1[i]` did not trap, so `0 <= i < len(b1) <= len(b2)`. Since
//! `covered` is loop-invariant, gcc unswitches the loop and the fast copy carries no
//! check at all; the pass never clones a block itself.
//!
//! Why conditional rather than the plan's original `min(len, len)` hoist: the second
//! read may sit on a BRANCH. Widening the first check to `min` would trap iterations
//! whose path never reads `b2` — a program that runs today would abort. The TODO's
//! claim of observational identity holds only when the second read post-dominates the
//! first, which in the motivating loop it does not (`:open` on a zero cell skips
//! `args[i]`). The conditional form is identical BY CONSTRUCTION on both sides of the
//! test: `covered` true elides only a check that could not have fired; `covered` false
//! is byte-for-byte today's access.
//!
//! Soundness requirements, all checked structurally:
//!   - the coverer dominates the covered access, with the SAME index SSA value — same
//!     value, same integer, no reasoning about mutation required;
//!   - the coverer's own check is unconditional (not itself covered);
//!   - both lists are defined outside the loop, so their lengths are loop-invariant
//!     and computable in the preheader (`neon_list_set_inplace`, the one write
//!     `ir::unique` introduces, never changes a length);
//!   - the loop has exactly one entering edge to put the test on.

use super::partial::{defined_inside, dominators, loop_body, predecessors, targets};
use super::repr::Repr;
use super::ssa::{BlockId, Func, Inst, Op, PrimOp, Program, Value};
use std::collections::{HashMap, HashSet};

pub fn apply(program: &mut Program) {
    for f in &mut program.funcs {
        cover_func(f);
    }
}

/// One planned rewrite: the access at `block`/`pos` becomes covered by the preheader
/// fact `len(base) >= len(coverer_base)`, computed at the end of `preheader`.
struct Plan {
    block: BlockId,
    pos: usize,
    base: Value,
    coverer_base: Value,
    preheader: BlockId,
    /// Any i64-typed value, for minting the length values' `TyId`.
    index: Value,
}

/// A checked list access, as found.
struct Site {
    block: BlockId,
    pos: usize,
    base: Value,
    index: Value,
}

fn cover_func(f: &mut Func) {
    let dom = dominators(f);
    let preds = predecessors(f);

    // Where every value is defined, for "is its length computable in the preheader".
    let mut def_block: HashMap<Value, BlockId> = HashMap::new();
    for &p in &f.params {
        def_block.insert(p, f.entry);
    }
    for b in &f.blocks {
        for &p in &b.params {
            def_block.insert(p, b.id);
        }
        for i in &b.insts {
            if let Some(r) = i.result {
                def_block.insert(r, b.id);
            }
        }
    }

    // Loop headers: any edge whose source the target dominates is a back edge.
    let mut headers: Vec<BlockId> = f
        .blocks
        .iter()
        .flat_map(|b| {
            targets(&b.term)
                .into_iter()
                .filter(|t| dom.get(&b.id).is_some_and(|d| d.contains(&t.to)))
                .map(|t| t.to)
        })
        .collect();
    headers.sort_by_key(|b| b.0);
    headers.dedup();

    let mut plans: Vec<Plan> = Vec::new();
    let mut taken: HashSet<(BlockId, usize)> = HashSet::new();

    for &header in &headers {
        let body = loop_body(f, header);
        // The single entering edge; a loop reachable two ways gets no test.
        let outside: Vec<BlockId> = preds
            .get(&header)
            .into_iter()
            .flatten()
            .filter(|p| !body.contains(p))
            .copied()
            .collect();
        let [preheader] = outside[..] else { continue };

        let mut sites: Vec<Site> = Vec::new();
        for b in &f.blocks {
            if !body.contains(&b.id) {
                continue;
            }
            for (pos, inst) in b.insts.iter().enumerate() {
                if let Op::Index {
                    base,
                    index,
                    covered: None,
                } = inst.op
                {
                    if matches!(f.value_repr(base), Repr::List(_)) {
                        sites.push(Site {
                            block: b.id,
                            pos,
                            base,
                            index,
                        });
                    }
                }
            }
        }

        for j in 0..sites.len() {
            let s2 = &sites[j];
            if taken.contains(&(s2.block, s2.pos)) || defined_inside(f, &body, s2.base) {
                continue;
            }
            let Some(&d2) = def_block.get(&s2.base) else {
                continue;
            };
            if !dom.get(&preheader).is_some_and(|d| d.contains(&d2)) {
                continue;
            }
            for s1 in &sites {
                if s1.base == s2.base || s1.index != s2.index {
                    continue;
                }
                // A coverer whose own check went conditional covers nothing.
                if taken.contains(&(s1.block, s1.pos)) || defined_inside(f, &body, s1.base) {
                    continue;
                }
                let dominates = if s1.block == s2.block {
                    s1.pos < s2.pos
                } else {
                    dom.get(&s2.block).is_some_and(|d| d.contains(&s1.block))
                };
                if !dominates {
                    continue;
                }
                let Some(&d1) = def_block.get(&s1.base) else {
                    continue;
                };
                if !dom.get(&preheader).is_some_and(|d| d.contains(&d1)) {
                    continue;
                }
                plans.push(Plan {
                    block: s2.block,
                    pos: s2.pos,
                    base: s2.base,
                    coverer_base: s1.base,
                    preheader,
                    index: s2.index,
                });
                taken.insert((s2.block, s2.pos));
                break;
            }
        }
    }

    // Apply: one preheader test per (preheader, coverer, covered) pair, shared by every
    // access it covers.
    let mut minted: HashMap<(BlockId, Value, Value), Value> = HashMap::new();
    for plan in plans {
        let key = (plan.preheader, plan.coverer_base, plan.base);
        let cov = match minted.get(&key) {
            Some(&c) => c,
            None => {
                let ty = f.value_ty(plan.index);
                let l1 = f.new_value(Repr::I64, ty);
                let l2 = f.new_value(Repr::I64, ty);
                let c = f.new_value(Repr::Bool, ty);
                let ph = &mut f.blocks[plan.preheader.0 as usize];
                ph.insts.push(Inst {
                    result: Some(l1),
                    op: Op::Native {
                        symbol: "neon_list_len".into(),
                        args: vec![plan.coverer_base],
                    },
                });
                ph.insts.push(Inst {
                    result: Some(l2),
                    op: Op::Native {
                        symbol: "neon_list_len".into(),
                        args: vec![plan.base],
                    },
                });
                ph.insts.push(Inst {
                    result: Some(c),
                    op: Op::Prim(PrimOp::Ge, vec![l2, l1]),
                });
                minted.insert(key, c);
                c
            }
        };
        if let Op::Index { covered, .. } = &mut f.blocks[plan.block.0 as usize].insts[plan.pos].op {
            *covered = Some(cov);
        }
    }
}
