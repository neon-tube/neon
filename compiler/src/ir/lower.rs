//! Lowering: the typed AST plus `TypecheckResult` become SSA. Nothing here re-derives
//! a type or re-resolves a call — every expression's type is read from `expr_types` and
//! turned into a `Repr`, and every dispatched call's decision is read from its
//! `Resolution`. See `docs/design/ir.md`.
//!
//! Lowering is also where monomorphisation happens. A generic function is never lowered
//! from its declaration; each call site solves its type arguments, mangles a name from
//! them, and queues an `InstanceJob`, so only the instances a program actually reaches
//! are emitted. Lambdas work the same way through `LambdaJob`, and both worklists are
//! drained to a fixpoint in `lower_module` — an instance can discover further lambdas and
//! instances, so the loop keeps running until neither has anything left.
//!
//! A form that has no lowering yet does not abort the pass: `unhandled_note` emits a
//! `<todo: ...>` string constant of the expected repr so the enclosing function still
//! lowers and `compiler/tests/ir_lower.rs` can report what remains.

use super::repr::{normalize_union, repr_of, Repr};
use super::ssa::{Builder, BlockId, Func, Op, PrimOp, Program, Target, Term, Value};
use crate::ast::{
    self, BinOp, Block, Decl, DeclKind, Expr, ExprId, ExprKind, Stmt, StmtKind, UnOp,
};
use crate::typecheck::env::Env;
use crate::typecheck::result::TypecheckResult;
use crate::typecheck::types::TyId;

/// A lambda whose body is lowered as its own function, discovered while lowering an
/// enclosing one. The captures are the free variables it closes over, in order.
struct LambdaJob {
    name: String,
    lambda: Expr,
    captures: Vec<(String, Repr, TyId)>,
    module: Vec<String>,
    /// The enclosing instance's substitution. A lambda written inside a generic mentions
    /// that generic's type parameters -- in its own parameter types, and in whatever its
    /// body does with them -- so it monomorphises *with* its enclosing function rather
    /// than once for all instantiations. Empty for a lambda in a non-generic function.
    subst: std::collections::HashMap<String, Repr>,
}

/// A concrete instance of a generic function, discovered at a call site. Monomorphisation
/// specialises the generic body under `subst`, mapping each generic parameter name to the
/// concrete `Repr` it was called with.
#[derive(Clone)]
struct InstanceJob {
    mangled: String,
    module: Vec<String>,
    fn_name: String,
    subst: std::collections::HashMap<String, Repr>,
    /// For a generic *impl* method, the `protocol$head$method` key that finds its body.
    /// A protocol's methods can be generic independently of the impl (`Mappable::map[T,U]`),
    /// so they monomorphise per call site exactly like a generic module function.
    impl_key: Option<String>,
}

/// Replace every type variable in a repr with its concrete binding. After this a
/// monomorphic instance has no `Var` left.
/// Whether a repr still mentions an unsubstituted type variable. A back-edge is not
/// walked: it names a type rather than describing one, and following it would not
/// terminate.
fn contains_var(r: &Repr) -> bool {
    match r {
        Repr::Var(_) => true,
        Repr::Record { fields, .. } => fields.iter().any(|(_, f)| contains_var(f)),
        Repr::Tuple(es) | Repr::Union(es) => es.iter().any(contains_var),
        Repr::List(e) | Repr::Nullable(e) => contains_var(e),
        Repr::Map(k, v) => contains_var(k) || contains_var(v),
        Repr::Runtime { args, .. } => args.iter().any(contains_var),
        Repr::Closure { params, throws, ret } => {
            params.iter().any(contains_var) || contains_var(throws) || contains_var(ret)
        }
        _ => false,
    }
}

fn substitute_repr(r: &Repr, subst: &std::collections::HashMap<String, Repr>) -> Repr {
    match r {
        Repr::Var(n) => subst.get(n).cloned().unwrap_or_else(|| r.clone()),
        Repr::Record { name, fields } => Repr::Record {
            name: name.clone(),
            fields: fields.iter().map(|(n, r)| (n.clone(), substitute_repr(r, subst))).collect(),
        },
        Repr::Tuple(rs) => Repr::Tuple(rs.iter().map(|r| substitute_repr(r, subst)).collect()),
        // Re-normalise: substituted variants may collapse (`T | null` with a pointer `T`
        // is a nullable pointer), coincide, or land in a different order than the concrete
        // type's — variables are collected after the base bits, so `T | null` substitutes
        // to `[null, i64]` while `i64 | null` is `[i64, null]`. Left alone, one type gets
        // two different C structs.
        Repr::Union(rs) => crate::ir::repr::normalize_union(
            rs.iter().map(|r| substitute_repr(r, subst)).collect(),
        ),
        Repr::List(e) => Repr::List(Box::new(substitute_repr(e, subst))),
        // Substitution reaches the arguments only: the nominal name and the C symbol name
        // the type itself, and `Resource[T, E]` instantiated at `i64` is still a
        // `Resource`. Both ride through untouched, together, so neither can drift.
        Repr::Runtime { nominal, c_type, args } => Repr::Runtime {
            nominal: nominal.clone(),
            c_type: c_type.clone(),
            args: args.iter().map(|r| substitute_repr(r, subst)).collect(),
        },
        Repr::Nullable(e) => Repr::Nullable(Box::new(substitute_repr(e, subst))),
        Repr::Map(k, v) => {
            Repr::Map(Box::new(substitute_repr(k, subst)), Box::new(substitute_repr(v, subst)))
        }
        Repr::Closure { params, throws, ret } => Repr::Closure {
            params: params.iter().map(|r| substitute_repr(r, subst)).collect(),
            throws: Box::new(substitute_repr(throws, subst)),
            ret: Box::new(substitute_repr(ret, subst)),
        },
        _ => r.clone(),
    }
}

/// Bind the type variables in a template repr to make it match a concrete one, for
/// building a call's instance substitution from its argument reprs.
fn match_repr(template: &Repr, concrete: &Repr, subst: &mut std::collections::HashMap<String, Repr>) {
    match (template, concrete) {
        (Repr::Var(n), c) => {
            subst.entry(n.clone()).or_insert_with(|| c.clone());
        }
        (Repr::List(a), Repr::List(b)) | (Repr::Nullable(a), Repr::Nullable(b)) => {
            match_repr(a, b, subst)
        }
        // Both halves must agree, not just the nominal. They are a pure function of one
        // another, so this is equivalent to testing `nominal` alone — it is written out so
        // the invariant is checked rather than assumed.
        (
            Repr::Runtime { nominal: an, c_type: ac, args: aa },
            Repr::Runtime { nominal: bn, c_type: bc, args: ba },
        ) if an == bn && ac == bc =>
        {
            aa.iter().zip(ba).for_each(|(x, y)| match_repr(x, y, subst));
        }
        (Repr::Map(ak, av), Repr::Map(bk, bv)) => {
            match_repr(ak, bk, subst);
            match_repr(av, bv, subst);
        }
        (Repr::Record { fields: a, .. }, Repr::Record { fields: b, .. }) => {
            for ((_, at), (_, bt)) in a.iter().zip(b) {
                match_repr(at, bt, subst);
            }
        }
        (Repr::Tuple(a), Repr::Tuple(b)) | (Repr::Union(a), Repr::Union(b)) => {
            a.iter().zip(b).for_each(|(x, y)| match_repr(x, y, subst));
        }
        // A nullable parameter (`T | null`) accepts a non-null argument: match the
        // variable in the non-null half against the concrete argument.
        (Repr::Nullable(a), c) => match_repr(a, c, subst),
        (Repr::Union(ts), c) => {
            for t in ts {
                if matches!(t, Repr::Var(_)) {
                    match_repr(t, c, subst);
                }
            }
        }
        (
            Repr::Closure { params: ap, throws: at, ret: ar },
            Repr::Closure { params: bp, throws: bt, ret: br },
        ) => {
            ap.iter().zip(bp).for_each(|(x, y)| match_repr(x, y, subst));
            match_repr(at, bt, subst);
            match_repr(ar, br, subst);
        }
        _ => {}
    }
}

/// A repr for a turbofish type argument, read from its syntax.
///
/// INJECTIVITY OBLIGATION — CURRENTLY VIOLATED. The old comment here read "only the head
/// is needed for a generic instance (mangling and bound-dispatch resolution)". That is
/// true of bound-dispatch resolution and FALSE of mangling, and the two were run
/// together. This result feeds `mangle_instance` → `repr_key` → the `lowered` dedup set,
/// where it is the IDENTITY of a monomorphisation instance, so two type arguments
/// colliding here means the second body is dropped and one instance serves call sites
/// with different layouts.
///
/// Three arms are lossy:
///   * `Named` with args builds `Record { fields: vec![] }` — a generic record carries
/// ```text
///     its arguments in its FIELDS, so `Box[i64]` and `Box[str]` become one repr.
/// ```
///   * `path.last()` drops the module, so `a::Point` and `b::Point` agree.
///   * `_ => Repr::Any` sends every tuple and arrow typespec to one value.
///
/// `repr_key`'s own obligation therefore holds over `Repr` and buys nothing here: an
/// injective function of an already-collapsed input is still collapsed.
///
/// Reproducer — there is no corpus file because the ratchet has no known-bug marker and
/// this program should eventually PASS, which `expected-pass.txt` cannot express:
///
/// ```text
///     record Box[T] { item: T }
///     fn ident[T](x: T) -> T { x }
///     fn main() {
///         let bi = Box { item: 7 };
///         let bs = Box { item: "hi" };
///         io::println("#{ident[Box[i64]](bi).item}");
///         io::println("#{ident[Box[str]](bs).item}");
///     }
/// ```
///
/// `neon ir` shows the collision directly — two call sites, one body:
///
/// ```text
///     %9  = call @ident$Box(%4)      // the Box[i64] call site
///     %10 = call @ident$Box(%8)      // the Box[str] call site
///     fn @ident$Box(%0 Box{item: str}) -> Box{item: str}
/// ```
///
/// and it currently dies at `incompatible types when assigning to type 'nr0' from type
/// 'nr1'`. gcc's nominal struct typing is the only thing standing between this and a
/// silent miscompile: two `Box` instantiations get separately interned structs even when
/// their layouts coincide (`Box[List[i64]]` and `Box[List[str]]` are both one pointer,
/// and are still `nr0`/`nr1`). That backstop holds only while no two instantiations
/// reach one interned struct — the same footing `repr_key`'s union collision had, where
/// `i64|bool` and `i64|f64` did coincide and it compiled silently.
///
/// The fix is not a cleverer projection: a repr cannot be rebuilt faithfully from a
/// typespec, because the field names that give a record its identity are not in the
/// syntax. It is to use the type the checker already resolved, as the non-turbofish path
/// does, and to stop deriving a third spelling of the type from syntax at all.
fn repr_from_typespec(spec: &ast::TypeSpec) -> Repr {
    match &spec.kind {
        ast::TypeSpecKind::Null => Repr::Null,
        ast::TypeSpecKind::Named { path, args } => {
            let name = path.last().map(String::as_str).unwrap_or("");
            match name {
                "i64" => Repr::I64,
                "f64" => Repr::F64,
                "str" => Repr::Str,
                "bool" => Repr::Bool,
                "List" => Repr::List(Box::new(args.first().map_or(Repr::Any, repr_from_typespec))),
                "Map" => Repr::Map(
                    Box::new(args.first().map_or(Repr::Any, repr_from_typespec)),
                    Box::new(args.get(1).map_or(Repr::Any, repr_from_typespec)),
                ),
                // The head alone is not the identity: `repr_key` spells a generic
                // record through its fields, so the arguments ride along as arg-slot
                // pseudo-fields — without them, `ident[Box[i64]]` and `ident[Box[str]]`
                // mangled to one monomorphisation instance, and only gcc's duplicate-
                // symbol complaint stood between that and one body serving both.
                other => Repr::Record {
                    name: Some(other.to_string()),
                    fields: args
                        .iter()
                        .enumerate()
                        .map(|(i, a)| (format!("#{i}"), repr_from_typespec(a)))
                        .collect(),
                },
            }
        }
        _ => Repr::Any,
    }
}

/// The mangled name of a generic instance: the base name with its concrete arguments.
fn mangle_instance(base: &str, subst: &std::collections::HashMap<String, Repr>) -> String {
    let mut keys: Vec<&String> = subst.keys().collect();
    keys.sort();
    let args: Vec<String> = keys.iter().map(|k| repr_key(&subst[*k])).collect();
    format!("{base}${}", args.join("$"))
}

/// A short, stable spelling of a repr for a mangled name.
///
/// INJECTIVITY OBLIGATION: this is an identity — it is the name of a monomorphisation
/// instance, and `lower_module`'s `lowered` set dedups on it, so two substitutions
/// sharing a key means the second body is DROPPED and one emitted instance serves call
/// sites that agreed with the compiler on a different layout. Every arm spells its
/// variant's components; nothing is elided. It is injective over `Repr` modulo the
/// `Recursive`/`BoxedRec` back-edges, which carry an id and are compared by it.
///
/// The separator is `_`, which identifiers may also contain, so injectivity here is
/// WEAKER than `ctype::key`'s bracketed scheme and is the part worth watching: a record
/// `A_B` with no fields and a record `A` with a single field of type `B`... do not in
/// fact collide, because the field arm spells `{field}_{type}` and so carries the field
/// name too — but the margin is thin and it is arity, not structure, that separates
/// them. If this ever needs to grow an arm, bracket it rather than adding another `_`.
///
/// Same caveat as `ctype::type_tag_name` on nominal names: `Record { name: Some(n), .. }`
/// spells the bare `n`, so two modules declaring one record name collide. See
/// `tests/lang/types/a_nominal_name_is_not_a_module_identity.neon`.
///
/// THE TELL, for future readers hunting this class of bug: a `match` over a
/// structured type whose arms return string or integer constants, where the result is
/// used as a name, key or tag. Every such function is a lossy projection wearing an
/// identity's job, and should carry an injectivity obligation in its doc — backed by
/// an assertion or a spelled-out structure, not prose. The class bottoms out at the
/// qualified declaration key; anything shorter is a fragment.
fn repr_key(r: &Repr) -> String {
    match r {
        Repr::I64 => "i64".into(),
        Repr::F64 => "f64".into(),
        Repr::Bool => "bool".into(),
        Repr::Str => "str".into(),
        Repr::Null => "null".into(),
        Repr::Unit => "unit".into(),
        Repr::Tag => "tag".into(),
        // The Neon name, matching what `Record` does one arm below: a mangled name spells
        // the *type*, and two distinct Neon types that happened to share a C symbol would
        // otherwise mangle to one monomorphisation instance.
        Repr::Runtime { nominal, args, .. } if args.is_empty() => nominal.clone(),
        Repr::Runtime { nominal, args, .. } => {
            format!("{nominal}_{}", args.iter().map(repr_key).collect::<Vec<_>>().join("_"))
        }
        // A generic record carries its arguments in its *fields* — `Box[i64]` and
        // `Box[str]` are both `Record { name: Some("Box"), .. }` — so the name alone is
        // not the type. `ctype::tag_name` spells a nominal record the same way for the
        // same reason; keying on the name alone gave one `unwrap$Box` body to both.
        Repr::Record { name: Some(n), fields } if fields.is_empty() => n.clone(),
        Repr::Record { name: Some(n), fields } => {
            format!("{n}_{}", fields.iter().map(|(_, r)| repr_key(r)).collect::<Vec<_>>().join("_"))
        }
        // An anonymous record has no name at all; its fields are its whole identity.
        Repr::Record { fields, .. } => format!(
            "rec_{}",
            fields
                .iter()
                .map(|(n, r)| format!("{n}_{}", repr_key(r)))
                .collect::<Vec<_>>()
                .join("_")
        ),
        Repr::List(e) => format!("list_{}", repr_key(e)),
        Repr::Map(k, v) => format!("map_{}_{}", repr_key(k), repr_key(v)),
        Repr::Tuple(rs) => format!("tup_{}", rs.iter().map(repr_key).collect::<Vec<_>>().join("_")),
        Repr::Nullable(e) => format!("opt_{}", repr_key(e)),
        // Structural, like `Tuple` and `Map` above, and for the same reason: this string
        // is the *identity* of a monomorphisation instance. `Closure { .. } => "fn"` and
        // `Union(_) => "union"` were constants, so every instantiation of one generic at
        // any two closure types — or any two union types — mangled to a single name, and
        // `lower_module`'s `lowered` set then dropped the second body on the floor. One
        // emitted instance, typed at whichever substitution happened to be popped first,
        // served call sites that had agreed with the compiler on a different layout.
        //
        // It is not reliably caught downstream. `fn ident[T](x: T) -> T` at `i64 | str`
        // and at `bool | f64` produced two C structs and the C compiler rejected the
        // mismatch; the same collision at `i64 | bool` and `i64 | f64` produced one struct
        // and compiled silently, correct only by coincidence of layout.
        //
        // Recursion terminates: the two back-edge reprs below carry an id and nothing to
        // descend into, and every cycle in a type graph passes through one of them.
        Repr::Closure { params, throws, ret } => format!(
            "fn_{}_{}_{}",
            params.iter().map(repr_key).collect::<Vec<_>>().join("_"),
            repr_key(throws),
            repr_key(ret)
        ),
        Repr::Union(vs) => {
            format!("union_{}", vs.iter().map(repr_key).collect::<Vec<_>>().join("_"))
        }
        Repr::Var(n) => n.clone(),
        Repr::Recursive(t) => format!("mu{}", t.0),
        Repr::BoxedRec(a) => format!("box{a}"),
        Repr::Any => "any".into(),
        Repr::Never => "never".into(),
    }
}

/// Lower a whole module to a program of SSA functions.
///
/// `libs` are the modules the program was checked against — the stdlib — with their module
/// paths: their function *bodies* must be lowered too, because the stdlib is no longer only
/// `@native` signatures. A concrete function is lowered outright; a generic one only per
/// instance, as its call sites discover it.
///
/// Lambdas and generic instances are both worklists, drained after the concrete functions
/// and impl methods are done, because lowering either can discover more of both. `lowered`
/// deduplicates by mangled name, which is what stops a recursive generic (an instance whose
/// body calls itself at the same type) from queueing itself forever.
pub fn lower_module<'a>(
    env: &Env,
    result: &TypecheckResult,
    module: &'a ast::Module,
    libs: &[(Vec<String>, &'a ast::Module)],
) -> Program {
    lower_module_with(env, result, module, libs, false)
}

/// One `test "name" { .. }` block, in the order `lower_module_with` emits them.
///
/// `symbol` is the IR function name, which the C backend mangles like any other; the test
/// runner never invents it, so the two sides cannot drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestEntry {
    pub name: String,
    pub symbol: String,
}

