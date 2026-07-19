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
    pub fn key_witness_ref(&self, r: &Repr) -> String {
        match self.key_witness_names.get(&key_with(r, &self.recursive)) {
            Some(n) => format!("&{n}"),
            None => "0".into(),
        }
    }

    /// Every key-witness the program needs, as `(name, key repr)`.
    pub fn key_witnesses(&self) -> &[(String, Repr)] {
        &self.key_witness_defs
    }

    /// The address-of expression for an element type's value-witness.
    pub fn witness_ref(&self, r: &Repr) -> String {
        match self.witness_names.get(&key_with(r, &self.recursive)) {
            Some(n) => format!("&{n}"),
            None => "0".into(),
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

    /// The env-drop function name for a closure environment, or `0` when it has no
    /// captured references to release (an empty environment).
    pub fn env_drop_ref(&self, r: &Repr) -> String {
        self.env_drop_names.get(&key_with(r, &self.recursive)).cloned().unwrap_or_else(|| "0".into())
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
            Repr::Runtime { name, .. } => format!("{name}*"),
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
            Repr::BoxedRec(atom) => {
                if let Some(shape) = self.boxed.get(atom) {
                    if let Some(sname) = self.names.get(&key_with(shape, &self.recursive)) {
                        self.emit_one(out, &sname.clone(), shape, done);
                    }
                    let _ = writeln!(out, "struct {name} {{");
                    let _ = writeln!(out, "    neon_header header;");
                    let _ = writeln!(out, "    {} value;", self.c_type(shape));
                    let _ = writeln!(out, "}};");
                }
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

/// The name a boxed value's type tag is derived from. It has to agree with what an
/// `is Name` test asks for, so a nominal record uses its own name and a primitive its
/// spelling in the language.
pub fn type_tag_name(r: &Repr) -> String {
    match r {
        Repr::I64 => "i64".into(),
        Repr::F64 => "f64".into(),
        Repr::Bool => "bool".into(),
        Repr::Str => "str".into(),
        Repr::Null => "null".into(),
        Repr::Unit => "unit".into(),
        Repr::Tag => "atom".into(),
        Repr::List(_) => "List".into(),
        Repr::Map(_, _) => "Map".into(),
        Repr::Closure { .. } => "fn".into(),
        Repr::Record { name: Some(n), .. } => n.clone(),
        Repr::Nullable(inner) => type_tag_name(inner),
        // Anonymous shapes have no name to test against; their structure is the identity.
        other => key(other),
    }
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

/// The type tag stored in a box for a given repr.
pub fn type_tag(r: &Repr) -> u64 {
    fnv1a(&type_tag_name(r))
}

/// A record field name, escaped so it is always a valid C identifier and never collides
/// with the tuple element scheme (`_0`) or a C keyword.
pub fn field_name(name: &str) -> String {
    let mut out = String::from("f_");
    for c in name.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' => out.push(c),
            other => {
                let _ = write!(out, "x{:02x}", other as u32);
            }
        }
    }
    out
}

/// A canonical structural key: two reprs share a C struct iff their keys match.
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
        Repr::Runtime { name, args } if args.is_empty() => format!("N{name}"),
        Repr::Runtime { name, args } => {
            format!("N{name}[{}]", args.iter().map(key).collect::<Vec<_>>().join(","))
        }
        Repr::Any => "a".into(),
        Repr::Never => "x".into(),
        Repr::Var(v) => format!("V{v}"),
        Repr::Recursive(ty) => format!("Z{}", ty.0),
        Repr::BoxedRec(a) => format!("P{a}"),
        Repr::Record { name, fields } => {
            let body: Vec<String> =
                fields.iter().map(|(n, r)| format!("{n}={}", key(r))).collect();
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
