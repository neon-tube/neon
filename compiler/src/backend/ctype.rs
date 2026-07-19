//! The C type table: every aggregate repr that needs a named C `struct` — records and
//! tuples — is collected from the program, given a stable name, and its definition emitted
//! in dependency order (a struct used by value as a field must be defined first). Reprs
//! that already have a runtime type (`str`, `list*`, closures) or are pointer-shaped
//! (nullable) do not need an entry; they map straight to a C type in [`TypeTable::c_type`].

use crate::ir::repr::Repr;
use crate::ir::ssa::Program;
use crate::typecheck::types::TyId;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

/// The names of the lifted lambdas in a program — functions that already take the
/// `(env, args…)` closure ABI, as opposed to ordinary functions that need an adapter
/// thunk to be used as a closure value.
pub fn lambda_names(program: &Program) -> HashSet<String> {
    program.funcs.iter().filter(|f| f.env.is_some()).map(|f| f.name.clone()).collect()
}

/// Names the aggregate structs a program needs and emits their C definitions.
pub struct TypeTable {
    /// Every recursive type, paired with its unfolding. A back-edge names a type without
    /// describing it, so laying one out or refcounting it means resolving through here.
    recursive: HashMap<TyId, Repr>,
    /// Recursive types already registered, so a cycle is walked once rather than forever.
    resolved: HashSet<TyId>,
    /// Record atoms whose cycle closes by value, paired with their pointee layout.
    boxed: HashMap<u32, Repr>,
    /// The heap wrapper for each: `struct nbN { neon_header header; nrN value; }`. The
    /// header comes first, so an `nbN*` is also its `neon_header*` — the trick `neon_list`
    /// uses — and a field read stays a single `->`.
    boxed_names: HashMap<u32, String>,
    /// Structural key → C struct name, for records and tuples.
    names: HashMap<String, String>,
    /// `(name, repr)` in discovery order; dependencies always registered before dependents.
    defs: Vec<(String, Repr)>,
    /// Structural key → value-witness name, for every container element type.
    witness_names: HashMap<String, String>,
    /// `(name, element repr)` for each witness the program needs.
    witness_defs: Vec<(String, Repr)>,
    /// Structural key → env-drop name, for each non-empty closure environment.
    env_drop_names: HashMap<String, String>,
    /// `(name, environment tuple repr)` for each closure-env drop the program needs.
    env_drop_defs: Vec<(String, Repr)>,
    /// Lifted lambda names, so a closure of a lambda calls it directly while a closure of
    /// an ordinary function goes through an adapter thunk.
    lambdas: HashSet<String>,
    /// Function name → its parameter reprs, for coercing arguments at call sites.
    params: HashMap<String, Vec<Repr>>,
    /// Function name → the tagged result it returns, for throwing functions only.
    results: HashMap<String, Repr>,
    /// Structural key → key-witness name, for each type used as a map key.
    key_witness_names: HashMap<String, String>,
    key_witness_defs: Vec<(String, Repr)>,
}

/// A repr that codegen cannot pin to a C type. Every one of these was a silent
/// `neon_value` until now, and each silence cost a bug: an unpinned repr becomes a bare
/// `void*` that C accepts anywhere a pointer is accepted, so the value is not erased —
/// it is *typed* as erased while holding unboxed bits, with no header, witness or tag.
/// `is`/`as` then read a garbage tag, and a field read lands at the wrong offset.
///
/// Panicking is the point. The alternative is emitting C that compiles and is wrong,
/// which is how the previous implementation died.
fn ice(r: &Repr, what: &str) -> ! {
    panic!("internal error: codegen reached {what}: {r:?}")
}

impl TypeTable {
    /// Collect every record and tuple repr reachable in the program.
    pub fn build(program: &Program) -> TypeTable {
        let mut t = TypeTable {
            recursive: program.recursive.clone(),
            resolved: HashSet::new(),
            boxed: program.boxed.clone(),
            boxed_names: HashMap::new(),
            names: HashMap::new(),
            defs: Vec::new(),
            witness_names: HashMap::new(),
            witness_defs: Vec::new(),
            env_drop_names: HashMap::new(),
            env_drop_defs: Vec::new(),
            lambdas: lambda_names(program),
            params: program
                .funcs
                .iter()
                .map(|f| {
                    (f.name.clone(), f.params.iter().map(|&p| f.value_repr(p).clone()).collect())
                })
                .collect(),
            key_witness_names: HashMap::new(),
            key_witness_defs: Vec::new(),
            results: program
                .funcs
                .iter()
                .filter_map(|f| f.result_repr().map(|r| (f.name.clone(), r)))
                .collect(),
        };
        for f in &program.funcs {
            t.register(&f.ret);
            for &p in &f.params {
                t.register(f.value_repr(p));
            }
            for v in f.values() {
                t.register(f.value_repr(v));
            }
            // A throwing function returns a tagged result; register its layout.
            if let Some(res) = f.result_repr() {
                t.register(&res);
            }
            // A closure with captures needs a drop for its boxed environment.
            if let Some(env) = &f.env {
                if matches!(env, Repr::Tuple(fields) if !fields.is_empty()) {
                    t.register(env);
                    t.intern_env_drop(env);
                }
            }
        }
        t
    }