/// Every `test` block in a module, in source order, descending into nested `mod`s.
///
/// The single source of truth for both what gets lowered and what the runner is told
/// exists — `neon test` calls this to learn the names, and lowering calls it to decide what
/// to emit, so an index means the same block on both sides.
pub fn test_entries(module: &ast::Module) -> Vec<TestEntry> {
    fn walk(decls: &[Decl], out: &mut Vec<TestEntry>) {
        for d in decls {
            match &d.kind {
                DeclKind::TestBlock(t) if t.kind == ast::TestKind::Test => {
                    let symbol = format!("__neon_test_{}", out.len());
                    out.push(TestEntry { name: t.name.clone(), symbol });
                }
                DeclKind::Mod(m) => walk(&m.decls, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&module.decls, &mut out);
    out
}

/// `lower_module`, with `tests` selecting whether `test` blocks become functions.
///
/// Off (a normal build) they are stripped, which is what `test` blocks have always done.
/// On (`neon test`) each becomes a nullary unit-returning function; nothing calls them from
/// Neon, so the backend's test entry point is what reaches them.
pub fn lower_module_with<'a>(
    env: &Env,
    result: &TypecheckResult,
    module: &'a ast::Module,
    libs: &[(Vec<String>, &'a ast::Module)],
    tests: bool,
) -> Program {
    let mut funcs = Vec::new();
    let mut lambda_jobs: Vec<LambdaJob> = Vec::new();
    let mut instance_jobs: Vec<InstanceJob> = Vec::new();
    let mut lowered: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Every function body, keyed by (module, name), so a generic instance can find its
    // source when its call site discovers it.
    let mut all_fns: std::collections::HashMap<(Vec<String>, String), ast::FnDecl> =
        std::collections::HashMap::new();
    collect_all_fns(&[], &module.decls, &mut all_fns);
    for (path, m) in libs {
        collect_all_fns(path, &m.decls, &mut all_fns);
    }

    // Non-generic top-level functions. A generic one is lowered only per instance, as its
    // call sites discover them.
    let mut fn_jobs: Vec<(Vec<String>, &'a ast::FnDecl)> = Vec::new();
    collect_fn_jobs(&[], &module.decls, &mut fn_jobs);
    for (path, m) in libs {
        collect_fn_jobs(path, &m.decls, &mut fn_jobs);
    }
    for (m, f) in fn_jobs {
        if !f.generics.is_empty() {
            continue;
        }
        let (func, l, i) = lower_fn(env, result, &m, f);
        lowered.insert(func.name.clone());
        funcs.push(func);
        lambda_jobs.extend(l);
        instance_jobs.extend(i);
    }

    // `test` blocks, in test mode only. Each is a nullary unit function; the entry point
    // the backend emits is the only caller.
    if tests {
        let mut blocks: Vec<(Vec<String>, &'a ast::TestBlock)> = Vec::new();
        collect_test_blocks(&[], &module.decls, &mut blocks);
        for (entry, (m, t)) in test_entries(module).into_iter().zip(blocks) {
            let (func, l, i) = lower_test_block(env, result, &m, t, entry.symbol);
            lowered.insert(func.name.clone());
            funcs.push(func);
            lambda_jobs.extend(l);
            instance_jobs.extend(i);
        }
    }

    // Impl methods: correlate each `ImplDef`'s method (which carries the types) with its
    // AST body (which carries the code) through the same mangled name that dispatch uses.
    let mut impl_bodies: std::collections::HashMap<String, &ast::FnDecl> =
        std::collections::HashMap::new();
    collect_impl_bodies(env, &[], &module.decls, &mut impl_bodies);
    for (prefix, m) in libs {
        collect_impl_bodies(env, prefix, &m.decls, &mut impl_bodies);
    }
    for impl_def in env.impls() {
        let proto = env.protocols()[impl_def.protocol.0].name.clone();
        let head = impl_head(env, impl_def);
        for m in &impl_def.methods {
            if !m.has_body || !m.generics.is_empty() {
                continue;
            }
            let name = mangle_impl(&proto, &head, &m.name);
            if let Some(fd) = impl_bodies.get(&name) {
                let (func, l, i) = lower_method(env, result, &impl_def.module, fd, m, name);
                lowered.insert(func.name.clone());
                funcs.push(func);
                lambda_jobs.extend(l);
                instance_jobs.extend(i);
            }
        }
    }

    // Drain the worklists: lambdas and generic instances, deduplicated by name.
    loop {
        if let Some(job) = lambda_jobs.pop() {
            if !lowered.insert(job.name.clone()) {
                continue;
            }
            let (func, l, i) = lower_lambda_job(env, result, job);
            funcs.push(func);
            lambda_jobs.extend(l);
            instance_jobs.extend(i);
            continue;
        }
        if let Some(job) = instance_jobs.pop() {
            if !lowered.insert(job.mangled.clone()) {
                continue;
            }
            if let Some((func, l, i)) = lower_instance(env, result, &all_fns, &impl_bodies, job) {
                funcs.push(func);
                lambda_jobs.extend(l);
                instance_jobs.extend(i);
            }
            continue;
        }
        break;
    }
    // Every recursive type the program mentions. Collected from the checker's expression
    // types rather than from the lowered reprs: a back-edge carries only a `TyId`, so by
    // the time it is in the IR the unfolding it names is no longer reachable.
    let mut recursive = std::collections::HashMap::new();
    let mut boxed = std::collections::HashMap::new();
    for (_, ty) in result.types() {
        crate::ir::repr::recursive_unfoldings(&env.solver.t, ty, &mut recursive);
        crate::ir::repr::boxed_shapes(&env.solver.t, ty, &mut boxed);
    }
    // Every `@pure` native's symbol, for the effect analysis. Declared purity only:
    // a native with no annotation is effectful, which is the safe direction.
    let pure_natives = env
        .fns()
        .iter()
        .filter(|s| s.pure)
        .filter_map(|s| s.native.clone())
        .collect();
    // `@inline` fns, by the mangled-in name codegen knows them by. A generic fn is
    // lowered once per instantiation with a suffixed name, so match on the prefix rather
    // than on equality -- otherwise `list::set` gets it and `list::set$i64` does not,
    // which is exactly backwards: the instance is what the hot loop calls.
    // Through `mangle`, because a `FnSig` carries its module separately from its name
    // while a `Func` carries the two already joined -- comparing the bare `set` against
    // `std__collections__list__set` matches nothing, silently.
    let inline_bases: Vec<String> =
        env.fns().iter().filter(|s| s.inline).map(|s| mangle(&s.module, &s.name)).collect();
    let inlined = funcs
        .iter()
        .map(|f| f.name.clone())
        .filter(|n| {
            inline_bases.iter().any(|b| n == b || n.starts_with(&format!("{b}$")))
        })
        .collect();
    Program { funcs, recursive, boxed, pure_natives, inlined }
}

/// Index every bodied function by `(module path, name)`, descending into nested `mod`s.
/// This is the table `lower_instance` looks a generic body up in, so it must cover the
/// stdlib and every nested module, not just the ones with concrete functions to lower.
/// Signature-only declarations (`@native`, protocol methods) have no body and are skipped.
fn collect_all_fns(
    module: &[String],
    decls: &[Decl],
    out: &mut std::collections::HashMap<(Vec<String>, String), ast::FnDecl>,
) {
    for d in decls {
        match &d.kind {
            DeclKind::Fn(f) if f.body.is_some() => {
                out.insert((module.to_vec(), f.name.clone()), f.clone());
            }
            DeclKind::Mod(m) => {
                let mut inner = module.to_vec();
                inner.push(m.name.clone());
                collect_all_fns(&inner, &m.decls, out);
            }
            _ => {}
        }
    }
}

/// Lower one concrete instance of a generic function under its substitution.
fn lower_instance(
    env: &Env,
    result: &TypecheckResult,
    all_fns: &std::collections::HashMap<(Vec<String>, String), ast::FnDecl>,
    impl_bodies: &std::collections::HashMap<String, &ast::FnDecl>,
    job: InstanceJob,
) -> Option<(Func, Vec<LambdaJob>, Vec<InstanceJob>)> {
    // Either a generic module function or a generic impl method: the body and the
    // signature come from different places, the lowering below is the same.
    let (f, sig): (&ast::FnDecl, crate::typecheck::env::FnSig) = match &job.impl_key {
        Some(key) => {
            let f = *impl_bodies.get(key)?;
            let sig = impl_method_sig(env, key)?;
            (f, sig)
        }
        None => {
            let f = all_fns.get(&(job.module.clone(), job.fn_name.clone()))?;
            let sig = env.fn_named(&job.module, std::slice::from_ref(&job.fn_name))?.clone();
            (f, sig)
        }
    };
    let ret_repr = substitute_repr(&repr_of(&env.solver.t, sig.ret), &job.subst);
    let throws_ty = Some(sig.throws);

    let mut lo = Lower::with_subst(
        env,
        result,
        job.module.clone(),
        job.mangled.clone(),
        ret_repr.clone(),
        job.subst.clone(),
    );
    if let Some(t) = throws_ty {
        set_throws(&mut lo.b, env, t, &job.subst);
    }
    let mut params = Vec::new();
    for (i, p) in f.params.iter().enumerate() {
        let ty = sig.params.get(i).map(|(_, t)| *t).unwrap_or(TyId(0));
        let r = lo.repr_of_ty(ty);
        let v = lo.b.block_param(BlockId(0), r, ty);
        lo.bind(&p.name, v);
        params.push(v);
    }
    lo.b.switch_to(BlockId(0));
    let body = f.body.as_ref().expect("generic fn has a body");
    let tail = lo.lower_block(body);
    if !lo.terminated {
        let ret = if matches!(ret_repr, Repr::Unit) { None } else { tail };
        lo.b.terminate(Term::Ret(ret));
    }
    let (l, i) = (std::mem::take(&mut lo.pending), std::mem::take(&mut lo.instances));
    Some((lo.b.finish(params), l, i))
}

/// Every bodied function in declaration order, paired with the module path it sits in.
/// The generic ones are filtered out by the caller, not here, so `all_fns` and this list
/// stay the same shape.
fn collect_fn_jobs<'a>(
    module: &[String],
    decls: &'a [Decl],
    out: &mut Vec<(Vec<String>, &'a ast::FnDecl)>,
) {
    for d in decls {
        match &d.kind {
            DeclKind::Fn(f) if f.body.is_some() => out.push((module.to_vec(), f)),
            DeclKind::Mod(m) => {
                let mut inner = module.to_vec();
                inner.push(m.name.clone());
                collect_fn_jobs(&inner, &m.decls, out);
            }
            _ => {}
        }
    }
}

/// The mangled name of a function. Monomorphisation will refine this with concrete type
/// arguments; for now it is the module path and name.
fn mangle(module: &[String], name: &str) -> String {
    if module.is_empty() {
        name.to_string()
    } else {
        format!("{}__{name}", module.join("__"))
    }
}

/// The signature behind a `protocol$head$method` key, for lowering a generic impl instance.
/// The key is split rather than carried structurally because it is the same string
/// dispatch already mangles, so the two sides cannot drift apart; the cost is this linear
/// scan over every impl in the environment.
fn impl_method_sig(
    env: &Env,
    key: &str,
) -> Option<crate::typecheck::env::FnSig> {
    let mut parts = key.split('$');
    let (proto, head, method) = (parts.next()?, parts.next()?, parts.next()?);
    for impl_def in env.impls() {
        if env.protocols()[impl_def.protocol.0].name != proto || impl_head(env, impl_def) != head {
            continue;
        }
        if let Some(m) = impl_def.methods.iter().find(|m| m.name == method) {
            return Some(m.clone());
        }
    }
    None
}

/// The name of an impl method, agreed between the site that dispatches to it and the site
/// that lowers it: protocol, target head, and method. Both sides must derive the head the
/// same way — `impl_head` from the checked `ImplDef`, `ast_head` from the syntax — or a
/// dispatch emits a call to a function nothing ever defines and the link fails.
fn mangle_impl(protocol: &str, head: &str, method: &str) -> String {
    format!("{protocol}${head}${method}")
}

/// The head of an impl's target, for mangling — the nominal or primitive name.
///
/// INJECTIVITY OBLIGATION — PARTIAL, and the shortfall is the `_` arm. Collapsing type
/// ARGUMENTS is correct and deliberate: a protocol is implemented on a type constructor,
/// so `impl Container for Box` is one impl serving `Box[i64]` and `Box[str]`
/// (`tests/lang/protocols/generic_impl.neon`). The codomain is meant to be the set of
/// heads, and over nominals and primitives this is injective.
///
/// `_ => String::new()` is not that. A target with no nominal or primitive head — a
/// tuple, a union — is given the EMPTY STRING as its identity, and the empty string is a
/// real value in this key space, not an absence. One such impl works, because both sides
/// derive the same empty head; two mangle to one symbol.
///
/// Reproducer — no corpus file, because the ratchet has no known-bug marker and this
/// program should eventually PASS, which `expected-pass.txt` cannot express:
///
/// ```text
///     protocol Show for T { fn show(v: T) -> str }
///     impl Show for (i64, i64) { fn show(v: (i64, i64)) -> str { "int tuple" } }
///     impl Show for (str, str) { fn show(v: (str, str)) -> str { "str tuple" } }
/// ```
///
/// Both impls become `Show$$show`, emitted as `nl_Show_S_Sshow`, and gcc reports
/// `conflicting types for 'nl_Show_S_Sshow'; have 'neon_str(nt1)'` against a previous
/// declaration `'neon_str(nt0)'`. It survives only because each tuple type gets its own
/// interned C struct, so the C type system catches the duplicate — the same "correct
/// only by coincidence" footing the original `repr_key` union collision had.
///
/// Two further collapses, both real and both recorded elsewhere rather than fixed here:
/// a nominal head is the BARE record name, so two modules declaring one record name
/// share an impl symbol (see
/// `tests/lang/types/a_nominal_name_is_not_a_module_identity.neon`); and `mangle_impl`
/// keys the protocol by its bare name too, so two protocols named `Show` in different
/// modules collide on one head.
///
/// Whoever fixes the `_` arm must change `ast_head` and `repr_head` in the same commit.
/// The three derive one head from three different representations — the checked
/// `ImplDef`, the written syntax, the receiver's repr — and they must AGREE, or a
/// dispatch emits a call to a symbol nothing defines and the failure moves from the C
/// compiler to the linker, which is strictly worse.
fn impl_head(env: &Env, impl_def: &crate::typecheck::env::ImplDef) -> String {
    if let Some(h) = &impl_def.target_head {
        return h.clone();
    }
    match impl_def.target.map(|t| repr_of(&env.solver.t, t)) {
        Some(Repr::Record { name: Some(n), .. }) => n,
        // Everything else spells its full structure through `repr_key`, which carries
        // an injectivity obligation of its own. The `_ => String::new()` this replaces
        // collided every two tuple impls into one symbol; the remedy for a collapsing
        // key is always the same — spell the structure, never a fragment of it.
        Some(other) => repr_key(&other),
        None => String::new(),
    }
}

/// The head name of a repr, for matching a bound-dispatch receiver to an impl.
fn repr_head(r: &Repr) -> Option<String> {
    Some(match r {
        Repr::Record { name: Some(n), .. } => n.clone(),
        Repr::I64 => "i64".into(),
        Repr::F64 => "f64".into(),
        Repr::Str => "str".into(),
        Repr::Bool => "bool".into(),
        Repr::List(_) => crate::typecheck::env::Env::LIST.into(),
        Repr::Map(_, _) => crate::typecheck::env::Env::MAP.into(),
        _ => return None,
    })
}

/// Find the impl of `protocol` for a type whose head is `head`, and its method — its
/// native symbol (if any) and what it throws. This discharges a `where` bound once the
/// receiver is concrete.
/// The impl of `protocol` whose head is `head`, by id — the switch lowering re-enters
/// the ordinary `Direct` path with it.
fn find_impl_id(
    env: &Env,
    protocol: crate::typecheck::env::ProtocolId,
    head: &str,
) -> Option<crate::typecheck::env::ImplId> {
    env.impls()
        .iter()
        .position(|i| i.protocol == protocol && impl_head(env, i) == head)
        .map(crate::typecheck::env::ImplId)
}

fn find_impl_method(
    env: &Env,
    protocol: crate::typecheck::env::ProtocolId,
    head: &str,
    method: &str,
) -> Option<(Option<String>, TyId)> {
    for impl_def in env.impls() {
        if impl_def.protocol == protocol && impl_head(env, impl_def) == head {
            if let Some(m) = impl_def.methods.iter().find(|m| m.name == method) {
                return Some((m.native.clone(), m.throws));
            }
        }
    }
    None
}

/// The head of an impl target written in the AST, matching `impl_head`.
///
/// Resolved through the environment rather than read off the syntax. `impl_head` derives
/// its head from the checked `ImplDef`, whose identity is now the QUALIFIED name, so
/// taking `path.last()` here produced `Mappable$List$fold` where dispatch called
/// `Mappable$std::collections::list::List$fold` — the exact "call a symbol nothing
/// defines" failure the doc above warns about. It also means the two spellings of one
/// type (`List` and `std::collections::list::List`) now key the same body.
///
/// Falls back to the written last segment when the path does not resolve, which happens
/// only for a target the checker already rejected.
fn ast_head(env: &Env, module: &[String], ty: &ast::TypeSpec) -> String {
    match &ty.kind {
        ast::TypeSpecKind::Named { path, .. } => env
            .lookup(module, path)
            .unwrap_or_else(|| path.last().cloned().unwrap_or_default()),
        _ => String::new(),
    }
}

/// Index every impl method that has a body under the same `protocol$head$method` key
/// dispatch uses. The `ImplDef`s in the environment carry the types but not the code, and
/// the AST carries the code but not the resolved types; this key is what correlates them.
fn collect_impl_bodies<'a>(
    env: &Env,
    module: &[String],
    decls: &'a [Decl],
    out: &mut std::collections::HashMap<String, &'a ast::FnDecl>,
) {
    for d in decls {
        match &d.kind {
            DeclKind::Impl(i) => {
                let proto = i.protocol.last().cloned().unwrap_or_default();
                let head = ast_head(env, module, &i.target);
                for m in &i.methods {
                    if m.body.is_some() {
                        out.insert(mangle_impl(&proto, &head, &m.name), m);
                    }
                }
            }
            DeclKind::Mod(m) => {
                let mut inner = module.to_vec();
                inner.push(m.name.clone());
                collect_impl_bodies(env, &inner, &m.decls, out)
            }
            _ => {}
        }
    }
}

/// Lower an impl method, whose types come from its `FnSig` and whose code from the AST.
fn lower_method(
    env: &Env,
    result: &TypecheckResult,
    module: &[String],
    f: &ast::FnDecl,
    sig: &crate::typecheck::env::FnSig,
    name: String,
) -> (Func, Vec<LambdaJob>, Vec<InstanceJob>) {
    let ret_repr = repr_of(&env.solver.t, sig.ret);
    let mut lo = Lower::new(env, result, module.to_vec(), name, ret_repr.clone());
    set_throws(&mut lo.b, env, sig.throws, &Default::default());
    let mut params = Vec::new();
    for (i, p) in f.params.iter().enumerate() {
        let ty = sig.params.get(i).map(|(_, t)| *t).unwrap_or(TyId(0));
        let r = repr_of(&env.solver.t, ty);
        let v = lo.b.block_param(BlockId(0), r, ty);
        lo.bind(&p.name, v);
        params.push(v);
    }
    lo.b.switch_to(BlockId(0));
    let body = f.body.as_ref().expect("filtered to bodied methods");
    let tail = lo.lower_block(body);
    if !lo.terminated {
        let ret = if matches!(ret_repr, Repr::Unit) { None } else { tail };
        lo.b.terminate(Term::Ret(ret));
    }
    let (l, i) = (std::mem::take(&mut lo.pending), std::mem::take(&mut lo.instances));
    (lo.b.finish(params), l, i)
}

/// Lower one non-generic top-level function. Its parameters are the entry block's
/// parameters, and its types come from the checker's `FnSig` rather than the AST's
/// annotations, so an inferred return or an elaborated union is what reaches the IR.
///
/// A body that falls off its end gets an implicit `Ret` of the tail value — except at
/// `Unit`, where the terminator returns nothing, because a unit-returning function has no
/// return value in the C ABI.
fn lower_fn(
    env: &Env,
    result: &TypecheckResult,
    module: &[String],
    f: &ast::FnDecl,
) -> (Func, Vec<LambdaJob>, Vec<InstanceJob>) {
    let name = f.name.clone();
    let sig = env.fn_named(module, std::slice::from_ref(&name));
    let ret_ty = sig.map(|s| s.ret).unwrap_or(TyId(0));
    let ret_repr = repr_of(&env.solver.t, ret_ty);

    let mut lo = Lower::new(env, result, module.to_vec(), mangle(module, &f.name), ret_repr.clone());
    if let Some(s) = sig {
        set_throws(&mut lo.b, env, s.throws, &Default::default());
    }

    // Parameters are the entry block's parameters.
    let mut params = Vec::new();
    for (i, p) in f.params.iter().enumerate() {
        let ty = sig.and_then(|s| s.params.get(i)).map(|(_, t)| *t).unwrap_or(TyId(0));
        let r = repr_of(&env.solver.t, ty);
        let v = lo.b.block_param(BlockId(0), r, ty);
        lo.bind(&p.name, v);
        params.push(v);
    }
    lo.b.switch_to(BlockId(0));

    let body = f.body.as_ref().expect("filtered to bodied fns");
    let tail = lo.lower_block(body);
    if !lo.terminated {
        let ret = if matches!(ret_repr, Repr::Unit) { None } else { tail };
        lo.b.terminate(Term::Ret(ret));
    }
    let (l, i) = (std::mem::take(&mut lo.pending), std::mem::take(&mut lo.instances));
    (lo.b.finish(params), l, i)
}

/// The `test` blocks of a declaration list, paired with the module path they sit in.
/// Walked in the same order as `test_entries`, which is what lets the two be zipped.
fn collect_test_blocks<'a>(
    module: &[String],
    decls: &'a [Decl],
    out: &mut Vec<(Vec<String>, &'a ast::TestBlock)>,
) {
    for d in decls {
        match &d.kind {
            DeclKind::TestBlock(t) if t.kind == ast::TestKind::Test => {
                out.push((module.to_vec(), t))
            }
            DeclKind::Mod(m) => {
                let mut inner = module.to_vec();
                inner.push(m.name.clone());
                collect_test_blocks(&inner, &m.decls, out);
            }
            _ => {}
        }
    }
}

/// Lower one `test` block as a nullary, unit-returning, non-throwing function.
///
/// It has no signature in the environment — a test block is not a declaration anything can
/// name — so unlike `lower_fn` there is nothing to look up: the shape is fixed by
/// construction. The checker gives the body `never` for both its return and its throws
/// (`check.rs`), so a `return` or an uncaught throw inside one is already a type error and
/// this cannot be reached with either.
fn lower_test_block(
    env: &Env,
    result: &TypecheckResult,
    module: &[String],
    t: &ast::TestBlock,
    symbol: String,
) -> (Func, Vec<LambdaJob>, Vec<InstanceJob>) {
    let mut lo = Lower::new(env, result, module.to_vec(), symbol, Repr::Unit);
    lo.b.switch_to(BlockId(0));
    lo.lower_block(&t.body);
    if !lo.terminated {
        lo.b.terminate(Term::Ret(None));
    }
    let (l, i) = (std::mem::take(&mut lo.pending), std::mem::take(&mut lo.instances));
    (lo.b.finish(vec![]), l, i)
}

/// Lower a lambda's body as its own function. Its first parameter is the environment (a
/// tuple of the captured values); the rest are the lambda's parameters.
fn lower_lambda_job(env: &Env, result: &TypecheckResult, job: LambdaJob) -> (Func, Vec<LambdaJob>, Vec<InstanceJob>) {
    let ExprKind::Lambda { params: lparams, body } = &job.lambda.kind else {
        unreachable!("a lambda job holds a lambda");
    };
    // The lambda's inferred arrow gives its parameter, throws and return reprs.
    let (param_reprs, throws_repr, ret_repr) =
        match result.ty(job.lambda.id).map(|t| repr_of(&env.solver.t, t)) {
            Some(Repr::Closure { params, throws, ret }) => (params, *throws, *ret),
            _ => (vec![], Repr::Never, Repr::Unit),
        };
    // The inferred arrow is the *generic* one, so its variables are still open; the
    // enclosing instance's substitution closes them.
    let param_reprs: Vec<Repr> =
        param_reprs.iter().map(|r| substitute_repr(r, &job.subst)).collect();
    let ret_repr = substitute_repr(&ret_repr, &job.subst);
    let throws_repr = substitute_repr(&throws_repr, &job.subst);

    let mut lo = Lower::with_subst(
        env,
        result,
        job.module.clone(),
        job.name.clone(),
        ret_repr.clone(),
        job.subst.clone(),
    );
    // A throwing lambda returns the tagged result, like any throwing function.
    if !matches!(throws_repr, Repr::Never) {
        lo.b.set_throws(throws_repr);
    }

    // The environment parameter, then unpack each capture from it.
    let env_repr = Repr::Tuple(job.captures.iter().map(|(_, r, _)| r.clone()).collect());
    let env_v = lo.b.block_param(BlockId(0), env_repr.clone(), TyId(0));
    let mut params = vec![env_v];
    if !job.captures.is_empty() {
        for (i, (n, r, cty)) in job.captures.iter().enumerate() {
            let cap = lo.b.emit(Op::Elem { base: env_v, index: i }, r.clone(), *cty);
            lo.bind(n, cap);
        }
    }
    for (i, p) in lparams.iter().enumerate() {
        let r = param_reprs.get(i).cloned().unwrap_or(Repr::Any);
        let v = lo.b.block_param(BlockId(0), r, TyId(0));
        lo.bind(&p.name, v);
        params.push(v);
    }
    lo.b.switch_to(BlockId(0));

    let tail = lo.lower_expr(body);
    if !lo.terminated {
        let ret = if matches!(ret_repr, Repr::Unit) { None } else { Some(tail) };
        lo.b.terminate(Term::Ret(ret));
    }
    let (l, i) = (std::mem::take(&mut lo.pending), std::mem::take(&mut lo.instances));
    (lo.b.finish_lambda(params, env_repr), l, i)
}

/// The state of lowering one function body. There is one of these per emitted function —
/// including per generic *instance*, which is why `subst` lives here rather than being
/// threaded through every call: inside an instance, every type read out of the checker is
/// the generic one and has to pass through `repr_of_ty` before it describes real memory.
struct Lower<'a> {
    env: &'a Env,
    result: &'a TypecheckResult,
    /// The module the current function is in, for resolving call targets.
    module: Vec<String>,
    b: Builder,
    /// Local bindings, innermost last: a name resolves to its SSA value.
    scope: Vec<Vec<(String, Value)>>,
    /// Whether the current block already has a terminator (a `return` was lowered), so
    /// the statements after it are dead and must not be emitted.
    terminated: bool,
    /// The enclosing loops, innermost last, for `break` and `continue`.
    loops: Vec<LoopCtx>,
    /// The enclosing `try` handlers, innermost last: the block a throwing call or a
    /// `throw` jumps to on error, passing the error value. Empty means an error
    /// propagates straight out of the function.
    handlers: Vec<BlockId>,
    /// Lambdas discovered while lowering this function, to be lowered as their own.
    pending: Vec<LambdaJob>,
    /// This instance's type-variable bindings (empty for a non-generic function).
    subst: std::collections::HashMap<String, Repr>,
    /// Generic instances discovered at call sites, to be lowered in turn.
    instances: Vec<InstanceJob>,
}

impl<'a> Lower<'a> {
    fn new(
        env: &'a Env,
        result: &'a TypecheckResult,
        module: Vec<String>,
        fn_name: String,
        ret: Repr,
    ) -> Self {
        Self::with_subst(env, result, module, fn_name, ret, Default::default())
    }

    /// As `new`, for a generic instance: `subst` binds the instance's type parameters and
    /// is applied by `repr_of_ty` to every type this body reads out of the checker.
    fn with_subst(
        env: &'a Env,
        result: &'a TypecheckResult,
        module: Vec<String>,
        fn_name: String,
        ret: Repr,
        subst: std::collections::HashMap<String, Repr>,
    ) -> Self {
        Lower {
            env,
            result,
            module,
            b: Builder::new(fn_name, ret),
            scope: vec![vec![]],
            terminated: false,
            loops: vec![],
            handlers: vec![],
            pending: vec![],
            subst,
            instances: vec![],
        }
    }
}

/// What `break` and `continue` need: where each jumps, and the loop-carried variables to
/// pass at the back-edge. `continue_target` is the header for `loop`/`while` and the
/// latch (which increments the index) for `for`; both take the carried variables.
struct LoopCtx {
    continue_target: BlockId,
    exit: BlockId,
    carried: Vec<String>,
    /// Whether the loop yields a value (`break e`), so the exit block takes an argument.
    has_value: bool,
}

impl Lower<'_> {
    fn bind(&mut self, name: &str, v: Value) {
        self.scope.last_mut().unwrap().push((name.to_string(), v));
    }

    /// Resolve a name to its SSA value. Both iterations are reversed: frames innermost
    /// first for ordinary shadowing, and *within* a frame latest first, because a rebind
    /// (`StmtKind::Assign`) pushes a second entry under the same name rather than
    /// replacing the first. Scanning a frame forwards would resolve every read after an
    /// assignment back to the pre-assignment value.
    fn lookup(&self, name: &str) -> Option<Value> {
        self.scope.iter().rev().flat_map(|s| s.iter().rev()).find(|(n, _)| n == name).map(|(_, v)| *v)
    }

    /// The repr of an expression, as the checker typed it. An expression with no recorded
    /// type is one the checker never reached (a form under an error); `Unit` keeps
    /// lowering going rather than aborting the whole program over one bad subtree.
    fn repr(&self, e: &Expr) -> Repr {
        match self.result.ty(e.id) {
            Some(ty) => self.repr_of_ty(ty),
            None => Repr::Unit,
        }
    }

    /// The error repr for a call into a generic instance. The callee's `throws` is read
    /// off its *declared* signature, so where the clause is a type variable
    /// (`fn apply[T, E, R](f: (T) throws E -> R) throws E -> R`) it must go through the
    /// same substitution that monomorphised the callee — the one used to mangle its name.
    ///
    /// Skipping it is not a missed optimisation. The instance returns
    /// `Union([ret, IndexError])`; an unsubstituted `Repr::Var("E")` reaches `c_type`'s
    /// catch-all and erases to `neon_value`, so the *call site* builds
    /// `Union([ret, neon_value])` and the two disagree about the layout of one call's
    /// result. With `E = never` it is worse in kind: the instance returns its bare value
    /// while the call site still tags it.
    fn instance_throws_repr(
        &self,
        throws: TyId,
        subst: &std::collections::HashMap<String, Repr>,
    ) -> Repr {
        substitute_repr(&self.repr_of_ty(throws), subst)
    }

    /// The repr of a type, with this instance's type variables substituted away.
    fn repr_of_ty(&self, ty: TyId) -> Repr {
        let r = repr_of(&self.env.solver.t, ty);
        if self.subst.is_empty() {
            r
        } else {
            substitute_repr(&r, &self.subst)
        }
    }

    fn ty(&self, e: &Expr) -> TyId {
        self.result.ty(e.id).unwrap_or(TyId(0))
    }

    // ---- blocks and statements ----

    /// Lower a block, returning its value (the tail expression's), or `None` for a
    /// statement-sequence block.
    fn lower_block(&mut self, block: &Block) -> Option<Value> {
        // A rebind of a variable bound *outside* this block has to outlive it. The block's
        // own frame is popped on the way out, which would otherwise discard the new binding
        // and leave reads after the block seeing the old value. A bare block is
        // straight-line — no branch, so no merge — and the new value simply replaces the
        // old one in the enclosing scope.
        let mut names = Vec::new();
        collect_assigns_block(block, &mut names);
        let escaping = self.carried(names);

        self.scope.push(vec![]);
        for s in &block.stmts {
            if self.terminated {
                break;
            }
            self.lower_stmt(s);
        }
        let tail = match &block.tail {
            Some(e) if !self.terminated => Some(self.lower_expr(e)),
            _ => None,
        };
        let updated: Vec<Option<Value>> = escaping.iter().map(|n| self.lookup(n)).collect();
        self.scope.pop();
        for (n, v) in escaping.iter().zip(updated) {
            if let Some(v) = v {
                self.bind(n, v);
            }
        }
        tail
    }

    /// Lower one statement. Neither `let` nor an assignment allocates: a binding is just a
    /// name pointing at an SSA value, and a rebind pushes a new name-to-value pair that
    /// shadows the old one. Values never move, so nothing here has to be undone.
    fn lower_stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { pat, value, .. } => {
                let mut v = self.lower_expr(value);
                // Widen to the declared type when there is one. `let n: P | :none = :none`
                // otherwise bound a bare tag, and every later use that expected the union
                // read the wrong layout -- comparing it against a `P` emitted C that
                // compared a `uint64_t` with a struct.
                if let Some(declared) = self.result.declared(value.id) {
                    let want = self.repr_of_ty(declared);
                    if want != *self.b.value_repr(v) {
                        v = self.b.emit(Op::Cast(v), want, declared);
                    }
                }
                self.bind_pattern(pat, v);
            }
            StmtKind::Assign { name, value } => {
                // A rebind is a fresh SSA value shadowing the old binding.
                let v = self.lower_expr(value);
                self.bind(name, v);
            }
            StmtKind::Expr(e) => {
                self.lower_expr(e);
            }
            StmtKind::Error => {}
        }
    }

    /// Bind the names a pattern introduces to a value. Only the irrefutable shapes reach
    /// here (a `let`); refutable patterns live in `match`.
    fn bind_pattern(&mut self, p: &ast::Pattern, v: Value) {
        match &p.kind {
            ast::PatternKind::Bind(n) => self.bind(n, v),
            ast::PatternKind::Wildcard => {}
            ast::PatternKind::Tuple(ps) => {
                for (i, sub) in ps.iter().enumerate() {
                    let r = elem_repr(&self.b, v, i);
                    let ty = self.b.value_ty(v);
                    let e = self.b.emit(Op::Elem { base: v, index: i }, r, ty);
                    self.bind_pattern(sub, e);
                }
            }
            ast::PatternKind::Record { fields, .. } => {
                for f in fields {
                    let r = field_repr(&self.b, v, &f.name);
                    let ty = self.b.value_ty(v);
                    let e = self.b.emit(Op::Field { base: v, field: f.name.clone() }, r, ty);
                    match &f.pat {
                        Some(sub) => self.bind_pattern(sub, e),
                        None => self.bind(&f.name, e),
                    }
                }
            }
            _ => {}
        }
    }

    // ---- expressions ----

    /// Lower an expression to the value it produces. Every arm reads its repr and type from
    /// the checker up front rather than deriving them from the operands, so lowering never
    /// re-decides a type.
    ///
    /// The diverging forms — `break`, `continue`, `throw`, `return` — still have to return
    /// a `Value` because they are expressions. They mint one at `Repr::Never` *without*
    /// emitting an instruction and set `terminated`, so nothing downstream is lowered and
    /// the value is never read.
    fn lower_expr(&mut self, e: &Expr) -> Value {
        let repr = self.repr(e);
        let ty = self.ty(e);
        match &e.kind {
            ExprKind::Int(n) => self.b.emit(Op::ConstI64(*n as i64), repr, ty),
            ExprKind::Float(s) => {
                let bits = s.parse::<f64>().unwrap_or(0.0).to_bits();
                self.b.emit(Op::ConstF64(bits), repr, ty)
            }
            ExprKind::Bool(b) => self.b.emit(Op::ConstBool(*b), repr, ty),
            ExprKind::Null => self.b.emit(Op::ConstNull, repr, ty),
            ExprKind::Atom(a) => self.b.emit(Op::ConstAtom(a.clone()), repr, ty),
            ExprKind::Str(parts) => self.lower_str(parts, repr, ty),
            ExprKind::Path(p) => self.lower_path(p, repr, ty),
            ExprKind::Unary { op, rhs } => {
                let r = self.lower_expr(rhs);
                self.b.emit(Op::Prim(un_prim(*op), vec![r]), repr, ty)
            }
            ExprKind::Binary { op, lhs, rhs } => self.lower_binary(*op, lhs, rhs, repr, ty),
            ExprKind::Call { callee, generics, args } => {
                self.lower_call(e.id, callee, generics, args, repr, ty)
            }
            ExprKind::If { cond, then, else_ } => self.lower_if(cond, then, else_.as_deref(), repr, ty),
            ExprKind::Match { scrutinee, arms } => self.lower_match(scrutinee, arms, repr, ty),
            ExprKind::Loop { body } => self.lower_loop(body, repr, ty),
            ExprKind::While { cond, body } => self.lower_while(cond, body, ty),
            ExprKind::For { pat, iter, body } => self.lower_for(pat, iter, body, ty),
            ExprKind::Break(v) => {
                let bv = match v {
                    Some(e) => self.lower_expr(e),
                    None => self.b.emit(Op::ConstNull, Repr::Null, ty),
                };
                if let Some(ctx) = self.loops.last() {
                    let (exit, has_value, carried) = (ctx.exit, ctx.has_value, ctx.carried.clone());
                    // The exit block carries the loop variables out: the break value first
                    // (when the loop yields one), then each carried variable's current value.
                    let mut args = if has_value { vec![bv] } else { vec![] };
                    args.extend(carried.iter().map(|n| self.lookup(n).unwrap()));
                    self.b.terminate(Term::Jump(Target { to: exit, args }));
                }
                self.terminated = true;
                self.b.value(Repr::Never, ty)
            }
            ExprKind::Continue => {
                self.jump_to_header();
                self.terminated = true;
                self.b.value(Repr::Never, ty)
            }
            ExprKind::Block(b) => self.lower_block(b).unwrap_or_else(|| self.unit(ty)),
            ExprKind::Field { base, name } => {
                let b = self.lower_expr(base);
                self.b.emit(Op::Field { base: b, field: name.clone() }, repr, ty)
            }
            ExprKind::Tuple(elems) => {
                let vs = elems.iter().map(|e| self.lower_expr(e)).collect();
                self.b.emit(Op::MakeTuple(vs), repr, ty)
            }
            ExprKind::List(elems) => self.lower_list(elems, repr, ty),
            ExprKind::RecordLit { path, fields, spread } => {
                self.lower_record(path.as_deref(), fields, spread.as_deref(), repr, ty)
            }
            ExprKind::Index { base, index } => {
                let b = self.lower_expr(base);
                let i = self.lower_expr(index);
                self.b.emit(Op::Index { base: b, index: i }, repr, ty)
            }
            ExprKind::Is { lhs, ty: spec } => {
                let v = self.lower_expr(lhs);
                self.type_test(v, spec, e.id)
            }
            ExprKind::As { form, lhs, ty: spec } => {
                let v = self.lower_expr(lhs);
                match form {
                    // Infallible by the checker's trichotomy: a plain coercion.
                    ast::CastForm::Plain => self.b.emit(Op::Cast(v), repr, ty),
                    ast::CastForm::Assert => self.lower_cast_assert(e.id, v, spec, repr, ty),
                    ast::CastForm::Soften => self.lower_cast_soften(e.id, v, spec, repr, ty),
                }
            }
            ExprKind::Try { form, body, catch } => {
                self.lower_try(e.id, *form, body, catch.as_ref(), repr, ty)
            }
            ExprKind::Lambda { .. } => self.lower_lambda(e, repr, ty),
            ExprKind::Throw(e) => {
                let ev = self.lower_expr(e);
                match self.handlers.last().copied() {
                    Some(h) => self.b.terminate(Term::Jump(Target { to: h, args: vec![ev] })),
                    None => self.throw_or_escape(ev, ty),
                }
                self.terminated = true;
                self.b.value(Repr::Never, ty)
            }
            ExprKind::Return(v) => {
                let rv = v.as_ref().map(|e| self.lower_expr(e));
                self.b.terminate(Term::Ret(rv));
                self.terminated = true;
                // The value of a `return` is never consumed; mint one without emitting.
                self.b.value(Repr::Never, ty)
            }
            ExprKind::Assert { kind, args } => self.lower_assert(*kind, args, ty),
            _ => self.unhandled(e, repr, ty),
        }
    }

    /// `assert(..)`, `assert_eq(..)`, `assert_ne(..)`. A failure calls `neon_panic` with a
    /// message built here.
    ///
    /// This is the whole reason these are intrinsics rather than stdlib functions: the
    /// compiler still holds the *operands*, so the message can carry both the source text
    /// of what was asserted and the values it actually saw. A library `assert(cond)` gets a
    /// bare `bool` and can only ever say "assertion failed".
    ///
    /// `assert(a == b)` is therefore not lowered as one opaque condition. When the argument
    /// is a comparison, its two sides are lowered separately, compared here, and both
    /// values are reported — so `assert(1 + 1 == 3)` reads the same as `assert_eq(1 + 1, 3)`.
    fn lower_assert(&mut self, kind: ast::AssertKind, args: &[Expr], ty: TyId) -> Value {
        if matches!(kind, ast::AssertKind::Throws) {
            // `assert_throws` needs the argument to be a *throwing* expression, which the
            // checker rejects outside a `try`; there is no well-typed program that reaches
            // here. Marked rather than mis-lowered.
            return self.unhandled_note("assert_throws", Repr::Unit, ty);
        }
        let Some(Assertion { cond, operands, text }) = self.assert_condition(kind, args, ty) else {
            return self.unit(ty);
        };

        let ok = self.b.new_block();
        let fail = self.b.new_block();
        self.b.terminate(Term::Branch {
            cond,
            then: Target { to: ok, args: vec![] },
            els: Target { to: fail, args: vec![] },
        });

        self.b.switch_to(fail);
        self.terminated = false;
        let mut msg = self.b.emit(Op::ConstStr(format!("assertion failed: {text}")), Repr::Str, ty);
        if let Some((l, r)) = operands {
            msg = self.append_operand(msg, "\n  left:  ", l, ty);
            msg = self.append_operand(msg, "\n  right: ", r, ty);
        }
        self.b.emit_void(Op::Native { symbol: "neon_panic".into(), args: vec![msg] });
        self.b.terminate(Term::Unreachable);

        self.b.switch_to(ok);
        self.terminated = false;
        self.unit(ty)
    }

    /// Everything an assertion's failure path needs. `None` when the intrinsic was written
    /// with too few arguments — a shape the parser accepts and there is nothing to check.
    fn assert_condition(
        &mut self,
        kind: ast::AssertKind,
        args: &[Expr],
        ty: TyId,
    ) -> Option<Assertion> {
        match kind {
            ast::AssertKind::Assert => {
                let a = args.first()?;
                let text = describe(a);
                // A comparison is split so both sides can be reported.
                if let ExprKind::Binary { op, lhs, rhs } = &a.kind {
                    if let Some(prim) = comparison_prim(*op) {
                        let l = self.lower_expr(lhs);
                        let r = self.lower_expr(rhs);
                        let cond = self.b.emit(Op::Prim(prim, vec![l, r]), Repr::Bool, ty);
                        return Some(Assertion { cond, operands: Some((l, r)), text });
                    }
                }
                let cond = self.lower_expr(a);
                Some(Assertion { cond, operands: None, text })
            }
            ast::AssertKind::Eq | ast::AssertKind::Ne => {
                let (le, re) = (args.first()?, args.get(1)?);
                let l = self.lower_expr(le);
                let r = self.lower_expr(re);
                let (prim, op) = match kind {
                    ast::AssertKind::Eq => (PrimOp::Eq, "=="),
                    _ => (PrimOp::Ne, "!="),
                };
                let cond = self.b.emit(Op::Prim(prim, vec![l, r]), Repr::Bool, ty);
                let text = format!("{} {} {}", describe(le), op, describe(re));
                Some(Assertion { cond, operands: Some((l, r)), text })
            }
            ast::AssertKind::Throws => None,
        }
    }

    /// Append `label` and a rendering of `v` to the message being built. A value whose
    /// repr has no `to_string` is named rather than faked: claiming a value we cannot
    /// print would be worse than admitting we cannot.
    fn append_operand(&mut self, acc: Value, label: &str, v: Value, ty: TyId) -> Value {
        let lab = self.b.emit(Op::ConstStr(label.into()), Repr::Str, ty);
        let acc = self.concat_str(acc, lab, ty);
        let repr = self.b.value_repr(v).clone();
        let rendered = match &repr {
            // Quoted, so `""` and `" "` are distinguishable in the report.
            Repr::Str => {
                let q = self.b.emit(Op::ConstStr("\"".into()), Repr::Str, ty);
                let open = self.concat_str(q, v, ty);
                let q2 = self.b.emit(Op::ConstStr("\"".into()), Repr::Str, ty);
                self.concat_str(open, q2, ty)
            }
            _ => match to_string_symbol(&repr) {
                Some(sym) => self.b.emit(Op::Native { symbol: sym, args: vec![v] }, Repr::Str, ty),
                None => self.b.emit(Op::ConstStr("<not displayable>".into()), Repr::Str, ty),
            },
        };
        self.concat_str(acc, rendered, ty)
    }

    fn concat_str(&mut self, a: Value, b: Value, ty: TyId) -> Value {
        self.b.emit(Op::Native { symbol: "neon_str_concat".into(), args: vec![a, b] }, Repr::Str, ty)
    }

    /// `[a, b, c]` — one `MakeList` with the elements already lowered, left to right.
    fn lower_list(&mut self, elems: &[ast::Elem], repr: Repr, ty: TyId) -> Value {
        // A spread (`..rest`) is a concatenation; not lowered yet, so mark it.
        if elems.iter().any(|e| matches!(e, ast::Elem::Spread(_))) {
            return self.unhandled_note("list spread", repr, ty);
        }
        let vs = elems
            .iter()
            .map(|e| match e {
                ast::Elem::Value(x) => self.lower_expr(x),
                ast::Elem::Spread(_) => unreachable!("guarded above"),
            })
            .collect::<Vec<_>>();
        // The checker adopts an expected type wholesale for a list literal, so
        // `let xs: any = [1, 2, 3]` hands us `Any` — a repr with no list in it at all.
        // A list is still what gets *built*, and the builder needs the element repr: it
        // sizes the slots and picks the value-witness the runtime copies and releases by.
        // So build at the literal's real repr and erase afterwards, which is exactly the
        // shape a record literal already has (`MakeRecord` at `Flat`, then a `Cast`).
        //
        // Only bare `Any` takes this path. Every other repr that fails to show a list is
        // left to ice in the backend rather than guessed at here: guessing an element
        // width is the specific mistake the ice was added to stop.
        if repr == Repr::Any {
            let elem = normalize_union(vs.iter().map(|&v| self.b.value_repr(v).clone()).collect());
            let built = self.b.emit(Op::MakeList(vs), Repr::List(Box::new(elem)), ty);
            return self.b.emit(Op::Cast(built), repr, ty);
        }
        self.b.emit(Op::MakeList(vs), repr, ty)
    }

    /// A record literal. The literal's *syntactic* field order is discarded: fields are
    /// emitted in the order the repr lists them, so two literals of the same type build the
    /// same struct however they were written.
    fn lower_record(
        &mut self,
        path: Option<&[String]>,
        fields: &[ast::FieldInit],
        spread: Option<&Expr>,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        if spread.is_some() {
            return self.unhandled_note("record spread", repr, ty);
        }
        // Emit fields in the repr's canonical order, so every value of a type is built
        // the same way. A field the literal omits is a nullable optional -> null.
        let order: Vec<String> = match &repr {
            Repr::Record { fields, .. } => fields.iter().map(|(n, _)| n.clone()).collect(),
            _ => fields.iter().map(|f| f.name.clone()).collect(),
        };
        let name = path.and_then(|p| p.last().cloned());
        let mut built = Vec::new();
        for fname in order {
            let v = match fields.iter().find(|f| f.name == fname) {
                Some(f) => self.lower_expr(&f.value),
                None => self.b.emit(Op::ConstNull, Repr::Null, ty),
            };
            built.push((fname, v));
        }
        self.b.emit(Op::MakeRecord { name, fields: built }, repr, ty)
    }

    /// A string literal, or an interpolation. A single text part is a constant; anything
    /// else is a left-to-right fold of `neon_str_concat`, so an n-hole interpolation costs
    /// n-1 allocations.
    ///
    /// A hole prefers the `to_string` the checker resolved for it; `to_string_symbol` is
    /// the fallback for the primitives, and a value with neither (already a `str`) is
    /// concatenated as it is.
    fn lower_str(&mut self, parts: &[ast::StrPart], repr: Repr, ty: TyId) -> Value {
        if let [ast::StrPart::Text(s)] = parts {
            return self.b.emit(Op::ConstStr(s.clone()), repr.clone(), ty);
        }
        if parts.is_empty() {
            return self.b.emit(Op::ConstStr(String::new()), repr, ty);
        }
        // Interpolation: each hole is `to_string`'d, each text chunk is a literal, and
        // the pieces are concatenated left to right.
        let mut acc: Option<Value> = None;
        for part in parts {
            let piece = match part {
                ast::StrPart::Text(s) => self.b.emit(Op::ConstStr(s.clone()), Repr::Str, ty),
                ast::StrPart::Interp(e) => {
                    // Lower the hole's value — including its own dispatched call, if it
                    // is one — then apply the interpolation's recorded `to_string`
                    // dispatch, which lives in its own table precisely so the two never
                    // collide on the id.
                    let v = self.lower_expr(e);
                    match self.result.interp_call(e.id).cloned() {
                        Some(res) => self.lower_dispatch(&res, "to_string", vec![v], Repr::Str, ty),
                        None => {
                            let vr = self.b.value_repr(v).clone();
                            match to_string_symbol(&vr) {
                                Some(sym) => {
                                    self.b.emit(Op::Native { symbol: sym, args: vec![v] }, Repr::Str, ty)
                                }
                                None => v,
                            }
                        }
                    }
                }
            };
            acc = Some(match acc {
                None => piece,
                Some(a) => self.b.emit(
                    Op::Native { symbol: "neon_str_concat".into(), args: vec![a, piece] },
                    Repr::Str,
                    ty,
                ),
            });
        }
        acc.unwrap_or_else(|| self.b.emit(Op::ConstStr(String::new()), repr, ty))
    }

    /// A path in value position. A local wins over a function of the same name, since a
    /// binding shadows. Everything reachable here is a *value* use of a function name, not
    /// a call — `lower_call_vals` handles the call case before ever reaching this — so the
    /// function becomes a closure with an empty environment.
    fn lower_path(&mut self, p: &[String], repr: Repr, ty: TyId) -> Value {
        if let [name] = p {
            if let Some(v) = self.lookup(name) {
                // A refined use: the checker narrowed this binding (an `is` guard, a
                // null test, a match arm), so this use's type — and repr — is narrower
                // than the binding's. The value itself must be converted here, at the
                // one funnel every variable use passes through, because downstream
                // consumers do not all coerce: a primitive reads its operand raw, a
                // field access projects at the base's repr. `Op::Cast` is the
                // conversion — a union projects its member, an erased value unboxes
                // through the tag check — and a non-erasing cast is a view, so the
                // binding stays the owner and refcounting is undisturbed.
                if self.b.value_repr(v) != &repr {
                    return self.b.emit(Op::Cast(v), repr, ty);
                }
                return v;
            }
        }
        // A `const` is inlined: its initialiser is lowered again here, at this use, and
        // `ir::opt`'s folder collapses it to a single constant. That is the whole
        // implementation -- there is no storage, no symbol and no initialisation order,
        // because there is no runtime object. A `str` const becomes `Op::ConstStr`, which
        // codegen emits as `neon_str_lit`, so its bytes land in `.rodata` exactly as a
        // literal written in place would.
        //
        // Re-lowering rather than caching one `Value`: a `Value` belongs to the function
        // it was emitted into, so a cached one would be out of scope at a use in any other
        // function. The duplicated instructions fold away.
        //
        // The recursion terminates because `Checker::const_cycles` has already rejected a
        // const that reaches itself. Without that pass this call does not return.
        if let Some(c) = self.env.const_named(&self.module, p) {
            let value = c.value.clone();
            return self.lower_expr(&value);
        }
        // A bare function name used as a value: a closure with no captured environment.
        if let Some(sig) = self.env.fn_named(&self.module, p) {
            let func = mangle(&sig.module, &sig.name);
            return self.b.emit(Op::MakeClosure { func, captures: vec![] }, repr, ty);
        }
        self.unhandled_note("path-as-value", repr, ty)
    }

    /// `(x) => e` — capture the free variables, queue the body to be lowered as its own
    /// function, and build a closure of the two.
    fn lower_lambda(&mut self, e: &Expr, repr: Repr, ty: TyId) -> Value {
        let captures = self.free_vars(e);
        let capture_vals: Vec<Value> = captures.iter().map(|n| self.lookup(n).unwrap()).collect();
        let cap_info: Vec<(String, Repr, TyId)> = captures
            .iter()
            .zip(&capture_vals)
            .map(|(n, &v)| (n.clone(), self.b.value_repr(v).clone(), self.b.value_ty(v)))
            .collect();
        // One instance per (source lambda, enclosing substitution). Naming it by source id
        // alone emitted a single erased function shared by every instantiation: its
        // parameters became `neon_value`, so `(a: T, b: T) => a < b` compared two
        // *pointers*, and each caller cast the closure to its own concrete signature on
        // top of that. `sort` delegating to `sort_by` through such a lambda answered `:eq`
        // for every pair and left the list untouched.
        let name = mangle_instance(&format!("lambda${}", e.id.0), &self.subst);
        self.pending.push(LambdaJob {
            name: name.clone(),
            lambda: e.clone(),
            captures: cap_info,
            module: self.module.clone(),
            subst: self.subst.clone(),
        });
        self.b.emit(Op::MakeClosure { func: name, captures: capture_vals }, repr, ty)
    }

    /// The free variables a lambda closes over: names its body uses that are bound in the
    /// enclosing scope, excluding its own parameters and locals.
    fn free_vars(&self, e: &Expr) -> Vec<String> {
        let ExprKind::Lambda { params, body } = &e.kind else { return vec![] };
        let mut bound: std::collections::HashSet<String> =
            params.iter().map(|p| p.name.clone()).collect();
        let mut used = Vec::new();
        collect_free_expr(body, &mut bound, &mut used);
        let mut seen = std::collections::HashSet::new();
        used.retain(|n| self.lookup(n).is_some() && seen.insert(n.clone()));
        used
    }

    /// A binary operator. The arithmetic, comparison and bitwise ones are a single `Prim`
    /// with both operands evaluated; the four that are not (`and`, `or`, `orelse`, `|>`)
    /// need control flow or argument rearrangement and get their own lowering. Keeping the
    /// split in `bin_prim` means the two lists cannot disagree about which is which.
    fn lower_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, repr: Repr, ty: TyId) -> Value {
        if let Some(p) = bin_prim(op) {
            let l = self.lower_expr(lhs);
            let r = self.lower_expr(rhs);
            return self.b.emit(Op::Prim(p, vec![l, r]), repr, ty);
        }
        match op {
            BinOp::And | BinOp::Or => self.lower_and_or(op, lhs, rhs, ty),
            BinOp::Orelse => self.lower_orelse(lhs, rhs, repr, ty),
            BinOp::Pipe => self.lower_pipe(lhs, rhs, repr, ty),
            _ => self.unhandled_note("binary op", repr, ty),
        }
    }

    /// `and`/`or` short-circuit: the right operand is only evaluated when the left does
    /// not already decide the result.
    fn lower_and_or(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, ty: TyId) -> Value {
        let l = self.lower_expr(lhs);
        let rhs_b = self.b.new_block();
        let short_b = self.b.new_block();
        let join = self.b.new_block();
        let jp = self.b.block_param(join, Repr::Bool, ty);

        // `and`: `l` false shorts to false, else evaluate `rhs`. `or`: `l` true shorts
        // to true, else evaluate `rhs`.
        let (then_tgt, else_tgt, short_const) = match op {
            BinOp::And => (rhs_b, short_b, false),
            BinOp::Or => (short_b, rhs_b, true),
            _ => unreachable!(),
        };
        self.b.terminate(Term::Branch {
            cond: l,
            then: Target { to: then_tgt, args: vec![] },
            els: Target { to: else_tgt, args: vec![] },
        });

        self.b.switch_to(rhs_b);
        self.terminated = false;
        let r = self.lower_expr(rhs);
        if !self.terminated {
            self.b.terminate(Term::Jump(Target { to: join, args: vec![r] }));
        }

        self.b.switch_to(short_b);
        self.terminated = false;
        let sv = self.b.emit(Op::ConstBool(short_const), Repr::Bool, ty);
        self.b.terminate(Term::Jump(Target { to: join, args: vec![sv] }));

        self.b.switch_to(join);
        self.terminated = false;
        jp
    }

    /// `a orelse b` — `b` when `a` is null, else `a`'s non-null value.
    fn lower_orelse(&mut self, lhs: &Expr, rhs: &Expr, repr: Repr, ty: TyId) -> Value {
        let l = self.lower_expr(lhs);
        let lty = self.b.value_ty(l);
        let isnull = self.b.emit(Op::IsNull(l), Repr::Bool, lty);
        let none_b = self.b.new_block();
        let some_b = self.b.new_block();
        let join = self.b.new_block();
        let jp = self.b.block_param(join, repr.clone(), ty);

        self.b.terminate(Term::Branch {
            cond: isnull,
            then: Target { to: none_b, args: vec![] },
            els: Target { to: some_b, args: vec![] },
        });

        self.b.switch_to(none_b);
        self.terminated = false;
        let r = self.lower_expr(rhs);
        if !self.terminated {
            self.b.terminate(Term::Jump(Target { to: join, args: vec![r] }));
        }

        self.b.switch_to(some_b);
        self.terminated = false;
        // The non-null value, reinterpreted to the non-null repr.
        let unwrapped = self.b.emit(Op::Cast(l), repr, ty);
        self.b.terminate(Term::Jump(Target { to: join, args: vec![unwrapped] }));

        self.b.switch_to(join);
        self.terminated = false;
        jp
    }

    /// `a |> f(b)` is `f(a, b)` — the pipe threads its left side as the first argument.
    fn lower_pipe(&mut self, lhs: &Expr, rhs: &Expr, repr: Repr, ty: TyId) -> Value {
        let ExprKind::Call { callee, args, .. } = &rhs.kind else {
            return self.unhandled_note("pipe rhs", repr, ty);
        };
        let mut arg_vs = vec![self.lower_expr(lhs)];
        arg_vs.extend(args.iter().map(|a| self.lower_expr(a)));
        // A pipe target's own turbofish, if any.
        let generics: &[ast::TypeSpec] = match &rhs.kind {
            ExprKind::Call { generics, .. } => generics,
            _ => &[],
        };
        self.lower_call_vals(rhs.id, callee, generics, arg_vs, repr, ty)
    }

    /// `f(a, b)` — arguments are lowered left to right, before anything is decided about
    /// the callee, so evaluation order matches the source regardless of which of the four
    /// call shapes `lower_call_vals` picks.
    fn lower_call(
        &mut self,
        id: crate::ast::ExprId,
        callee: &Expr,
        generics: &[ast::TypeSpec],
        args: &[Expr],
        repr: Repr,
        ty: TyId,
    ) -> Value {
        let arg_vs: Vec<Value> = args.iter().map(|a| self.lower_expr(a)).collect();
        self.lower_call_vals(id, callee, generics, arg_vs, repr, ty)
    }

    /// Lower a call whose arguments are already lowered (shared by `f(..)` and pipe).
    ///
    /// The callee is decided by a ladder, in this order: a resolution the checker recorded
    /// for this expression id (protocol dispatch); a local of arrow type, which is a
    /// closure call; a named module function, direct or `@native`; and otherwise an
    /// arbitrary expression evaluated to a closure. The order matters — a local shadowing a
    /// function name must call the local, and a dispatched method must not be mistaken for
    /// a plain module function of the same name.
    ///
    /// A generic Neon function specialises here. Its type arguments come from the first
    /// source that has them: the checker's solved arguments, then a turbofish, then
    /// matching the declared parameter and return reprs against the concrete ones. The
    /// checker's solution is preferred over the turbofish because `repr_from_typespec` only
    /// recovers a nominal's *head* — enough to mangle a name, not enough to lay out the
    /// instance's parameters and locals.
    fn lower_call_vals(
        &mut self,
        id: crate::ast::ExprId,
        callee: &Expr,
        generics: &[ast::TypeSpec],
        arg_vs: Vec<Value>,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        // A dispatched call: the checker already chose the impl. An interpolation
        // hole's `to_string` resolution lives in its own table (`interp_call`), so
        // this consult never needs suppressing.
        if let Some(res) = self.result.call(id) {
            let res = res.clone();
            let method = match &callee.kind {
                ExprKind::Path(p) => p.last().cloned().unwrap_or_default(),
                _ => String::new(),
            };
            return self.lower_dispatch(&res, &method, arg_vs, repr, ty);
        }

        // A call through a local of arrow type is a closure call.
        if let ExprKind::Path(p) = &callee.kind {
            if let [one] = p.as_slice() {
                if let Some(callee_v) = self.lookup(one) {
                    return self.lower_closure_call(callee, callee_v, arg_vs, repr, ty);
                }
            }
            // A direct call to a named module function: native symbol or a Neon body.
            if let Some(sig) = self.env.fn_named(&self.module, p) {
                let throws = sig.throws;
                let native = sig.native.clone();
                let is_generic = !sig.generics.is_empty();
                let sgenerics = sig.generics.clone();
                let (smodule, sname) = (sig.module.clone(), sig.name.clone());
                let param_tys: Vec<TyId> = sig.params.iter().map(|(_, t)| *t).collect();
                let ret_ty = sig.ret;

                if is_generic && native.is_none() {
                    // Specialise. A turbofish pins the type arguments outright; otherwise
                    // infer them by matching the parameter (and return) reprs against the
                    // concrete argument reprs.
                    let mut subst = std::collections::HashMap::new();
                    // Prefer the checker's solved type arguments: a turbofish's *syntax*
                    // gives only a nominal's head (enough to mangle a name), while the
                    // instance's parameters and locals need the type's full layout.
                    if let Some(solved) = self.result.generics(id) {
                        for (name, gty) in solved {
                            subst.insert(name.clone(), self.repr_of_ty(*gty));
                        }
                    } else if !generics.is_empty() {
                        for (gname, spec) in sgenerics.iter().zip(generics) {
                            subst.insert(gname.clone(), repr_from_typespec(spec));
                        }
                    } else {
                        for (i, &av) in arg_vs.iter().enumerate() {
                            if let Some(&pty) = param_tys.get(i) {
                                let template = repr_of(&self.env.solver.t, pty);
                                let concrete = self.b.value_repr(av).clone();
                                match_repr(&template, &concrete, &mut subst);
                            }
                        }
                        let ret_template = repr_of(&self.env.solver.t, ret_ty);
                        match_repr(&ret_template, &repr, &mut subst);
                    }
                    let mangled = mangle_instance(&mangle(&smodule, &sname), &subst);
                    let err_repr = self.instance_throws_repr(throws, &subst);
                    self.instances.push(InstanceJob {
                        mangled: mangled.clone(),
                        module: smodule,
                        fn_name: sname,
                        subst,
                        impl_key: None,
                    });
                    let result = self.b.emit(Op::Call { func: mangled, args: arg_vs }, repr.clone(), ty);
                    return self.wrap_throwing_repr(result, err_repr, throws, repr, ty);
                }

                let op = match native {
                    Some(sym) => Op::Native { symbol: sym, args: arg_vs },
                    None => Op::Call { func: mangle(&smodule, &sname), args: arg_vs },
                };
                let result = self.b.emit(op, repr.clone(), ty);
                return self.wrap_throwing(result, throws, repr, ty);
            }
        }
        // The callee is an expression producing a closure -- `f()(x)`, `(lambda)(x)` --
        // so evaluate it and call through it.
        let callee_v = self.lower_expr(callee);
        self.lower_closure_call(callee, callee_v, arg_vs, repr, ty)
    }

    /// Call through a closure value. A throwing closure's function returns the tagged
    /// result — its repr carries the `throws` — so the call is unwrapped exactly like a
    /// direct call to a throwing function.
    fn lower_closure_call(
        &mut self,
        callee: &Expr,
        callee_v: Value,
        args: Vec<Value>,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        let err_repr = match self.b.value_repr(callee_v) {
            Repr::Closure { throws, .. } => throws.as_ref().clone(),
            _ => Repr::Never,
        };
        let result = self.b.emit(Op::CallClosure { callee: callee_v, args }, repr.clone(), ty);
        // The error's TyId, for the handler's parameter; the arrow the checker recorded
        // for the callee carries it. The repr above stays authoritative — it is
        // substituted where the arrow type would still mention a variable.
        let err_ty = self
            .result
            .ty(callee.id)
            .and_then(|t| self.env.solver.t.as_arrow(t))
            .map(|a| a.throws)
            .unwrap_or(ty);
        self.wrap_throwing_repr(result, err_repr, err_ty, repr, ty)
    }

    /// Lower a call the checker resolved by protocol dispatch. There is no vtable: every
    /// case ends in a direct call or a native symbol.
    ///
    /// A `Direct` to a native impl (the primitives) is a native call; to a user impl, a
    /// call to the method's own lowered function under its `mangle_impl` name, specialised
    /// per call site if the *method* is generic independently of the impl. A `Bound` is a
    /// `where` clause discharged here rather than at the check: inside a monomorphic
    /// instance the receiver is concrete, so its head names the impl the bound stood for.
    /// `Switch` — a union receiver needing a runtime discriminant test — has no lowering
    /// yet and is marked.
    fn lower_dispatch(
        &mut self,
        res: &crate::typecheck::dispatch::Resolution,
        method: &str,
        args: Vec<Value>,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        use crate::typecheck::dispatch::Resolution;
        let method = method.to_string();
        match res {
            Resolution::Direct(impl_id) => {
                let impl_def = &self.env.impls()[impl_id.0];
                let m = impl_def.methods.iter().find(|m| m.name == method);
                let Some(m) = m else {
                    return self.unhandled_note("dispatch: no method", repr, ty);
                };
                let throws = m.throws;
                let generics = m.generics.clone();
                let mparams: Vec<TyId> = m.params.iter().map(|(_, t)| *t).collect();
                let mret = m.ret;
                // Set only on the generic-instance path, where the declared `throws` may
                // still be a type variable. See `instance_throws_repr`.
                let mut err_repr: Option<Repr> = None;
                let op = match &m.native {
                    Some(sym) => Op::Native { symbol: sym.clone(), args },
                    None => {
                        // A user impl: call the method's own lowered function.
                        let proto = self.env.protocols()[impl_def.protocol.0].name.clone();
                        let head = impl_head(self.env, impl_def);
                        let key = mangle_impl(&proto, &head, &method);
                        if generics.is_empty() {
                            Op::Call { func: key, args }
                        } else {
                            // A generic method (`Mappable::map[T, U]`) monomorphises per
                            // call site, exactly like a generic module function: read the
                            // bindings off the actual arguments and queue that instance.
                            let mut subst = std::collections::HashMap::new();
                            for (i, &av) in args.iter().enumerate() {
                                if let Some(&pty) = mparams.get(i) {
                                    let template = repr_of(&self.env.solver.t, pty);
                                    let concrete = self.b.value_repr(av).clone();
                                    match_repr(&template, &concrete, &mut subst);
                                }
                            }
                            let ret_template = repr_of(&self.env.solver.t, mret);
                            match_repr(&ret_template, &repr, &mut subst);
                            let mangled = mangle_instance(&key, &subst);
                            err_repr = Some(self.instance_throws_repr(throws, &subst));
                            self.instances.push(InstanceJob {
                                mangled: mangled.clone(),
                                module: impl_def.module.clone(),
                                fn_name: method.clone(),
                                subst,
                                impl_key: Some(key),
                            });
                            Op::Call { func: mangled, args }
                        }
                    }
                };
                let result = self.b.emit(op, repr.clone(), ty);
                match err_repr {
                    Some(er) => self.wrap_throwing_repr(result, er, throws, repr, ty),
                    None => self.wrap_throwing(result, throws, repr, ty),
                }
            }
            Resolution::Switch(arms) => {
                // The checker computed the arms (coverage-checked, most-specific
                // filtered); lowering used to throw them away here and print
                // `<todo: dispatch switch>` as program output.
                let arms: Vec<(Repr, crate::typecheck::env::ImplId)> =
                    arms.iter().map(|&(t, id)| (self.repr_of_ty(t), id)).collect();
                self.lower_dispatch_switch(&arms, &method, args, repr, ty)
            }
            Resolution::Bound { protocol, .. } => {
                // In a monomorphic instance the receiver is concrete, so its head picks
                // the impl the bound stood for.
                let recv = args.first().copied();
                let head = recv.and_then(|v| repr_head(self.b.value_repr(v)));
                let proto = self.env.protocols()[protocol.0].name.clone();
                match head {
                    Some(h) => {
                        let found = find_impl_method(self.env, *protocol, &h, &method);
                        match found {
                            Some((Some(sym), throws)) => {
                                let result = self.b.emit(Op::Native { symbol: sym, args }, repr.clone(), ty);
                                self.wrap_throwing(result, throws, repr, ty)
                            }
                            Some((None, throws)) => {
                                let result = self.b.emit(
                                    Op::Call { func: mangle_impl(&proto, &h, &method), args },
                                    repr.clone(),
                                    ty,
                                );
                                self.wrap_throwing(result, throws, repr, ty)
                            }
                            None => self.unhandled_note("bound: no impl", repr, ty),
                        }
                    }
                    None => {
                        // A UNION receiver in a monomorphic instance: the head is
                        // per variant, so the dispatch is the same switch a
                        // `Resolution::Switch` lowers to — one impl per variant,
                        // chosen by the variant's own head.
                        if let Some(&recv) = args.first() {
                            if let Repr::Union(variants) = self.b.value_repr(recv).clone() {
                                let mut arms: Vec<(Repr, crate::typecheck::env::ImplId)> =
                                    Vec::new();
                                let mut ok = true;
                                for v in &variants {
                                    let found = repr_head(v)
                                        .and_then(|h| find_impl_id(self.env, *protocol, &h));
                                    match found {
                                        Some(id) => arms.push((v.clone(), id)),
                                        None => {
                                            ok = false;
                                            break;
                                        }
                                    }
                                }
                                if ok && !arms.is_empty() {
                                    return self.lower_dispatch_switch(
                                        &arms, &method, args, repr, ty,
                                    );
                                }
                            }
                        }
                        self.unhandled_note("bound: abstract receiver", repr, ty)
                    }
                }
            }
        }
    }

    /// A dispatched call whose receiver holds one of several types at run time: an
    /// if-else chain over the arms, each testing the receiver with the same runtime
    /// tag test `is` compiles to, projecting it to the arm's type, and calling that
    /// arm's impl directly through the ordinary `Resolution::Direct` path. The last
    /// arm goes untested — the checker proved coverage. Each arm's call is emitted at
    /// the arm's OWN return repr and the join widens it into the call site's (the
    /// block-parameter `assignable` relation), because a `to_string` returning `str`
    /// per arm must not be assigned into a union-typed C slot.
    fn lower_dispatch_switch(
        &mut self,
        arms: &[(Repr, crate::typecheck::env::ImplId)],
        method: &str,
        args: Vec<Value>,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        let Some(&recv) = args.first() else {
            return self.unhandled_note("dispatch switch: no receiver", repr, ty);
        };
        if arms.is_empty() {
            return self.unhandled_note("dispatch switch: no arms", repr, ty);
        }
        let join = self.b.new_block();
        let join_param = self.b.block_param(join, repr.clone(), ty);
        let bty = self.b.value_ty(recv);
        for (i, (arm_repr, impl_id)) in arms.iter().enumerate() {
            let last = i + 1 == arms.len();
            if !last {
                let test = self.b.emit(
                    Op::IsVariant {
                        value: recv,
                        variant: String::new(),
                        tested: Some(arm_repr.clone()),
                    },
                    Repr::Bool,
                    bty,
                );
                let body_b = self.b.new_block();
                let next_b = self.b.new_block();
                self.b.terminate(Term::Branch {
                    cond: test,
                    then: Target { to: body_b, args: vec![] },
                    els: Target { to: next_b, args: vec![] },
                });
                self.b.switch_to(body_b);
                self.emit_switch_arm(recv, arm_repr, *impl_id, method, &args, join, bty);
                self.b.switch_to(next_b);
            } else {
                self.emit_switch_arm(recv, arm_repr, *impl_id, method, &args, join, bty);
            }
        }
        self.b.switch_to(join);
        self.terminated = false;
        join_param
    }

    /// One arm of a dispatch switch: project, call direct, jump to the join.
    #[allow(clippy::too_many_arguments)]
    fn emit_switch_arm(
        &mut self,
        recv: Value,
        arm_repr: &Repr,
        impl_id: crate::typecheck::env::ImplId,
        method: &str,
        args: &[Value],
        join: crate::ir::ssa::BlockId,
        bty: TyId,
    ) {
        let proj = self.b.emit(Op::Cast(recv), arm_repr.clone(), bty);
        let mut arm_args = args.to_vec();
        arm_args[0] = proj;
        let m_ret = self.env.impls()[impl_id.0]
            .methods
            .iter()
            .find(|m| m.name == method)
            .map(|m| m.ret);
        let (arm_ret_ty, arm_ret_repr) = match m_ret {
            Some(r) => (r, self.repr_of_ty(r)),
            None => (bty, arm_repr.clone()),
        };
        let res = crate::typecheck::dispatch::Resolution::Direct(impl_id);
        let out = self.lower_dispatch(&res, method, arm_args, arm_ret_repr, arm_ret_ty);
        self.b.terminate(Term::Jump(Target { to: join, args: vec![out] }));
    }

    /// `if cond { .. } else { .. }`. The join block's parameters are the merge: the `if`'s
    /// own value first (only when it produces one), then one per reassigned variable, in a
    /// fixed order both branches follow.
    ///
    /// `produces` requires an `else`. An `if` without one cannot yield a value on the
    /// missing path, so the join takes no value parameter and the expression is unit —
    /// while the mutated variables still merge, the else edge passing their pre-`if` values.
    fn lower_if(
        &mut self,
        cond: &Expr,
        then: &Block,
        else_: Option<&Expr>,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        // Variables a branch reassigns must be merged at the join, or reads after the `if`
        // would see the pre-branch value. A branch's rebinds live in its own scope frame
        // and vanish when it pops, so each branch's values are captured before that.
        let mut names = Vec::new();
        collect_assigns_block(then, &mut names);
        if let Some(e) = else_ {
            collect_assigns_expr(e, &mut names);
        }
        let mutated = self.carried(names);

        let cond_v = self.lower_expr(cond);
        let then_b = self.b.new_block();
        let else_b = self.b.new_block();
        let join = self.b.new_block();
        let produces = !matches!(repr, Repr::Unit) && else_.is_some();
        let join_param = produces.then(|| self.b.block_param(join, repr.clone(), ty));
        // A join parameter per mutated variable, in a fixed order the branches follow.
        let mut mut_params = Vec::new();
        for n in &mutated {
            let v = self.lookup(n).unwrap();
            let (r, vty) = (self.b.value_repr(v).clone(), self.b.value_ty(v));
            mut_params.push(self.b.block_param(join, r, vty));
        }

        self.b.terminate(Term::Branch {
            cond: cond_v,
            then: Target { to: then_b, args: vec![] },
            els: Target { to: else_b, args: vec![] },
        });

        // then
        self.b.switch_to(then_b);
        self.terminated = false;
        self.scope.push(vec![]);
        let tv = self.lower_block_inline(then);
        if !self.terminated {
            let mut args = if produces { vec![tv.unwrap_or_else(|| self.unit(ty))] } else { vec![] };
            args.extend(mutated.iter().map(|n| self.lookup(n).unwrap()));
            self.b.terminate(Term::Jump(Target { to: join, args }));
        }
        self.scope.pop();

        // else (or straight to join when absent)
        self.b.switch_to(else_b);
        self.terminated = false;
        match else_ {
            Some(e) => {
                self.scope.push(vec![]);
                let ev = self.lower_expr(e);
                if !self.terminated {
                    let mut args = if produces { vec![ev] } else { vec![] };
                    args.extend(mutated.iter().map(|n| self.lookup(n).unwrap()));
                    self.b.terminate(Term::Jump(Target { to: join, args }));
                }
                self.scope.pop();
            }
            None => {
                // No else means the mutated variables keep their pre-`if` values here.
                let args: Vec<Value> = mutated.iter().map(|n| self.lookup(n).unwrap()).collect();
                self.b.terminate(Term::Jump(Target { to: join, args }));
            }
        }

        self.b.switch_to(join);
        self.terminated = false;
        // Reads after the `if` see the merged values.
        for (n, &p) in mutated.iter().zip(&mut_params) {
            self.bind(n, p);
        }
        join_param.unwrap_or_else(|| self.unit(ty))
    }

    /// Lower a block's statements and tail in the current scope frame (the caller manages
    /// push/pop), so mutations stay visible for the caller to thread out.
    fn lower_block_inline(&mut self, block: &Block) -> Option<Value> {
        for s in &block.stmts {
            if self.terminated {
                break;
            }
            self.lower_stmt(s);
        }
        match &block.tail {
            Some(e) if !self.terminated => Some(self.lower_expr(e)),
            _ => None,
        }
    }

    /// `try`/`try?`/`try!` and `try ... catch`. The body runs with an error handler
    /// installed; a throwing call or `throw` inside jumps to it. On success the body's
    /// value flows to the join; the handler propagates, softens to null, aborts, or runs
    /// the catch.
    fn lower_try(
        &mut self,
        id: crate::ast::ExprId,
        form: ast::TryForm,
        body: &Expr,
        catch: Option<&ast::CatchArm>,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        let join = self.b.new_block();
        let join_p = self.b.block_param(join, repr.clone(), ty);
        let handler = self.b.new_block();
        // The handler's error parameter takes the exact type the checker computed for what
        // this `try` can catch. Defaulting it to `any` would erase a type that is known.
        let err_ty = self.result.caught(id).unwrap_or(ty);
        // Through `repr_of_ty`, not `repr_of`: inside a generic instance whose clause is
        // `throws E` this type *is* the variable, and an unsubstituted `Var` erases to
        // `neon_value` at the C boundary — the handler would then read the error's fields
        // off the wrong layout.
        let err_repr = self.repr_of_ty(err_ty);
        let err_param = self.b.block_param(handler, err_repr, err_ty);

        self.handlers.push(handler);
        let body_v = self.lower_expr(body);
        if !self.terminated {
            self.b.terminate(Term::Jump(Target { to: join, args: vec![body_v] }));
        }
        self.handlers.pop();

        self.b.switch_to(handler);
        self.terminated = false;
        if let Some(c) = catch {
            self.scope.push(vec![]);
            self.bind(&c.binding, err_param);
            let cv = self.lower_block(&c.body).unwrap_or_else(|| self.unit(ty));
            if !self.terminated {
                self.b.terminate(Term::Jump(Target { to: join, args: vec![cv] }));
            }
            self.scope.pop();
        } else {
            match form {
                // "Propagate" means *to the next handler out*, which is an enclosing
                // `try ... catch` in this same function when there is one, and only the
                // function's `throws` clause when there is not. `self.handlers` has
                // already had this `try`'s own handler popped, so its top is exactly that
                // next handler.
                //
                // Consulting only `throws()` — the function's clause — asked a question
                // one level away from the one that decides where the error goes, and the
                // checker does not ask it that way: `note_throw` appends to the enclosing
                // `throw_sinks` frame, so it attributes an inner `try` to the outer
                // `catch`. The two disagreed, and `try { let v = try go(); v } catch (e)`
                // in a non-throwing `main` aborted the process instead of running its
                // catch. `ExprKind::Throw` four hundred lines up already does it this way.
                ast::TryForm::Propagate => match self.handlers.last().copied() {
                    Some(h) => {
                        self.b.terminate(Term::Jump(Target { to: h, args: vec![err_param] }))
                    }
                    None => self.throw_or_escape(err_param, ty),
                },
                ast::TryForm::Soften => {
                    let n = self.b.emit(Op::ConstNull, Repr::Null, ty);
                    self.b.terminate(Term::Jump(Target { to: join, args: vec![n] }));
                }
                ast::TryForm::Assert => {
                    let msg = self.error_message(err_param, ty);
                    self.b.emit_void(Op::Native {
                        symbol: "neon_panic".into(),
                        args: vec![msg],
                    });
                    self.b.terminate(Term::Unreachable);
                }
            }
        }

        self.b.switch_to(join);
        self.terminated = false;
        join_p
    }

    /// Wrap a call whose target may throw: check the tagged result and, on error, jump
    /// to the enclosing handler with the error; on success continue with the ok value.
    fn wrap_throwing(&mut self, result: Value, throws_ty: TyId, ok_repr: Repr, ty: TyId) -> Value {
        // `repr_of_ty` applies the *enclosing* instance's substitution. A call into a
        // generic callee needs the callee's as well — those sites go through
        // `instance_throws_repr` and `wrap_throwing_repr` directly.
        let err_repr = self.repr_of_ty(throws_ty);
        self.wrap_throwing_repr(result, err_repr, throws_ty, ok_repr, ty)
    }

    /// The same, with the error repr already in hand — a closure call reads it off the
    /// callee's repr, which is substituted where a TyId would still be generic.
    fn wrap_throwing_repr(
        &mut self,
        result: Value,
        err_repr: Repr,
        throws_ty: TyId,
        ok_repr: Repr,
        ty: TyId,
    ) -> Value {
        if matches!(err_repr, Repr::Never) {
            return result;
        }
        // The call yields a tagged result, not the callee's declared return. Retyping it
        // here keeps codegen and the refcount pass agreeing about what the value holds.
        self.b.set_value_repr(result, Repr::Union(vec![ok_repr.clone(), err_repr.clone()]));

        let iserr = self.b.emit(Op::IsErr(result), Repr::Bool, ty);
        let err = self.b.emit(Op::UnwrapErr(result), err_repr, throws_ty);
        let ok_b = self.b.new_block();
        match self.handlers.last().copied() {
            Some(h) => self.b.terminate(Term::Branch {
                cond: iserr,
                then: Target { to: h, args: vec![err] },
                els: Target { to: ok_b, args: vec![] },
            }),
            // The checker forbids a bare throwing call, so a handler is always present;
            // defensively, propagate straight out.
            None => self.b.terminate(Term::Branch {
                cond: iserr,
                then: Target { to: ok_b, args: vec![] },
                els: Target { to: ok_b, args: vec![] },
            }),
        }
        self.b.switch_to(ok_b);
        self.terminated = false;
        self.b.emit(Op::UnwrapOk(result), ok_repr, ty)
    }

    /// A `match` as a decision list: each arm tests the subject and, on a match, binds
    /// its pattern and runs its body, jumping to a join with the result. A dense
    /// integer or tag decision list is left for the optimiser to fold into a switch.
    fn lower_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[ast::MatchArm],
        repr: Repr,
        ty: TyId,
    ) -> Value {
        let subj = self.lower_expr(scrutinee);
        let produces = !matches!(repr, Repr::Unit);
        let join = self.b.new_block();
        let join_param = produces.then(|| self.b.block_param(join, repr.clone(), ty));

        for arm in arms {
            let matched = self.b.new_block();
            let next = self.b.new_block();

            // Test the pattern in the current block.
            match self.pattern_test(subj, &arm.pat) {
                None => self.b.terminate(Term::Jump(Target { to: matched, args: vec![] })),
                Some(test) => self.b.terminate(Term::Branch {
                    cond: test,
                    then: Target { to: matched, args: vec![] },
                    els: Target { to: next, args: vec![] },
                }),
            }

            // The matched block binds the pattern, checks any guard, and runs the body.
            self.b.switch_to(matched);
            self.terminated = false;
            self.scope.push(vec![]);
            self.bind_match_pattern(subj, &arm.pat);
            if let Some(g) = &arm.guard {
                let gv = self.lower_expr(g);
                let body_b = self.b.new_block();
                self.b.terminate(Term::Branch {
                    cond: gv,
                    then: Target { to: body_b, args: vec![] },
                    els: Target { to: next, args: vec![] },
                });
                self.b.switch_to(body_b);
                self.terminated = false;
            }
            let bv = self.lower_expr(&arm.body);
            if !self.terminated {
                let args = join_param.map(|_| vec![bv]).unwrap_or_default();
                self.b.terminate(Term::Jump(Target { to: join, args }));
            }
            self.scope.pop();

            self.b.switch_to(next);
            self.terminated = false;
        }

        // The checker proved the arms exhaustive, so falling off the last is unreachable.
        self.b.terminate(Term::Unreachable);
        self.b.switch_to(join);
        self.terminated = false;
        join_param.unwrap_or_else(|| self.unit(ty))
    }

    /// The test an arm's pattern imposes, or `None` when it always matches. Sub-patterns
    /// (a nested literal in a field) contribute their own tests, ANDed together.
    fn pattern_test(&mut self, subj: Value, pat: &ast::Pattern) -> Option<Value> {
        match &pat.kind {
            ast::PatternKind::Wildcard | ast::PatternKind::Bind(_) => None,
            ast::PatternKind::Is(spec) => Some(self.type_test(subj, spec, pat.id)),
            ast::PatternKind::Literal(lit) => Some(self.literal_test(subj, lit)),
            ast::PatternKind::Record { path, fields, .. } => {
                let tested = self.tested_repr(pat.id);
                let mut test = path.as_ref().and_then(|p| p.last()).map(|n| {
                    self.b.emit(
                        Op::IsVariant {
                            value: subj,
                            variant: n.clone(),
                            tested: tested.clone(),
                        },
                        Repr::Bool,
                        subj_ty(&self.b, subj),
                    )
                });
                for f in fields {
                    if let Some(sub) = &f.pat {
                        let r = field_repr(&self.b, subj, &f.name);
                        let fv = self.b.emit(
                            Op::Field { base: subj, field: f.name.clone() },
                            r,
                            subj_ty(&self.b, subj),
                        );
                        if let Some(sub_test) = self.pattern_test(fv, sub) {
                            test = Some(self.and(test, sub_test));
                        }
                    }
                }
                test
            }
            ast::PatternKind::Tuple(ps) => {
                let mut test = None;
                for (i, sub) in ps.iter().enumerate() {
                    let r = elem_repr(&self.b, subj, i);
                    let ev =
                        self.b.emit(Op::Elem { base: subj, index: i }, r, subj_ty(&self.b, subj));
                    if let Some(sub_test) = self.pattern_test(ev, sub) {
                        test = Some(self.and(test, sub_test));
                    }
                }
                test
            }
            _ => None,
        }
    }

    /// The repr of the type an `is` names, as the checker resolved it.
    ///
    /// `None` where the checker recorded nothing — a test under an already-reported error,
    /// or a pattern form that resolves no path. Not a licence to guess: the backend needs
    /// this for an erased subject and refuses without it. A `Var` is filtered out for the
    /// same reason `type_tag_name` ices on one; inside a generic body `self.subst` has
    /// already replaced every variable the instance pinned, so what survives is genuinely
    /// unpinned and must not be turned into a tag.
    fn tested_repr(&self, id: ExprId) -> Option<Repr> {
        let r = self.result.tested(id).map(|t| self.repr_of_ty(t))?;
        (!contains_var(&r)).then_some(r)
    }

    /// `x is T` as a runtime test: null becomes a null check, anything else a
    /// discriminant compare — by head name against a union's arms, and by the checker's
    /// resolved type against an erased value's box tag.
    /// The short spelling of a type spec for a trap message: the written head for a
    /// named type, a token for the structural shapes. The message names what the
    /// programmer asserted, not the checker's full resolution.
    fn spec_text(spec: &ast::TypeSpec) -> String {
        match &spec.kind {
            ast::TypeSpecKind::Named { path, .. } => path.join("::"),
            ast::TypeSpecKind::Atom(a) => format!(":{a}"),
            ast::TypeSpecKind::Null => "null".into(),
            _ => "the target type".into(),
        }
    }

    /// `x as! T`: assert the value holds `T`; a mismatch traps.
    ///
    /// An erased source needs no explicit test — `Op::Cast` out of `any` is the
    /// tag-checked unbox (`neon_box_expect`), which traps with its own message. Every
    /// other fallible source (a union to one of its members, a nullable to its payload)
    /// branches on the same runtime test `is` compiles to, and the mismatch arm panics:
    /// same exit status as a trap, and the message names the asserted type.
    fn lower_cast_assert(
        &mut self,
        id: ExprId,
        v: Value,
        spec: &ast::TypeSpec,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        if matches!(self.b.value_repr(v), Repr::Any) {
            return self.b.emit(Op::Cast(v), repr, ty);
        }
        let test = self.type_test(v, spec, id);
        let ok_b = self.b.new_block();
        let bad_b = self.b.new_block();
        self.b.terminate(Term::Branch {
            cond: test,
            then: Target { to: ok_b, args: vec![] },
            els: Target { to: bad_b, args: vec![] },
        });
        self.b.switch_to(bad_b);
        let msg = self.b.emit(
            Op::ConstStr(format!(
                "`as! {}` failed: the value holds a different type",
                Self::spec_text(spec)
            )),
            Repr::Str,
            ty,
        );
        self.b.emit_void(Op::Native { symbol: "neon_panic".into(), args: vec![msg] });
        self.b.terminate(Term::Unreachable);
        self.b.switch_to(ok_b);
        self.terminated = false;
        self.b.emit(Op::Cast(v), repr, ty)
    }

    /// `x as? T`: the same test, softened — the match arm casts and widens into
    /// `T | null` at the join, the mismatch arm contributes `null`.
    fn lower_cast_soften(
        &mut self,
        id: ExprId,
        v: Value,
        spec: &ast::TypeSpec,
        repr: Repr,
        ty: TyId,
    ) -> Value {
        let test = self.type_test(v, spec, id);
        let ok_b = self.b.new_block();
        let null_b = self.b.new_block();
        let join = self.b.new_block();
        let join_param = self.b.block_param(join, repr.clone(), ty);
        self.b.terminate(Term::Branch {
            cond: test,
            then: Target { to: ok_b, args: vec![] },
            els: Target { to: null_b, args: vec![] },
        });
        // The narrow value first, at the tested type's own repr; the jump into the
        // join widens it into `T | null`, exactly as a `try?` widens its ok value.
        self.b.switch_to(ok_b);
        let target = self.result.tested(id).map(|t| self.repr_of_ty(t)).unwrap_or(repr.clone());
        let tested_ty = self.result.tested(id).unwrap_or(ty);
        let cast = self.b.emit(Op::Cast(v), target, tested_ty);
        self.b.terminate(Term::Jump(Target { to: join, args: vec![cast] }));
        self.b.switch_to(null_b);
        let n = self.b.emit(Op::ConstNull, Repr::Null, ty);
        self.b.terminate(Term::Jump(Target { to: join, args: vec![n] }));
        self.b.switch_to(join);
        self.terminated = false;
        join_param
    }

    fn type_test(&mut self, subj: Value, spec: &ast::TypeSpec, id: ExprId) -> Value {
        let bty = subj_ty(&self.b, subj);
        match &spec.kind {
            ast::TypeSpecKind::Null => self.b.emit(Op::IsNull(subj), Repr::Bool, bty),
            ast::TypeSpecKind::Named { path, .. } => {
                let variant = path.last().cloned().unwrap_or_default();
                let tested = self.tested_repr(id);
                self.b.emit(Op::IsVariant { value: subj, variant, tested }, Repr::Bool, bty)
            }
            ast::TypeSpecKind::Atom(a) => {
                let lit = self.b.emit(Op::ConstAtom(a.clone()), Repr::Tag, bty);
                self.b.emit(Op::Prim(PrimOp::Eq, vec![subj, lit]), Repr::Bool, bty)
            }
            // A structural type spec — a tuple `(i64, str)`, an arrow `(i64) -> str`.
            // These have no head name to write, but they are not unanswerable: the
            // checker resolved the spec to a type, and `Op::IsVariant` compares *that*
            // against the value, by box tag when the subject is erased and statically
            // otherwise. `ConstBool(true)` was here instead, so `x is (i64, str)` was
            // true for a `5` and `x is (i64) -> str` was true for everything — the
            // answer a person writes an `is` precisely to avoid, and `as` trusts `is`.
            //
            // With no resolved type there is deliberately no answer: `IsVariant` carries
            // `tested: None`, which the backend refuses on an erased value rather than
            // guessing, and reads as "not that variant" on a concrete one.
            _ => {
                let tested = self.tested_repr(id);
                self.b.emit(
                    Op::IsVariant { value: subj, variant: String::new(), tested },
                    Repr::Bool,
                    bty,
                )
            }
        }
    }

    /// A literal pattern tests equality; a `null` literal is a null check.
    fn literal_test(&mut self, subj: Value, lit: &Expr) -> Value {
        let bty = subj_ty(&self.b, subj);
        if matches!(lit.kind, ExprKind::Null) {
            return self.b.emit(Op::IsNull(subj), Repr::Bool, bty);
        }
        // A pattern literal is not a checked expression, so it has no recorded type; give
        // the constant the subject's (narrowed) repr rather than defaulting to unit.
        let scalar = variant_scalar(self.b.value_repr(subj));
        let lv = match &lit.kind {
            ExprKind::Int(n) => self.b.emit(Op::ConstI64(*n as i64), scalar, bty),
            ExprKind::Bool(b) => self.b.emit(Op::ConstBool(*b), scalar, bty),
            ExprKind::Atom(a) => self.b.emit(Op::ConstAtom(a.clone()), Repr::Tag, bty),
            _ => self.lower_expr(lit),
        };
        self.b.emit(Op::Prim(PrimOp::Eq, vec![subj, lv]), Repr::Bool, bty)
    }

    /// Conjoin two pattern tests, with `None` meaning "no test so far". This is a plain
    /// `Prim(And)`, not the short-circuiting `lower_and_or`: both operands are pure
    /// projections and comparisons that have already been emitted, so there is nothing to
    /// skip and no need for the extra blocks.
    fn and(&mut self, a: Option<Value>, b: Value) -> Value {
        match a {
            Some(a) => {
                let bty = subj_ty(&self.b, b);
                self.b.emit(Op::Prim(PrimOp::And, vec![a, b]), Repr::Bool, bty)
            }
            None => b,
        }
    }

    /// Bind the names an arm's pattern introduces: a bare binding narrows the subject,
    /// a record/tuple pattern projects and recurses.
    fn bind_match_pattern(&mut self, subj: Value, pat: &ast::Pattern) {
        match &pat.kind {
            ast::PatternKind::Bind(n) => self.bind(n, subj),
            ast::PatternKind::Record { fields, .. } => {
                for f in fields {
                    let r = field_repr(&self.b, subj, &f.name);
                    let fv = self.b.emit(
                        Op::Field { base: subj, field: f.name.clone() },
                        r,
                        subj_ty(&self.b, subj),
                    );
                    match &f.pat {
                        Some(sub) => self.bind_match_pattern(fv, sub),
                        None => self.bind(&f.name, fv),
                    }
                }
            }
            ast::PatternKind::Tuple(ps) => {
                for (i, sub) in ps.iter().enumerate() {
                    let r = elem_repr(&self.b, subj, i);
                    let ev =
                        self.b.emit(Op::Elem { base: subj, index: i }, r, subj_ty(&self.b, subj));
                    self.bind_match_pattern(ev, sub);
                }
            }
            _ => {}
        }
    }

    /// `loop { body }` — an infinite loop the body leaves with `break`. Its value is
    /// the union of the break values (the exit block's parameter). Loop-carried
    /// variables (those the body reassigns) become the header block's parameters, which
    /// is how mutable-looking locals stay SSA.
    fn lower_loop(&mut self, body: &Block, repr: Repr, ty: TyId) -> Value {
        let carried = self.carried_vars(body);
        let inits: Vec<Value> = carried.iter().map(|n| self.lookup(n).unwrap()).collect();

        let header = self.b.new_block();
        let exit = self.b.new_block();
        let produces = !matches!(repr, Repr::Unit);
        let exit_param = produces.then(|| self.b.block_param(exit, repr.clone(), ty));

        // Header parameters mirror the carried variables' current reprs; the exit block
        // takes the same set, so code after the loop reads their final values.
        let mut header_params = Vec::new();
        let mut exit_carried = Vec::new();
        for &v in &inits {
            let (r, vty) = (self.b.value_repr(v).clone(), self.b.value_ty(v));
            header_params.push(self.b.block_param(header, r.clone(), vty));
            exit_carried.push(self.b.block_param(exit, r, vty));
        }

        self.b.terminate(Term::Jump(Target { to: header, args: inits }));
        self.b.switch_to(header);
        self.terminated = false;
        self.scope.push(vec![]);
        for (n, &p) in carried.iter().zip(&header_params) {
            self.bind(n, p);
        }

        self.loops.push(LoopCtx { continue_target: header, exit, carried: carried.clone(), has_value: produces });
        for s in &body.stmts {
            if self.terminated {
                break;
            }
            self.lower_stmt(s);
        }
        if !self.terminated {
            if let Some(t) = &body.tail {
                self.lower_expr(t);
            }
        }
        // The back-edge: loop around with the carried variables' latest values.
        if !self.terminated {
            self.jump_to_header();
        }
        self.loops.pop();
        self.scope.pop();
        // Reads after the loop see the carried variables' exit values.
        for (n, &p) in carried.iter().zip(&exit_carried) {
            self.bind(n, p);
        }

        self.b.switch_to(exit);
        self.terminated = false;
        exit_param.unwrap_or_else(|| self.unit(ty))
    }

    /// `while cond { body }` — a loop whose header tests the condition. Yields unit.
    fn lower_while(&mut self, cond: &Expr, body: &Block, ty: TyId) -> Value {
        let carried = self.carried_vars(body);
        let inits: Vec<Value> = carried.iter().map(|n| self.lookup(n).unwrap()).collect();

        let header = self.b.new_block();
        let body_b = self.b.new_block();
        let exit = self.b.new_block();

        let mut header_params = Vec::new();
        let mut exit_carried = Vec::new();
        for &v in &inits {
            let (r, vty) = (self.b.value_repr(v).clone(), self.b.value_ty(v));
            header_params.push(self.b.block_param(header, r.clone(), vty));
            exit_carried.push(self.b.block_param(exit, r, vty));
        }

        self.b.terminate(Term::Jump(Target { to: header, args: inits }));
        self.b.switch_to(header);
        self.terminated = false;
        self.scope.push(vec![]);
        for (n, &p) in carried.iter().zip(&header_params) {
            self.bind(n, p);
        }
        let cond_v = self.lower_expr(cond);
        // The condition-false edge is the natural exit: it hands the carried variables'
        // current values (the header params) out to the exit block.
        self.b.terminate(Term::Branch {
            cond: cond_v,
            then: Target { to: body_b, args: vec![] },
            els: Target { to: exit, args: header_params.clone() },
        });

        self.b.switch_to(body_b);
        self.terminated = false;
        self.loops.push(LoopCtx { continue_target: header, exit, carried: carried.clone(), has_value: false });
        for s in &body.stmts {
            if self.terminated {
                break;
            }
            self.lower_stmt(s);
        }
        // The body's tail expression runs for its effects (a while yields unit, so its
        // value is discarded) — dropping it would lose a trailing `if … { break }`.
        if !self.terminated {
            if let Some(t) = &body.tail {
                self.lower_expr(t);
            }
        }
        if !self.terminated {
            self.jump_to_header();
        }
        self.loops.pop();
        self.scope.pop();
        for (n, &p) in carried.iter().zip(&exit_carried) {
            self.bind(n, p);
        }

        self.b.switch_to(exit);
        self.terminated = false;
        self.unit(ty)
    }

    /// `for x in xs { body }` — a C loop over a contiguous list, indexed from 0 to its
    /// length. The index and any reassigned locals are block parameters; the latch block
    /// increments the index and is where `continue` lands.
    fn lower_for(&mut self, pat: &ast::Pattern, iter: &Expr, body: &Block, ty: TyId) -> Value {
        let list = self.lower_expr(iter);
        let elem_repr = match self.b.value_repr(list) {
            Repr::List(e) => (**e).clone(),
            _ => Repr::Any,
        };
        let len = self.b.emit(Op::Native { symbol: "neon_list_len".into(), args: vec![list] }, Repr::I64, ty);
        let zero = self.b.emit(Op::ConstI64(0), Repr::I64, ty);

        let carried = self.carried_vars(body);
        let inits: Vec<Value> = carried.iter().map(|n| self.lookup(n).unwrap()).collect();

        let header = self.b.new_block();
        let body_b = self.b.new_block();
        let latch = self.b.new_block();
        let exit = self.b.new_block();

        let i_param = self.b.block_param(header, Repr::I64, ty);
        let mut carried_params = Vec::new();
        let mut exit_carried = Vec::new();
        for &v in &inits {
            let (r, vty) = (self.b.value_repr(v).clone(), self.b.value_ty(v));
            carried_params.push(self.b.block_param(header, r.clone(), vty));
            exit_carried.push(self.b.block_param(exit, r, vty));
        }
        // The latch takes the carried variables from each back-edge (body end, continue).
        let mut latch_params = Vec::new();
        for &v in &inits {
            let (r, vty) = (self.b.value_repr(v).clone(), self.b.value_ty(v));
            latch_params.push(self.b.block_param(latch, r, vty));
        }

        let mut entry_args = vec![zero];
        entry_args.extend(inits);
        self.b.terminate(Term::Jump(Target { to: header, args: entry_args }));

        // header: test the index, bind carried, branch into the body or out.
        self.b.switch_to(header);
        self.terminated = false;
        self.scope.push(vec![]);
        for (n, &p) in carried.iter().zip(&carried_params) {
            self.bind(n, p);
        }
        let cond = self.b.emit(Op::Prim(PrimOp::Lt, vec![i_param, len]), Repr::Bool, ty);
        // Exhausting the list exits, handing the carried variables out to the exit block.
        self.b.terminate(Term::Branch {
            cond,
            then: Target { to: body_b, args: vec![] },
            els: Target { to: exit, args: carried_params.clone() },
        });

        // body: bind the element and run.
        self.b.switch_to(body_b);
        self.terminated = false;
        self.scope.push(vec![]);
        let elem = self.b.emit(Op::Index { base: list, index: i_param }, elem_repr, ty);
        self.bind_pattern(pat, elem);
        self.loops.push(LoopCtx {
            continue_target: latch,
            exit,
            carried: carried.clone(),
            has_value: false,
        });
        for s in &body.stmts {
            if self.terminated {
                break;
            }
            self.lower_stmt(s);
        }
        if !self.terminated {
            if let Some(t) = &body.tail {
                self.lower_expr(t);
            }
        }
        if !self.terminated {
            let args: Vec<Value> = carried.iter().map(|n| self.lookup(n).unwrap()).collect();
            self.b.terminate(Term::Jump(Target { to: latch, args }));
        }
        self.loops.pop();
        self.scope.pop();

        // latch: increment the index, loop with the carried variables it received.
        self.b.switch_to(latch);
        self.terminated = false;
        let one = self.b.emit(Op::ConstI64(1), Repr::I64, ty);
        let next_i = self.b.emit(Op::Prim(PrimOp::Add, vec![i_param, one]), Repr::I64, ty);
        let mut back = vec![next_i];
        back.extend(latch_params);
        self.b.terminate(Term::Jump(Target { to: header, args: back }));

        self.scope.pop();
        for (n, &p) in carried.iter().zip(&exit_carried) {
            self.bind(n, p);
        }
        self.b.switch_to(exit);
        self.terminated = false;
        self.unit(ty)
    }

    /// Leave the function with an error. A function that declares `throws` returns the
    /// error case of its tagged result. Otherwise the error is escaping the program — only
    /// `main` reaches here — so it is reported: `Error::message` resolves statically for
    /// the concrete type, and the text goes to the top-level panic.
    fn throw_or_escape(&mut self, err: Value, ty: TyId) {
        if self.b.throws().is_some() {
            self.b.terminate(Term::Throw(err));
            return;
        }
        let msg = self.error_message(err, ty);
        self.b.emit_void(Op::Native { symbol: "neon_panic".into(), args: vec![msg] });
        self.b.terminate(Term::Unreachable);
    }

    /// `Error::message(e)` for an error value: resolved at compile time where the value
    /// has one nominal type, and by a runtime test over the variants where it does not.
    ///
    /// A `try` body that can throw two error types produces a *union*, which has no head,
    /// and the union case used to fall straight through to the `"error"` constant below.
    /// The program then panicked with the literal text `error` — never the message the
    /// impl was written to produce — with nothing to distinguish it from a real one.
    fn error_message(&mut self, err: Value, ty: TyId) -> Value {
        let r = self.b.value_repr(err).clone();
        if let Some(head) = repr_head(&r) {
            let name = mangle_impl("Error", &head, "message");
            return self.b.emit(Op::Call { func: name, args: vec![err] }, Repr::Str, ty);
        }
        // A union of error types: which impl to call is a runtime question, so ask it.
        // Each variant is tested in turn and the matching arm narrows and calls its own
        // `Error::message`. Only taken when every variant names an impl — a union with an
        // unnameable member has no such chain and falls through.
        if let Repr::Union(variants) = &r {
            let heads: Option<Vec<String>> = variants.iter().map(repr_head).collect();
            if let Some(heads) = heads.filter(|h| !h.is_empty()) {
                let variants = variants.clone();
                let join = self.b.new_block();
                let jp = self.b.block_param(join, Repr::Str, ty);
                for (v, head) in variants.iter().zip(&heads) {
                    let hit = self.b.new_block();
                    let next = self.b.new_block();
                    let test = self.b.emit(
                        Op::IsVariant {
                            value: err,
                            variant: head.clone(),
                            tested: Some(v.clone()),
                        },
                        Repr::Bool,
                        ty,
                    );
                    self.b.terminate(Term::Branch {
                        cond: test,
                        then: Target { to: hit, args: vec![] },
                        els: Target { to: next, args: vec![] },
                    });
                    self.b.switch_to(hit);
                    self.terminated = false;
                    // The impl method takes the concrete type, so narrow before calling.
                    let narrowed = self.b.emit(Op::Cast(err), v.clone(), ty);
                    let msg = self.b.emit(
                        Op::Call {
                            func: mangle_impl("Error", head, "message"),
                            args: vec![narrowed],
                        },
                        Repr::Str,
                        ty,
                    );
                    self.b.terminate(Term::Jump(Target { to: join, args: vec![msg] }));
                    self.b.switch_to(next);
                    self.terminated = false;
                }
                // The value is one of the union's variants, so every test failing is not
                // a case to invent a message for.
                self.b.terminate(Term::Unreachable);
                self.b.switch_to(join);
                self.terminated = false;
                return jp;
            }
        }
        // No nominal head to dispatch on; report something rather than nothing.
        self.b.emit(Op::ConstStr("error".into()), Repr::Str, ty)
    }

    /// Jump to the innermost loop's header with the current values of its carried vars.
    fn jump_to_header(&mut self) {
        if let Some(ctx) = self.loops.last() {
            let (target, carried) = (ctx.continue_target, ctx.carried.clone());
            let args: Vec<Value> = carried.iter().map(|n| self.lookup(n).unwrap()).collect();
            self.b.terminate(Term::Jump(Target { to: target, args }));
        }
    }

    /// The variables a loop body reassigns that are bound outside it — the loop-carried
    /// state, which becomes the header block's parameters.
    ///
    /// The scan descends into nested loops: an inner loop reassigning an outer local makes
    /// that local carried by *both*, and the inner loop's exit block hands the value back
    /// for the outer back-edge to pass on. It does not descend into a lambda body, because
    /// a closure cannot reassign a capture.
    fn carried_vars(&self, body: &Block) -> Vec<String> {
        let mut names = Vec::new();
        collect_assigns_block(body, &mut names);
        self.carried(names)
    }

    /// Reduce a list of assigned names to those bound outside — the variables a construct
    /// must thread through its merge points — keeping first-seen order and dropping dups.
    fn carried(&self, mut names: Vec<String>) -> Vec<String> {
        names.retain(|n| self.lookup(n).is_some());
        let mut seen = std::collections::HashSet::new();
        names.retain(|n| seen.insert(n.clone()));
        names
    }

    // ---- helpers ----

    fn unit(&mut self, ty: TyId) -> Value {
        self.b.emit(Op::ConstUnit, Repr::Unit, ty)
    }

    /// A not-yet-lowered expression, named by its `ExprKind`.
    fn unhandled(&mut self, e: &Expr, repr: Repr, ty: TyId) -> Value {
        self.unhandled_note(kind_name(&e.kind), repr, ty)
    }

    /// The placeholder for a form with no lowering: a string constant carrying the note,
    /// emitted at the repr the expression was *supposed* to have so the rest of the
    /// function still lowers and the IR stays well-formed. `compiler/tests/ir_lower.rs`
    /// scans dumps for these markers and reports what remains; it does not fail on them,
    /// so a marker is a visible gap rather than a build break. The value itself is a lie
    /// about its repr — a program that reaches one at runtime is undefined.
    fn unhandled_note(&mut self, what: &str, repr: Repr, ty: TyId) -> Value {
        self.b.emit(Op::ConstStr(format!("<todo: {what}>")), repr, ty)
    }
}

/// A short name for an expression form, used only in `<todo: ...>` markers.
fn kind_name(k: &ExprKind) -> &'static str {
    match k {
        ExprKind::Match { .. } => "match",
        ExprKind::List(_) => "list literal",
        ExprKind::RecordLit { .. } => "record literal",
        ExprKind::Tuple(_) => "tuple",
        ExprKind::Lambda { .. } => "lambda",
        ExprKind::Loop { .. } => "loop",
        ExprKind::While { .. } => "while",
        ExprKind::For { .. } => "for",
        ExprKind::Break(_) => "break",
        ExprKind::Continue => "continue",
        ExprKind::Throw(_) => "throw",
        ExprKind::Try { .. } => "try",
        ExprKind::Is { .. } => "is",
        ExprKind::As { .. } => "as",
        ExprKind::Index { .. } => "index",
        ExprKind::Field { .. } => "field",
        ExprKind::Assert { .. } => "assert",
        _ => "expr",
    }
}

/// Record a function's declared error type on its builder, so its result becomes a tagged
/// union. A `never` clause means it does not throw and the result stays the plain value.
fn set_throws(
    b: &mut Builder,
    env: &Env,
    throws: TyId,
    subst: &std::collections::HashMap<String, Repr>,
) {
    let r = substitute_repr(&repr_of(&env.solver.t, throws), subst);
    if !matches!(r, Repr::Never) {
        b.set_throws(r);
    }
}