    /// Register a repr and its aggregate sub-reprs, giving each record/tuple a name. Fields
    /// are registered first so a struct's dependencies always precede it in `defs`.
    fn register(&mut self, r: &Repr) {
        // Anything concrete may be erased into `any`, and boxing needs the value-witness
        // for its size and release, so every such repr gets one.
        if is_boxable(r) {
            self.intern_witness(r);
        }
        match r {
            Repr::Record { fields, .. } => {
                for (_, fr) in fields {
                    self.register(fr);
                }
                self.intern(r, "nr");
            }
            Repr::Tuple(elems) => {
                for e in elems {
                    self.register(e);
                }
                self.intern(r, "nt");
            }
            Repr::List(e) => {
                self.register(e);
                self.intern_witness(e);
            }
            Repr::Runtime { args, .. } => {
                for a in args {
                    self.register(a);
                    self.intern_witness(a);
                }
            }
            Repr::Nullable(e) => self.register(e),
            Repr::Map(k, v) => {
                self.register(k);
                self.register(v);
                self.intern_witness(k);
                self.intern_witness(v);
                // Only a map key is hashed, so only a map key gets a key-witness.
                self.intern_key_witness(k);
            }
            Repr::Union(vs) => {
                for v in vs {
                    self.register(v);
                }
                self.intern(r, "nu");
            }
            Repr::Closure { params, throws, ret } => {
                for p in params {
                    self.register(p);
                }
                self.register(ret);
                // A throwing closure's function returns the tagged result; register its
                // layout so a call's cast (and the value it lands in) can name the struct
                // even when this program never stores such a result itself.
                if !matches!(throws.as_ref(), Repr::Never) {
                    self.register(throws);
                    self.register(&Repr::Union(vec![ret.as_ref().clone(), throws.as_ref().clone()]));
                }
            }
            // A back-edge carries no structure, so register the type it names instead —
            // once. The guard is what stops the cycle re-entering itself forever.
            Repr::Recursive(ty) => {
                if self.resolved.insert(*ty) {
                    if let Some(u) = self.recursive.get(ty).cloned() {
                        self.register(&u);
                    }
                }
            }
            // A heap-allocated record: register the layout it points at, then the wrapper
            // that carries the header in front of it.
            Repr::BoxedRec(atom) => {
                if self.boxed_names.contains_key(atom) {
                    return;
                }
                if let Some(shape) = self.boxed.get(atom).cloned() {
                    let name = format!("nb{}", self.defs.len());
                    self.boxed_names.insert(*atom, name.clone());
                    self.defs.push((name, Repr::BoxedRec(*atom)));
                    self.register(&shape);
                    // The header/value layout is exactly a closure environment's, so the
                    // same drop generator serves: release the counted fields, free the
                    // block. `neon_release` calls `drop` unconditionally, so a boxed
                    // record without one segfaults the moment its count reaches zero.
                    self.intern_env_drop(&shape);
                }
            }
            _ => {}
        }
    }

    /// Assign a fresh `<prefix><n>` name to a repr if it has none yet.
    fn intern(&mut self, r: &Repr, prefix: &str) {
        let k = key_with(r, &self.recursive);
        if self.names.contains_key(&k) {
            return;
        }
        let name = format!("{prefix}{}", self.defs.len());
        self.names.insert(k, name.clone());
        self.defs.push((name, r.clone()));
    }

    /// Assign a value-witness name to an element repr if it has none yet.
    fn intern_witness(&mut self, r: &Repr) {
        let k = key_with(r, &self.recursive);
        if self.witness_names.contains_key(&k) {
            return;
        }
        let name = format!("nw{}", self.witness_defs.len());
        self.witness_names.insert(k, name.clone());
        self.witness_defs.push((name, r.clone()));
    }

    /// Assign a key-witness name to a map key repr if it has none yet.
    fn intern_key_witness(&mut self, r: &Repr) {
        let k = key_with(r, &self.recursive);
        if self.key_witness_names.contains_key(&k) {
            return;
        }
        let name = format!("nkw{}", self.key_witness_defs.len());
        self.key_witness_names.insert(k, name.clone());
        self.key_witness_defs.push((name, r.clone()));
    }