/// The type to attribute to a test or projection emitted against a match subject. Pattern
/// syntax is not checked expressions, so none of it has a recorded type of its own; the
/// subject's is the closest honest answer.
fn subj_ty(b: &Builder, v: Value) -> TyId {
    b.value_ty(v)
}

/// The runtime `to_string` symbol for a primitive repr, for string interpolation. A
/// `str` needs none (identity); a user type needs a Display dispatch instead.
/// A lowered assertion: the bool it turns on, the two operands to report when it fails
/// (absent for an assertion that is not a comparison — there is no "left" and "right" for
/// `assert(is_ready())`), and the source text to name it by.
struct Assertion {
    cond: Value,
    operands: Option<(Value, Value)>,
    text: String,
}

/// The `PrimOp` for a *comparison* binary operator, and `None` for anything else. An
/// assertion over a comparison reports both of its sides, so it has to recognise exactly
/// the operators whose two operands are the interesting thing.
fn comparison_prim(op: BinOp) -> Option<PrimOp> {
    Some(match op {
        BinOp::Eq => PrimOp::Eq,
        BinOp::Ne => PrimOp::Ne,
        BinOp::Lt => PrimOp::Lt,
        BinOp::Le => PrimOp::Le,
        BinOp::Gt => PrimOp::Gt,
        BinOp::Ge => PrimOp::Ge,
        _ => return None,
    })
}

/// Render an expression back to something that reads like the source the author wrote, for
/// an assertion's failure message.
///
/// Deliberately *not* the formatter: that one prints a whole module against a token stream
/// and a comment table, and needs the source text. This needs one expression, from the AST
/// alone, on a single line. Forms that do not appear inside an assertion collapse to a
/// placeholder rather than growing a second formatter here — a message reading
/// `assertion failed: <expr>` with the values still attached is the honest failure mode.
fn describe(e: &Expr) -> String {
    match &e.kind {
        ExprKind::Int(n) => n.to_string(),
        ExprKind::Float(s) => s.clone(),
        ExprKind::Bool(b) => b.to_string(),
        ExprKind::Null => "null".into(),
        ExprKind::Rune(c) => format!("'{c}'"),
        ExprKind::Atom(a) => format!(":{a}"),
        ExprKind::Path(p) => p.join("::"),
        ExprKind::Str(parts) => {
            let mut s = String::from("\"");
            for p in parts {
                match p {
                    ast::StrPart::Text(t) => s.push_str(t),
                    ast::StrPart::Interp(i) => s.push_str(&format!("#{{{}}}", describe(i))),
                }
            }
            s.push('"');
            s
        }
        ExprKind::Unary { op, rhs } => {
            let o = match op {
                ast::UnOp::Neg => "-",
                ast::UnOp::Not => "not ",
                ast::UnOp::Bnot => "bnot ",
            };
            format!("{o}{}", describe(rhs))
        }
        // Bracketed by the same precedence table the parser and formatter share, so the
        // message reads back as what the author wrote: `assert(1 + 1 == 3)` reports
        // `1 + 1 == 3`, not `(1 + 1) == 3`.
        ExprKind::Binary { op, lhs, rhs } => {
            format!("{} {} {}", describe_at(lhs, op.prec()), op.text(), describe_at(rhs, op.prec()))
        }
        ExprKind::Call { callee, args, .. } => {
            let a: Vec<String> = args.iter().map(describe).collect();
            format!("{}({})", describe(callee), a.join(", "))
        }
        ExprKind::Index { base, index } => format!("{}[{}]", describe_sub(base), describe(index)),
        ExprKind::Field { base, name } => format!("{}.{name}", describe_sub(base)),
        ExprKind::Tuple(elems) => {
            let a: Vec<String> = elems.iter().map(describe).collect();
            format!("({})", a.join(", "))
        }
        ExprKind::List(elems) => {
            let a: Vec<String> = elems
                .iter()
                .map(|el| match el {
                    ast::Elem::Value(x) => describe(x),
                    ast::Elem::Spread(x) => format!("..{}", describe(x)),
                })
                .collect();
            format!("[{}]", a.join(", "))
        }
        ExprKind::Is { lhs, .. } => format!("{} is _", describe_sub(lhs)),
        ExprKind::As { lhs, .. } => format!("{} as _", describe_sub(lhs)),
        _ => "<expr>".into(),
    }
}