    /// The address-of expression for a map key's key-witness.
    ///
    /// Ices on a miss. `0` is a null `neon_key_witness*`, and the runtime dereferences it
    /// unconditionally to hash and compare — there is no "no key witness" behaviour for it
    /// to fall back to, so the old default bought a segfault at the first insertion in
    /// exchange for hiding which key repr was never interned.
    pub fn key_witness_ref(&self, r: &Repr) -> String {
        match self.key_witness_names.get(&key_with(r, &self.recursive)) {
            Some(n) => format!("&{n}"),
            None => ice(r, "a map key with no interned key-witness"),
        }
    }

    /// Every key-witness the program needs, as `(name, key repr)`.
    pub fn key_witnesses(&self) -> &[(String, Repr)] {
        &self.key_witness_defs
    }

    /// The address-of expression for an element type's value-witness.
    ///
    /// Ices on a miss, for the same reason `key_witness_ref` does. A witness carries the
    /// element's *size*, and a container handed a null one has no way to know how many
    /// bytes a slot is; it is also what boxing into `any` records so the value can be
    /// released later. `0` here is not "no witness needed", it is a container that cannot
    /// describe its own contents.
    pub fn witness_ref(&self, r: &Repr) -> String {
        match self.witness_names.get(&key_with(r, &self.recursive)) {
            Some(n) => format!("&{n}"),
            None => ice(r, "an element type with no interned value-witness"),
        }
    }

    /// Every value-witness the program needs, as `(name, element repr)`.
    pub fn witnesses(&self) -> &[(String, Repr)] {
        &self.witness_defs
    }

    /// Assign an env-drop name to a closure-environment repr if it has none yet.
    fn intern_env_drop(&mut self, r: &Repr) {
        let k = key_with(r, &self.recursive);
        if self.env_drop_names.contains_key(&k) {
            return;
        }
        let name = format!("ned{}", self.env_drop_defs.len());
        self.env_drop_names.insert(k, name.clone());
        self.env_drop_defs.push((name, r.clone()));
    }

    /// The env-drop function name for a closure environment.
    ///
    /// Ices on a miss. The old default was `0`, documented as "an empty environment has
    /// nothing to release" — but an empty environment never reaches here:
    /// `emit_make_closure` returns early when there are no captures, and a boxed record's
    /// shape is interned by `register` alongside its wrapper. So the only way to get a `0`
    /// was a *non*-empty environment whose repr had not been interned, and `0` there is a
    /// `neon_header` freed without releasing anything it captured — a silent leak of every
    /// counted capture, on the one path (`neon_release` reaching zero) that no test watches.
    /// A drop is also mandatory for a boxed record: `neon_release` calls it unconditionally.
    pub fn env_drop_ref(&self, r: &Repr) -> String {
        match self.env_drop_names.get(&key_with(r, &self.recursive)) {
            Some(n) => n.clone(),
            None => ice(r, "a closure environment with no interned drop"),
        }
    }

    /// Every closure-env drop the program needs, as `(name, env tuple repr)`.
    pub fn env_drops(&self) -> &[(String, Repr)] {
        &self.env_drop_defs
    }

    /// Whether a function name is a lifted lambda (already has the closure ABI).
    pub fn is_lambda(&self, name: &str) -> bool {
        self.lambdas.contains(name)
    }

    /// The tagged result a function returns, if it throws. A call's result value is typed
    /// by this rather than by its declared return.
    pub fn result_of(&self, name: &str) -> Option<&Repr> {
        self.results.get(name)
    }

    /// A function's parameter reprs, for coercing arguments at a call site.
    pub fn param_reprs(&self, name: &str) -> Option<&[Repr]> {
        self.params.get(name).map(Vec::as_slice)
    }

    /// Resolve a back-edge to the type it names. A `Recursive` says only *which* type,
    /// so anything that needs the shape — a layout, a refcount walk — goes through here.
    pub fn resolve<'a>(&'a self, r: &'a Repr) -> &'a Repr {
        match r {
            // CORRECT DEFAULT: a back-edge naming a type the table does not hold stays a
            // back-edge, and every caller then refuses rather than guessing — `c_type` ices
            // on an unresolved `Recursive`, `eq_expr` and `hash_expr` ice, and
            // `rc_parts_rec` stops on the `seen` chain. Returning `r` keeps that one
            // decision in the callers instead of inventing a shape here.
            Repr::Recursive(ty) => self.recursive.get(ty).unwrap_or(r),
            _ => r,
        }
    }