/// `describe`, bracketed when the sub-expression binds less tightly than `min_prec`.
fn describe_at(e: &Expr, min_prec: u8) -> String {
    match &e.kind {
        ExprKind::Binary { op, .. } if op.prec() < min_prec => format!("({})", describe(e)),
        _ => describe(e),
    }
}

/// `describe`, bracketed for a position that takes a postfix — `x.f`, `xs[i]`. Anything
/// operator-shaped needs brackets there whatever its precedence.
fn describe_sub(e: &Expr) -> String {
    match &e.kind {
        ExprKind::Binary { .. } | ExprKind::Unary { .. } => format!("({})", describe(e)),
        _ => describe(e),
    }
}

fn to_string_symbol(r: &Repr) -> Option<String> {
    Some(match r {
        Repr::I64 => "neon_i64_to_string",
        Repr::F64 => "neon_f64_to_string",
        Repr::Bool => "neon_bool_to_string",
        _ => return None,
    }
    .to_string())
}

// ---- scanning for a lambda's free variables ----

/// Every name a pattern binds, so a scan can treat them as bound in whatever scope the
/// pattern opens. A record field with no sub-pattern binds the field's own name.
fn pattern_names(p: &ast::Pattern, out: &mut Vec<String>) {
    match &p.kind {
        ast::PatternKind::Bind(n) => out.push(n.clone()),
        ast::PatternKind::Tuple(ps) => ps.iter().for_each(|s| pattern_names(s, out)),
        ast::PatternKind::Record { fields, .. } => {
            for f in fields {
                match &f.pat {
                    Some(sub) => pattern_names(sub, out),
                    None => out.push(f.name.clone()),
                }
            }
        }
        _ => {}
    }
}

/// A block's free names. `bound` is threaded by `&mut` here, not cloned, because a `let`
/// in a block scopes to the rest of that block — the statements after it must see the name
/// as bound. Constructs whose bindings scope more narrowly (a lambda, a match arm, a `for`
/// pattern, a `catch`) clone `bound` instead so their names do not leak to their siblings.
fn collect_free(block: &Block, bound: &mut std::collections::HashSet<String>, used: &mut Vec<String>) {
    for s in &block.stmts {
        match &s.kind {
            StmtKind::Let { pat, value, .. } => {
                collect_free_expr(value, bound, used);
                let mut names = Vec::new();
                pattern_names(pat, &mut names);
                bound.extend(names);
            }
            StmtKind::Assign { value, .. } => collect_free_expr(value, bound, used),
            StmtKind::Expr(e) => collect_free_expr(e, bound, used),
            StmtKind::Error => {}
        }
    }
    if let Some(t) = &block.tail {
        collect_free_expr(t, bound, used);
    }
}

/// An expression's free names, appended to `used` in first-use order and with duplicates
/// left in — `free_vars` filters and dedups. Only a single-segment path counts as a
/// variable use; a qualified path is a module item, which is not captured.
fn collect_free_expr(
    e: &Expr,
    bound: &mut std::collections::HashSet<String>,
    used: &mut Vec<String>,
) {
    match &e.kind {
        ExprKind::Path(p) => {
            if let [name] = p.as_slice() {
                if !bound.contains(name) {
                    used.push(name.clone());
                }
            }
        }
        ExprKind::Lambda { params, body } => {
            // A nested lambda's own parameters are bound within it; do not leak them out.
            let mut inner = bound.clone();
            inner.extend(params.iter().map(|p| p.name.clone()));
            collect_free_expr(body, &mut inner, used);
        }
        ExprKind::Unary { rhs, .. } => collect_free_expr(rhs, bound, used),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_free_expr(lhs, bound, used);
            collect_free_expr(rhs, bound, used);
        }
        ExprKind::Call { callee, args, .. } => {
            collect_free_expr(callee, bound, used);
            args.iter().for_each(|a| collect_free_expr(a, bound, used));
        }
        ExprKind::Index { base, index } => {
            collect_free_expr(base, bound, used);
            collect_free_expr(index, bound, used);
        }
        ExprKind::Field { base, .. } => collect_free_expr(base, bound, used),
        ExprKind::List(elems) => elems.iter().for_each(|el| match el {
            ast::Elem::Value(x) | ast::Elem::Spread(x) => collect_free_expr(x, bound, used),
        }),
        ExprKind::RecordLit { fields, spread, .. } => {
            fields.iter().for_each(|f| collect_free_expr(&f.value, bound, used));
            if let Some(s) = spread {
                collect_free_expr(s, bound, used);
            }
        }
        ExprKind::Tuple(es) => es.iter().for_each(|x| collect_free_expr(x, bound, used)),
        ExprKind::If { cond, then, else_ } => {
            collect_free_expr(cond, bound, used);
            collect_free(then, bound, used);
            if let Some(x) = else_ {
                collect_free_expr(x, bound, used);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_free_expr(scrutinee, bound, used);
            for arm in arms {
                // Arm-pattern bindings scope to the arm; over-approximate by adding them
                // to `bound` (a capture named the same as an arm binding is rare).
                let mut names = Vec::new();
                pattern_names(&arm.pat, &mut names);
                let mut inner = bound.clone();
                inner.extend(names);
                if let Some(g) = &arm.guard {
                    collect_free_expr(g, &mut inner, used);
                }
                collect_free_expr(&arm.body, &mut inner, used);
            }
        }
        ExprKind::Block(b) => collect_free(b, bound, used),
        ExprKind::Loop { body } => collect_free(body, bound, used),
        ExprKind::While { cond, body } => {
            collect_free_expr(cond, bound, used);
            collect_free(body, bound, used);
        }
        ExprKind::For { pat, iter, body } => {
            collect_free_expr(iter, bound, used);
            let mut names = Vec::new();
            pattern_names(pat, &mut names);
            let mut inner = bound.clone();
            inner.extend(names);
            collect_free(body, &mut inner, used);
        }
        ExprKind::Break(Some(x)) | ExprKind::Return(Some(x)) | ExprKind::Throw(x) => {
            collect_free_expr(x, bound, used)
        }
        ExprKind::Try { body, catch, .. } => {
            collect_free_expr(body, bound, used);
            if let Some(c) = catch {
                let mut inner = bound.clone();
                inner.insert(c.binding.clone());
                collect_free(&c.body, &mut inner, used);
            }
        }
        ExprKind::Is { lhs, .. } | ExprKind::As { lhs, .. } => collect_free_expr(lhs, bound, used),
        ExprKind::Assert { args, .. } => args.iter().for_each(|a| collect_free_expr(a, bound, used)),
        _ => {}
    }
}