    /// Every boxed record: its wrapper name and pointee layout. Ordered by the name, so
    /// emission is deterministic.
    pub fn boxed_records(&self) -> Vec<(String, Repr)> {
        let mut out: Vec<(String, Repr)> = self
            .boxed_names
            .iter()
            .filter_map(|(atom, name)| self.boxed.get(atom).map(|r| (name.clone(), r.clone())))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Whether a repr is a pointer to a heap-allocated recursive record.
    pub fn is_boxed(&self, r: &Repr) -> bool {
        matches!(r, Repr::BoxedRec(_))
    }

    /// The wrapper name and pointee layout of a boxed record, for construction and field
    /// access. A boxed value *is* the pointer, so this is the only way to its shape.
    pub fn boxed_shape(&self, r: &Repr) -> Option<(&str, &Repr)> {
        let Repr::BoxedRec(atom) = r else { return None };
        Some((self.boxed_names.get(atom)?, self.boxed.get(atom)?))
    }

    /// The name a boxed value's type tag is derived from. It has to agree with what an
    /// `is` test asks for, so a nominal record uses its own name and a primitive its
    /// spelling in the language — and a generic type its *arguments*, spelled by this same
    /// function. `is` and the box tag are two readings of one string: `Op::IsVariant` calls
    /// this on the type the checker resolved the test to, and `coerce_expr` calls it on the
    /// repr being boxed. Deriving them separately is what made `List[i64] is List[str]`
    /// true, and `as` trusts `is`.
    ///
    /// The structural key (`key_with`) already distinguishes all of this and already cuts
    /// recursion, and it is deliberately *not* reused as the tag: it keys interning, so it
    /// spells a runtime type's C symbol (`N{nominal}@{c_type}`), which is an implementation
    /// detail an `is` must not be able to observe, and it separates `Nullable(T)` from `T`,
    /// where a tag deliberately names a nullable after its payload. Its back-edge form is
    /// borrowed for the cycle cut below, which is the part worth sharing.
    ///
    /// A method rather than a free function because a *recursive* record is a
    /// `BoxedRec(atom)` — an atom id and nothing else — and only the type table can turn
    /// that back into the name the source wrote. It could not, and the old catch-all
    /// answered with the structural key `P3`: `let a: any = Node { .. }; a is Node` was
    /// silently `false` for every self-referencing record while a flat one answered `true`.
    /// INJECTIVITY OBLIGATION: this is an identity, and the sharpest one in the compiler
    /// — it is what `is` compares, and `as` is an unchecked reinterpretation that trusts
    /// `is`. A collision here is not a wrong answer, it is a wrong CAST. Every arm
    /// therefore spells the whole shape of its variant and elides nothing; three separate
    /// bugs (`List` without its element, `Runtime` without its arguments, `BoxedRec`
    /// falling through to a structural key) and later a fourth (`Closure` as the constant
    /// `"fn"`) were each an arm that had dropped a component.
    ///
    /// It is injective over `Repr` EXCEPT for two deliberate collapses, both of which
    /// identify things that genuinely are one type: `Nullable(T)` is spelled as `T`, so a
    /// nullable answers to its payload's name; and a `Recursive` back-edge is spelled as
    /// the type it names, so a `mu` type and its unfolding agree.
    ///
    /// It is NOT injective over the source language, and that is an open bug rather than
    /// a design choice: `Record { name: Some(n), .. }` spells `n`, which
    /// `typecheck/env.rs::record_body` built from the bare identifier with no module
    /// path, so two modules declaring the same record name collide here. See
    /// `tests/lang/types/a_nominal_name_is_not_a_module_identity.neon`, which fails today.
    /// The fix belongs upstream in the checker, not in this function — by the time a
    /// `Repr` arrives the module is already gone.
    pub fn type_tag_name(&self, r: &Repr) -> String {
        self.tag_name(r, &mut Vec::new())
    }

    /// `type_tag_name`, carrying the chain of aggregates currently being spelled.
    ///
    /// `open` holds the structural key of every repr on the path from the root. A type
    /// graph is cyclic — `record Node { next: Node | null }` — so a name that walks into
    /// arguments and fields has to cut somewhere, and the cut is a back-edge spelled with
    /// `key_with`'s `Z`/`P` form: stable, and already the identity the type table interns
    /// on. This mirrors `key_with`'s own back-edge handling rather than inventing a second
    /// scheme; without it, spelling `Node` recursed until the stack ran out.
    fn tag_name(&self, r: &Repr, open: &mut Vec<String>) -> String {
        // Aggregates are the only reprs that can lead back to themselves, and the only
        // ones whose spelling descends, so the cycle check is theirs alone.
        let recurses = matches!(
            r,
            Repr::Record { .. }
                | Repr::BoxedRec(_)
                | Repr::Recursive(_)
                | Repr::List(_)
                | Repr::Map(_, _)
                | Repr::Runtime { .. }
                | Repr::Nullable(_)
                | Repr::Closure { .. }
        );
        if recurses {
            let k = key_with(r, &self.recursive);
            if open.contains(&k) {
                return format!("^{k}");
            }
            open.push(k);
            let out = self.tag_name_inner(r, open);
            open.pop();
            return out;
        }
        self.tag_name_inner(r, open)
    }

    fn tag_name_inner(&self, r: &Repr, open: &mut Vec<String>) -> String {
        let arg = |x: &Repr, open: &mut Vec<String>| self.tag_name(x, open);
        match r {
            Repr::I64 => "i64".into(),
            Repr::F64 => "f64".into(),
            Repr::Bool => "bool".into(),
            Repr::Str => "str".into(),
            Repr::Null => "null".into(),
            Repr::Unit => "unit".into(),
            Repr::Tag => "atom".into(),
            // The arguments are part of the name, and that is the whole fix. A tag of
            // `List` was shared by `List[i64]` and `List[str]`, so `is` — the guard a
            // person writes *before* `as` — could not tell them apart and the cast that
            // followed reinterpreted the payload. Nesting is by recursion, so
            // `List[List[i64]]` and `List[List[str]]` differ too: the name is the type's
            // whole shape, not its first level.
            Repr::List(e) => format!("List[{}]", arg(e, open)),
            Repr::Map(k, v) => format!("Map[{},{}]", arg(k, open), arg(v, open)),
            // The signature is part of the name, for the same reason `List`'s element is.
            // `Closure { .. } => "fn"` gave every closure in the program one tag, so `is`
            // could not tell `(i64) -> i64` from `(str) -> str` and answered true for both.
            // `as` trusts `is`, and the cast that follows hands a `neon_str` to a body
            // compiled for `int64_t`. Spelled by recursion like every other argument, so
            // nesting — `((i64) -> i64) -> i64` — is distinguished at every level too.
            Repr::Closure { params, throws, ret } => format!(
                "fn({})!{}->{}",
                params.iter().map(|p| arg(p, open)).collect::<Vec<_>>().join(","),
                arg(throws, open),
                arg(ret, open)
            ),
            // A generic record carries its arguments in its *fields* — `Box[i64]` and
            // `Box[str]` are both `Record { name: Some("Box"), .. }` — so the fields are
            // what distinguishes them. Spelled with names, since a record's identity
            // includes them.
            Repr::Record { name: Some(n), fields } if !fields.is_empty() => {
                let body: Vec<String> =
                    fields.iter().map(|(f, t)| format!("{f}={}", arg(t, open))).collect();
                format!("{n}[{}]", body.join(","))
            }
            Repr::Record { name: Some(n), .. } => n.clone(),
            Repr::Nullable(inner) => arg(inner, open),
            // A back-edge names a type without describing it; tag the type it names, so a
            // `mu` type and its unfolding agree on one tag.
            Repr::Recursive(_) => {
                let resolved = self.resolve(r);
                if matches!(resolved, Repr::Recursive(_)) {
                    key(r)
                } else {
                    // Cloned out first: `resolve` borrows `self`, and the recursive call
                    // needs it again. The `open` entry pushed for this back-edge is what
                    // stops the unfolding walking back into it.
                    let resolved = resolved.clone();
                    self.tag_name(&resolved, open)
                }
            }
            // A self-referencing record: the pointer carries no name, the table does.
            // Its fields go into the name for the same reason a flat record's do — a
            // recursive record can be generic — and the back-edge through `next` is cut by
            // `open`, which already holds this `P{atom}` key.
            Repr::BoxedRec(atom) => match self.boxed.get(atom) {
                Some(shape @ Repr::Record { name: Some(_), .. }) => {
                    let shape = shape.clone();
                    self.tag_name_inner(&shape, open)
                }
                _ => key(r),
            },
            // The Neon name plus its arguments, exactly as the `Record` arm above. The
            // arguments used to be left out on the grounds that `is` names a head type;
            // that was the bug. The source *can* write `a is Resource[i64, str]`, and
            // `Op::IsVariant` now compares the type the checker resolved that to, through
            // this same function, rather than a hash of the last path segment.
            Repr::Runtime { nominal, args, .. } if args.is_empty() => nominal.clone(),
            Repr::Runtime { nominal, args, .. } => {
                format!(
                    "{nominal}[{}]",
                    args.iter().map(|a| arg(a, open)).collect::<Vec<_>>().join(",")
                )
            }
            // Anonymous shapes have no name to test against; their structure is the
            // identity, and `variant_name` in c.rs asks the same question the same way, so
            // the two sides of an `is` still agree.
            Repr::Record { name: None, .. } | Repr::Tuple(_) | Repr::Union(_) => key(r),
            // Never at the root — `coerce_expr` tags the *source* of a box, which is
            // always a concrete value, and `variant_tag` in c.rs filters these out before
            // asking — but reachable as an *argument*, and spelled rather than elided
            // there. `List[never]` (the type of an empty literal that met no expectation)
            // and `List[any]` are distinct types and are distinct answers: `is` reports
            // the type the value actually has, and a `List[never]` is not a `List[i64]`,
            // however convenient it would be to say it was.
            Repr::Any => "any".into(),
            Repr::Never => "never".into(),
            Repr::Var(_) => ice(r, "a type variable being given a type tag"),
        }
    }

    /// The type tag stored in a box for a given repr.
    pub fn type_tag(&self, r: &Repr) -> u64 {
        fnv1a(&self.type_tag_name(r))
    }

    /// The C type for a repr. Aggregates resolve to their struct name (by value); runtime
    /// and scalar reprs map directly.
    pub fn c_type(&self, r: &Repr) -> String {
        match r {
            Repr::I64 => "int64_t".into(),
            Repr::F64 => "double".into(),
            Repr::Bool => "bool".into(),
            Repr::Str => "neon_str".into(),
            Repr::Unit | Repr::Null => "neon_unit".into(),
            Repr::Tag => "uint64_t".into(),
            Repr::List(_) => "neon_list*".into(),
            Repr::Map(_, _) => "neon_map*".into(),
            // The only reader of `c_type`. Unchanged: the repr is a pointer to the C struct
            // the runtime declares, and the Neon name has no bearing on how it is spelled.
            Repr::Runtime { c_type, .. } => format!("{c_type}*"),
            Repr::Closure { .. } => "neon_closure".into(),
            Repr::BoxedRec(atom) => match self.boxed_names.get(atom) {
                Some(n) => format!("{n}*"),
                None => ice(r, "a boxed record with no registered wrapper"),
            },
            Repr::Record { .. } | Repr::Tuple(_) | Repr::Union(_) => self
                .names
                .get(&key_with(r, &self.recursive))
                .cloned()
                .unwrap_or_else(|| ice(r, "an aggregate that was never interned")),
            // A back-edge names a type without describing it, so type the resolution.
            // A recursive union finds its interned struct either way (both spellings
            // share the Z-key), but a recursion that terminates through a pointer —
            // `mu type F = null | (i64) throws E -> F` unfolds to a nullable closure —
            // has no struct at all, and the old `neon_value` fallback disagreed with
            // the value-witness, which is generated from the *resolved* repr and
            // touches `.env` on the member this type declares.
            Repr::Recursive(_) => {
                let resolved = self.resolve(r);
                if matches!(resolved, Repr::Recursive(_)) {
                    ice(r, "a recursive back-edge that does not resolve")
                } else {
                    self.c_type(resolved)
                }
            }
            // A nullable pointer is just the pointer type; `null` is a null pointer.
            Repr::Nullable(inner) => match inner.as_ref() {
                Repr::Str => "neon_str".into(),
                other => self.c_type(other),
            },
            // The one deliberate erasure, and now the only place in the compiler that
            // names `neon_value`. It is reached when the source said `any` — never as a
            // fallback.
            Repr::Any => "neon_value".into(),
            // Uninhabited, and reached often: it is the error half of a result whose
            // function does not throw, so a C type must be spelled even though no value
            // of it exists. `neon_value` deliberately: a pointer and a one-byte struct
            // give any union containing this variant different sizes, and two paths
            // disagreeing about a layout is the failure this file panics to prevent.
            // Not erasure — nothing is ever loaded or stored through it.
            Repr::Never => "neon_value".into(),
            // A type variable here is always a compiler bug: monomorphisation ran, so
            // every repr reaching codegen should be concrete.
            Repr::Var(n) => panic!(
                "internal error: type variable '{n} reached codegen — a repr was built \
                 without applying the instance substitution (see `repr_of_ty`)"
            ),
        }
    }

    /// Emit forward declarations then full definitions, dependencies first.
    pub fn emit_defs(&self, out: &mut String) {
        if self.defs.is_empty() {
            return;
        }
        for (name, _) in &self.defs {
            let _ = writeln!(out, "typedef struct {name} {name};");
        }
        out.push('\n');
        let mut done = HashSet::new();
        for (name, repr) in &self.defs {
            self.emit_one(out, name, repr, &mut done);
        }
        out.push('\n');
    }

    /// Emit one struct after ensuring every struct it embeds by value is emitted first.
    fn emit_one(&self, out: &mut String, name: &str, repr: &Repr, done: &mut HashSet<String>) {
        if !done.insert(name.to_string()) {
            return;
        }
        // Emit by-value struct dependencies first.
        let deps: Vec<&Repr> = match repr {
            Repr::Record { fields, .. } => fields.iter().map(|(_, r)| r).collect(),
            Repr::Tuple(elems) => elems.iter().collect(),
            Repr::Union(variants) => variants.iter().collect(),
            _ => vec![],
        };
        for d in deps {
            // Resolve a back-edge first: a field typed `Node | null` arrives as a
            // `Recursive`, and skipping it left the union it names undefined at the point
            // the struct used it. A *boxed* back-edge is only a pointer, so the forward
            // typedef is enough and it needs no definition here.
            let d = self.resolve(d);
            if matches!(d, Repr::Record { .. } | Repr::Tuple(_) | Repr::Union(_)) {
                if let Some(dname) = self.names.get(&key_with(d, &self.recursive)).cloned() {
                    self.emit_one(out, &dname, d, done);
                }
            }
        }
        match repr {
            // The heap wrapper for a by-value cycle. The header is first, so the pointer
            // doubles as a `neon_header*` and refcounting needs no offset.
            // Skipping on a miss emitted the forward `typedef` and no definition, so every
            // use of the wrapper became an incomplete type and `cc` reported the *uses*
            // rather than the missing layout. `register` puts the name and the shape into
            // their tables together, so a miss means those two have drifted.
            Repr::BoxedRec(atom) => {
                let Some(shape) = self.boxed.get(atom) else {
                    ice(repr, "a boxed record whose pointee layout was never registered")
                };
                if let Some(sname) = self.names.get(&key_with(shape, &self.recursive)) {
                    self.emit_one(out, &sname.clone(), shape, done);
                }
                let _ = writeln!(out, "struct {name} {{");
                let _ = writeln!(out, "    neon_header header;");
                let _ = writeln!(out, "    {} value;", self.c_type(shape));
                let _ = writeln!(out, "}};");
            }
            Repr::Record { name: nominal, fields } => {
                if let Some(n) = nominal {
                    let _ = writeln!(out, "// {n}");
                }
                let _ = writeln!(out, "struct {name} {{");
                // An empty aggregate still needs a member: a zero-size struct is not
                // portable C.
                if fields.is_empty() {
                    let _ = writeln!(out, "    char _empty;");
                }
                for (fname, fr) in fields {
                    let _ = writeln!(out, "    {} {};", self.c_type(fr), field_name(fname));
                }
                let _ = writeln!(out, "}};");
            }
            Repr::Tuple(elems) => {
                let _ = writeln!(out, "struct {name} {{");
                if elems.is_empty() {
                    let _ = writeln!(out, "    char _empty;");
                }
                for (i, e) in elems.iter().enumerate() {
                    let _ = writeln!(out, "    {} _{i};", self.c_type(e));
                }
                let _ = writeln!(out, "}};");
            }
            // A union is a discriminant plus a payload big enough for any variant. The tag
            // is the variant's index in the repr, so inject/project/`is` all agree.
            Repr::Union(variants) => {
                let _ = writeln!(out, "struct {name} {{");
                let _ = writeln!(out, "    uint32_t tag;");
                let _ = writeln!(out, "    union {{");
                for (i, v) in variants.iter().enumerate() {
                    let _ = writeln!(out, "        {} _{i};", self.c_type(v));
                }
                let _ = writeln!(out, "    }} u;");
                let _ = writeln!(out, "}};");
            }
            _ => {}
        }
    }
}

/// Whether a repr is a concrete value that can be boxed into `any`.
fn is_boxable(r: &Repr) -> bool {
    !matches!(r, Repr::Any | Repr::Never | Repr::Var(_) | Repr::Recursive(_))
}

/// FNV-1a, the same 64-bit hash the atom tags use.
pub fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// A record field name, escaped so it is always a valid C identifier and never collides
/// with the tuple element scheme (`_0`) or a C keyword.
///
/// INJECTIVITY OBLIGATION: this is an identity — it names a C struct member, and two
/// fields of one record colliding here would be a duplicate member or a silent alias.
///
/// The domain is two disjoint sets. USER field names are identifiers, and the lexer
/// admits only `[A-Za-z_][A-Za-z0-9_]*` (`lexer/mod.rs::is_ident_start`), every
/// character of which passes through verbatim. SYNTHESIZED labels are `#`-prefixed —
/// `#inner` for a newtype's hidden field, `#nominal` for a record's identity tag, `#0`…
/// for tuple elements — and `#` is not an identifier character, so source can never
/// write one. The prefix (`f_` vs `fh_`) records which set the name came from, and
/// within each set the spelling is the identity, so the whole thing is injective.
///
/// The prefixes cannot collide either: a user name yields `f_` then the name, a
/// synthesized one `fh_` then the name, and those differ at position 1 (`_` vs `h`) for
/// every input, including a user field that itself begins with `h`.
///
/// This replaced a `write!(out, "x{:02x}", ..)` escape that was NOT injective — it was
/// undelimited, so a field named `a-b` and a field literally named `ax2db` both spelled
/// `f_ax2db`. The doc comment claimed that arm was unreachable; a `debug_assert` put
/// there to back the claim failed on the first test run, on `#inner`. Which is the
/// lesson: the assertion below is load-bearing, not decoration, and it is what will
/// catch the next label scheme that does not fit either set.
pub fn field_name(name: &str) -> String {
    let synthesized = name.starts_with('#');
    let mut out = String::from(if synthesized { "fh_" } else { "f_" });
    for c in name.chars().skip(usize::from(synthesized)) {
        debug_assert!(
            c.is_ascii_alphanumeric() || c == '_',
            "field_name got {c:?} in {name:?}, which is neither an identifier nor a \
             `#`-prefixed synthesized label. Both prefix namespaces spell the rest of \
             the name verbatim, so a character outside `[A-Za-z0-9_]` has no injective \
             spelling here. Give this a delimited escape before admitting such names."
        );
        out.push(c);
    }
    out
}

/// A canonical structural key: two reprs share a C struct iff their keys match.
///
/// INJECTIVITY OBLIGATION: this is an identity — it interns C structs, value-witnesses
/// and key-witnesses, so two reprs sharing a key get one struct and one witness. It is
/// injective over `Repr` because every arm is either a distinct one-character constant
/// or a distinct one-character constructor prefix followed by a bracketed, comma-joined
/// spelling of every field of that variant; nothing is elided. The separators are safe
/// because the only free strings are identifiers, and the lexer admits only
/// `[A-Za-z_][A-Za-z0-9_]*` (`lexer/mod.rs::is_ident_start`/`is_ident_continue`), so no
/// name can contain `[`, `]`, `,`, `=` or `@` and forge a bracket structure.
///
/// The one caveat, and it is deliberate: `Recursive(ty)` and any repr that unfolds to a
/// type in `rec` both key as `Z<ty>`. That is a COLLAPSE, and the right one — a
/// recursive type has two faithful spellings and they must share a struct. It means the
/// key is injective over *types*, not over *reprs*.
fn key(r: &Repr) -> String {
    key_with(r, &HashMap::new())
}

/// The same key, but resolving recursive types by identity.
///
/// A recursive type has two faithful spellings — the back-edge `Recursive(ty)` and the
/// unfolding it names — and which one arrives depends on whether the type was reached
/// below its own root or at a monomorphisation root. Keying both as `Z<ty>` is what makes
/// them one C struct instead of two the compiler then refuses to assign between.
fn key_with(r: &Repr, rec: &HashMap<TyId, Repr>) -> String {
    if let Repr::Recursive(ty) = r {
        return format!("Z{}", ty.0);
    }
    // Smallest id wins, so distinct types that unfold identically still agree on a key.
    if let Some(ty) =
        rec.iter().filter(|(_, u)| *u == r).map(|(t, _)| *t).min_by_key(|t| t.0)
    {
        return format!("Z{}", ty.0);
    }
    let key = |x: &Repr| key_with(x, rec);
    match r {
        Repr::I64 => "i".into(),
        Repr::F64 => "f".into(),
        Repr::Bool => "b".into(),
        Repr::Str => "s".into(),
        Repr::Null => "n".into(),
        Repr::Unit => "u".into(),
        Repr::Tag => "t".into(),
        // BOTH halves are in the key, and that is the point. This key is the interning
        // identity for structs, value-witnesses and key-witnesses: two reprs sharing it get
        // one C struct and one witness. Keying by the C symbol alone merged any two Neon
        // types that named the same symbol into a single witness — the exact collision
        // `ice` in this file exists to catch, arriving silently instead. Keying by the
        // nominal alone would be safe today only because `c_type` is a function of
        // `nominal`; spelling both keeps the key honest if that ever stops holding, and
        // costs nothing since the key never leaves this table.
        Repr::Runtime { nominal, c_type, args } if args.is_empty() => format!("N{nominal}@{c_type}"),
        Repr::Runtime { nominal, c_type, args } => {
            format!(
                "N{nominal}@{c_type}[{}]",
                args.iter().map(key).collect::<Vec<_>>().join(",")
            )
        }
        Repr::Any => "a".into(),
        Repr::Never => "x".into(),
        Repr::Var(v) => format!("V{v}"),
        Repr::Recursive(ty) => format!("Z{}", ty.0),
        Repr::BoxedRec(a) => format!("P{a}"),
        Repr::Record { name, fields } => {
            let body: Vec<String> =
                fields.iter().map(|(n, r)| format!("{n}={}", key(r))).collect();
            // CORRECT DEFAULT: an anonymous record has no name, and the empty string is
            // its name in the key. Two records share a C struct iff their keys match, and
            // a structural record must not collide with a nominal one that happens to have
            // the same fields — `R[..]` and `RUser[..]` differ, which is the whole point.
            format!("R{}[{}]", name.as_deref().unwrap_or(""), body.join(","))
        }
        Repr::Tuple(elems) => {
            format!("T[{}]", elems.iter().map(key).collect::<Vec<_>>().join(","))
        }
        Repr::List(e) => format!("L[{}]", key(e)),
        Repr::Map(k, v) => format!("M[{},{}]", key(k), key(v)),
        Repr::Closure { params, throws, ret } => format!(
            "C[{}!{}=>{}]",
            params.iter().map(key).collect::<Vec<_>>().join(","),
            key(throws),
            key(ret)
        ),
        Repr::Union(vs) => format!("U[{}]", vs.iter().map(key).collect::<Vec<_>>().join(",")),
        Repr::Nullable(i) => format!("N[{}]", key(i)),
    }
}