// ---- scanning for a loop's reassigned variables ----
//
// Descends into every sub-expression except a lambda's body: a closure cannot reassign
// a capture (captures are sealed), so it never contributes to a loop's carried set.

/// Every name a block assigns to. A `let` is not an assignment — it introduces a new
/// binding, so only its initialiser is scanned. Names bound *inside* the block are not
/// filtered out here; `Lower::carried` drops them by checking what is actually in scope.
fn collect_assigns_block(b: &Block, out: &mut Vec<String>) {
    for s in &b.stmts {
        match &s.kind {
            StmtKind::Let { value, .. } => collect_assigns_expr(value, out),
            StmtKind::Assign { name, value } => {
                out.push(name.clone());
                collect_assigns_expr(value, out);
            }
            StmtKind::Expr(e) => collect_assigns_expr(e, out),
            StmtKind::Error => {}
        }
    }
    if let Some(t) = &b.tail {
        collect_assigns_expr(t, out);
    }
}

/// The expression half of the assignment scan. Exhaustive over the forms that can contain
/// a statement or a sub-expression; the catch-all arm covers the leaves (literals, paths,
/// `break`/`return` with no value) and the one deliberate omission, a lambda body.
fn collect_assigns_expr(e: &Expr, out: &mut Vec<String>) {
    match &e.kind {
        ExprKind::Unary { rhs, .. } => collect_assigns_expr(rhs, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_assigns_expr(lhs, out);
            collect_assigns_expr(rhs, out);
        }
        ExprKind::Call { callee, args, .. } => {
            collect_assigns_expr(callee, out);
            args.iter().for_each(|a| collect_assigns_expr(a, out));
        }
        ExprKind::Index { base, index } => {
            collect_assigns_expr(base, out);
            collect_assigns_expr(index, out);
        }
        ExprKind::Field { base, .. } => collect_assigns_expr(base, out),
        ExprKind::List(elems) => {
            for el in elems {
                match el {
                    ast::Elem::Value(x) | ast::Elem::Spread(x) => collect_assigns_expr(x, out),
                }
            }
        }
        ExprKind::RecordLit { fields, spread, .. } => {
            fields.iter().for_each(|f| collect_assigns_expr(&f.value, out));
            if let Some(s) = spread {
                collect_assigns_expr(s, out);
            }
        }
        ExprKind::Tuple(es) => es.iter().for_each(|x| collect_assigns_expr(x, out)),
        ExprKind::If { cond, then, else_ } => {
            collect_assigns_expr(cond, out);
            collect_assigns_block(then, out);
            if let Some(e) = else_ {
                collect_assigns_expr(e, out);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_assigns_expr(scrutinee, out);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    collect_assigns_expr(g, out);
                }
                collect_assigns_expr(&arm.body, out);
            }
        }
        ExprKind::Block(b) => collect_assigns_block(b, out),
        ExprKind::Loop { body } => collect_assigns_block(body, out),
        ExprKind::While { cond, body } => {
            collect_assigns_expr(cond, out);
            collect_assigns_block(body, out);
        }
        ExprKind::For { iter, body, .. } => {
            collect_assigns_expr(iter, out);
            collect_assigns_block(body, out);
        }
        ExprKind::Break(Some(e)) | ExprKind::Return(Some(e)) | ExprKind::Throw(e) => {
            collect_assigns_expr(e, out)
        }
        ExprKind::Try { body, catch, .. } => {
            collect_assigns_expr(body, out);
            if let Some(c) = catch {
                collect_assigns_block(&c.body, out);
            }
        }
        ExprKind::Is { lhs, .. } | ExprKind::As { lhs, .. } => collect_assigns_expr(lhs, out),
        ExprKind::Assert { args, .. } => args.iter().for_each(|a| collect_assigns_expr(a, out)),
        // A lambda's body is a separate scope that cannot reassign a capture.
        _ => {}
    }
}

/// The repr of a tuple element, read off the base's own repr rather than the checker.
/// A tuple pattern's sub-positions are not checked expressions, so this is the only
/// place the element layout is available.
fn elem_repr(b: &Builder, base: Value, index: usize) -> Repr {
    match b.value_repr(base) {
        Repr::Tuple(rs) => rs.get(index).cloned().unwrap_or(Repr::Unit),
        _ => Repr::Unit,
    }
}

/// The repr of a record field, searched through unions and nullables so a pattern can
/// project out of a value the checker has not narrowed yet. The first variant carrying the
/// field wins, so two variants with a same-named field of different layout would silently
/// resolve to one of them.
fn field_repr(b: &Builder, base: Value, field: &str) -> Repr {
    fn find(r: &Repr, field: &str) -> Option<Repr> {
        match r {
            Repr::Record { fields, .. } => {
                fields.iter().find(|(n, _)| n == field).map(|(_, r)| r.clone())
            }
            // Destructuring a union or nullable value: the field lives in a variant.
            Repr::Union(vs) => vs.iter().find_map(|v| find(v, field)),
            Repr::Nullable(inner) => find(inner, field),
            _ => None,
        }
    }
    find(b.value_repr(base), field).unwrap_or(Repr::Unit)
}

/// The scalar a value collapses to once its `null` variant is excluded — the type a literal
/// pattern compares against.
fn variant_scalar(r: &Repr) -> Repr {
    match r {
        Repr::Union(vs) => vs.iter().find(|v| !matches!(v, Repr::Null)).cloned().unwrap_or(Repr::I64),
        other => other.clone(),
    }
}

fn un_prim(op: UnOp) -> PrimOp {
    match op {
        UnOp::Neg => PrimOp::Neg,
        UnOp::Not => PrimOp::Not,
        UnOp::Bnot => PrimOp::Bnot,
    }
}

/// The `PrimOp` a binary operator maps to, or `None` for the four that are not a single
/// instruction. This is the one place that split is decided; `lower_binary` reads it rather
/// than keeping a second list that could disagree.
fn bin_prim(op: BinOp) -> Option<PrimOp> {
    Some(match op {
        BinOp::Add => PrimOp::Add,
        BinOp::Sub => PrimOp::Sub,
        BinOp::Mul => PrimOp::Mul,
        BinOp::Div => PrimOp::Div,
        BinOp::Rem => PrimOp::Rem,
        BinOp::Eq => PrimOp::Eq,
        BinOp::Ne => PrimOp::Ne,
        BinOp::Lt => PrimOp::Lt,
        BinOp::Le => PrimOp::Le,
        BinOp::Gt => PrimOp::Gt,
        BinOp::Ge => PrimOp::Ge,
        BinOp::Band => PrimOp::Band,
        BinOp::Bor => PrimOp::Bor,
        BinOp::Bxor => PrimOp::Bxor,
        BinOp::Bsl => PrimOp::Bsl,
        BinOp::Bsr => PrimOp::Bsr,
        // Short-circuit and null/pipe forms desugar; not a plain prim.
        BinOp::And | BinOp::Or | BinOp::Orelse | BinOp::Pipe => return None,
    })
}

#[cfg(test)]
mod tests;
