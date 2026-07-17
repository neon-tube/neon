//! The declaration environment: every named type, protocol and impl in a module
//! tree.
//!
//! Three passes. Declare every name, so declaration order never matters and
//! mutual reference works; check μ-contractivity, which is pure syntax and must
//! settle before anything can instantiate a μ; then resolve the bodies.

use super::empty::Solver;
use super::resolve::{self, Scope, ScopeVar};
use super::types::TyId;
use crate::ast;
use crate::lexer::Span;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

/// Bounds the two places a type expression can expand without bound: an
/// instantiation chain (`record R[T] { r: R[Box[T]] }`) and the contractivity
/// walk that follows it. A cap turns a pathological declaration into a
/// diagnostic instead of a hang.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct TypeError {
    pub span: Span,
    pub kind: TypeErrorKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeErrorKind {
    Unknown(String),
    UnknownProtocol(String),
    Duplicate(String),
    Arity { name: String, expected: usize, found: usize },
    DuplicateField(String),
    /// A plain `type` that names itself. Recursion is declared, not inferred.
    RecursiveAlias(String),
    /// `newtype T = List[T]`. Recursion is `mu type`'s job.
    RecursiveNewtype(String),
    MuWithoutRecursion(String),
    MuMutual { name: String, other: String },
    MuUnguarded(String),
    MuUnderNegation(String),
    MuInParameter(String),
    TooDeep(String),
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            TypeErrorKind::Unknown(n) => write!(f, "unknown type `{n}`"),
            TypeErrorKind::UnknownProtocol(n) => write!(f, "unknown protocol `{n}`"),
            TypeErrorKind::Duplicate(n) => write!(f, "`{n}` is already declared in this module"),
            TypeErrorKind::Arity { name, expected, found } => write!(
                f,
                "`{name}` takes {expected} type argument(s), but {found} were given"
            ),
            TypeErrorKind::DuplicateField(n) => write!(f, "duplicate field `{n}`"),
            TypeErrorKind::RecursiveAlias(n) => write!(
                f,
                "`type {n}` is recursive: a plain alias may not name itself. \
                 Write `mu type {n}` if the recursion is intended"
            ),
            TypeErrorKind::RecursiveNewtype(n) => write!(
                f,
                "`newtype {n}` may not be recursive: a newtype is a nominal wrapper. \
                 Use `mu type` for a recursive alias, or `record {n}` for a recursive \
                 nominal type"
            ),
            TypeErrorKind::MuWithoutRecursion(n) => write!(
                f,
                "`mu type {n}` never names itself: the binder asserts recursion. \
                 Write `type {n}` instead"
            ),
            TypeErrorKind::MuMutual { name, other } => write!(
                f,
                "mutual recursion between `{name}` and `{other}` is not supported: \
                 a `mu type` binds itself. Mutually recursive records work"
            ),
            TypeErrorKind::MuUnguarded(n) => write!(
                f,
                "the recursive occurrence of `{n}` is not guarded: it must sit beneath \
                 a type constructor — a generic argument, a record field, a tuple element \
                 — or unfolding `{n}` never makes progress"
            ),
            TypeErrorKind::MuUnderNegation(n) => write!(
                f,
                "the recursive occurrence of `{n}` sits beneath a negation, which has \
                 no fixed point"
            ),
            TypeErrorKind::MuInParameter(n) => write!(
                f,
                "the recursive occurrence of `{n}` sits in a function parameter, which \
                 is contravariant"
            ),
            TypeErrorKind::TooDeep(n) => write!(
                f,
                "`{n}` expands without bound: polymorphic recursion is not supported"
            ),
        }
    }
}

// ---- what dispatch.rs consumes ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProtocolId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImplId(pub usize);

#[derive(Debug, Clone)]
pub struct FnSig {
    pub name: String,
    pub module: Vec<String>,
    pub generics: Vec<String>,
    pub params: Vec<(String, TyId)>,
    pub ret: TyId,
    /// `throws E`, written before `->`. No clause is `never`.
    pub throws: TyId,
    /// `where T: Display` — the protocol path, not a type.
    pub wheres: Vec<(String, Vec<String>)>,
    /// The signature as an arrow, for a value-position use of the name.
    pub ty: TyId,
    /// `false` for a protocol's required method.
    pub has_body: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Protocol {
    pub name: String,
    pub module: Vec<String>,
    /// `protocol P for T` — the name `T` is bound in every method signature.
    pub subject: String,
    /// Non-zero for a constructor subject, `for C[_]`.
    pub subject_arity: usize,
    pub methods: Vec<FnSig>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ImplDef {
    pub protocol: ProtocolId,
    pub module: Vec<String>,
    pub generics: Vec<String>,
    /// `None` when the target is a bare constructor — `impl Container for Box`,
    /// which names the constructor and not a type.
    pub target: Option<TyId>,
    /// Set instead of `target` for a constructor target.
    pub target_head: Option<String>,
    pub methods: Vec<FnSig>,
    pub span: Span,
}

// ---- declarations ----

#[derive(Debug, Clone)]
enum Sort {
    Record(ast::RecordDecl),
    Alias(ast::AliasDecl),
    Mu(ast::AliasDecl),
    Newtype(ast::AliasDecl),
}

#[derive(Debug, Clone)]
struct TypeDecl {
    module: Vec<String>,
    sort: Sort,
    span: Span,
}

impl TypeDecl {
    fn generics(&self) -> &[String] {
        match &self.sort {
            Sort::Record(r) => &r.generics,
            Sort::Alias(a) | Sort::Mu(a) | Sort::Newtype(a) => &a.generics,
        }
    }
    fn name(&self) -> &str {
        match &self.sort {
            Sort::Record(r) => &r.name,
            Sort::Alias(a) | Sort::Mu(a) | Sort::Newtype(a) => &a.name,
        }
    }
}

pub struct Env {
    pub solver: Solver,
    decls: HashMap<String, TypeDecl>,
    /// Module key -> (bound name, full path).
    uses: HashMap<String, Vec<(String, String)>>,
    protocols: Vec<Protocol>,
    protocol_ids: HashMap<String, ProtocolId>,
    impls: Vec<ImplDef>,
    fns: Vec<FnSig>,
    errors: Vec<TypeError>,

    inst: HashMap<(String, Vec<TyId>), TyId>,
    /// Alias and newtype expansions in progress. Those two inline, so a cycle
    /// through one does not terminate and has to be caught here; a record or a
    /// `mu` is reserved before its body is read, so a cycle through one is just
    /// a cycle in the graph.
    active: Vec<(String, Vec<TyId>)>,
    /// `mu` declarations whose contractivity check failed.
    mu_bad: Vec<String>,
    depth: usize,
    error_ty: TyId,
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

impl Env {
    pub fn new() -> Self {
        let mut solver = Solver::new();
        // `#` is not an identifier character, so no source can name this.
        let n = solver.t.name("#error");
        let error_ty = solver.t.var(n);
        Env {
            solver,
            decls: HashMap::new(),
            uses: HashMap::new(),
            protocols: vec![],
            protocol_ids: HashMap::new(),
            impls: vec![],
            fns: vec![],
            errors: vec![],
            inst: HashMap::new(),
            active: vec![],
            mu_bad: vec![],
            depth: 0,
            error_ty,
        }
    }

    pub fn build(module: &ast::Module) -> Self {
        let mut env = Env::new();
        env.declare(&[], &module.decls);
        env.check_contractivity();
        env.resolve_bodies(&[], &module.decls);
        env
    }

    pub fn errors(&self) -> &[TypeError] {
        &self.errors
    }

    pub fn protocols(&self) -> &[Protocol] {
        &self.protocols
    }

    pub fn protocol(&self, id: ProtocolId) -> &Protocol {
        &self.protocols[id.0]
    }

    pub fn impls(&self) -> &[ImplDef] {
        &self.impls
    }

    pub fn impls_of(&self, p: ProtocolId) -> impl Iterator<Item = (ImplId, &ImplDef)> {
        self.impls
            .iter()
            .enumerate()
            .filter(move |(_, i)| i.protocol == p)
            .map(|(n, i)| (ImplId(n), i))
    }

    /// Every protocol declaring a method of this name — the candidate set
    /// dispatch starts from, and where two protocols answering the same call
    /// becomes visible.
    pub fn protocols_with_method(&self, name: &str) -> Vec<ProtocolId> {
        self.protocols
            .iter()
            .enumerate()
            .filter(|(_, p)| p.methods.iter().any(|m| m.name == name))
            .map(|(i, _)| ProtocolId(i))
            .collect()
    }

    pub fn fns(&self) -> &[FnSig] {
        &self.fns
    }

    /// The poison. Recovery only: it is produced where a diagnostic has already
    /// been emitted, and it propagates to the top of a type expression so that
    /// one bad name costs one diagnostic.
    ///
    /// It is a rigid variable under a name source cannot write, so it is
    /// disjoint from every type a program can name — `error <: T` and `T <: error`
    /// are both false. It is not outside the lattice, so `error <: any` still
    /// holds; the force of the poison is `is_error`, which callers check before
    /// they complain.
    pub fn error_ty(&self) -> TyId {
        self.error_ty
    }

    pub fn is_error(&self, t: TyId) -> bool {
        t == self.error_ty
    }

    pub fn error(&mut self, span: Span, kind: TypeErrorKind) {
        self.errors.push(TypeError { span, kind });
    }

    pub fn resolve(&mut self, scope: &Scope, spec: &ast::TypeSpec) -> TyId {
        resolve::resolve(self, scope, spec)
    }

    // ---- pass 1: declare ----

    fn declare(&mut self, module: &[String], decls: &[ast::Decl]) {
        for d in decls {
            match &d.kind {
                ast::DeclKind::Record(r) => {
                    self.declare_type(module, d.span.clone(), Sort::Record(r.clone()))
                }
                ast::DeclKind::TypeAlias(a) => {
                    self.declare_type(module, d.span.clone(), Sort::Alias(a.clone()))
                }
                ast::DeclKind::MuType(a) => {
                    self.declare_type(module, d.span.clone(), Sort::Mu(a.clone()))
                }
                ast::DeclKind::Newtype(a) => {
                    self.declare_type(module, d.span.clone(), Sort::Newtype(a.clone()))
                }
                ast::DeclKind::Protocol(p) => {
                    let key = qualify(module, &p.name);
                    let id = ProtocolId(self.protocols.len());
                    if self.protocol_ids.insert(key, id).is_some() {
                        self.error(d.span.clone(), TypeErrorKind::Duplicate(p.name.clone()));
                    }
                    self.protocols.push(Protocol {
                        name: p.name.clone(),
                        module: module.to_vec(),
                        subject: p.subject.clone(),
                        subject_arity: p.subject_arity,
                        methods: vec![],
                        span: d.span.clone(),
                    });
                }
                ast::DeclKind::Use(u) => {
                    if let Some(last) = u.path.last() {
                        self.uses
                            .entry(module.join("::"))
                            .or_default()
                            .push((last.clone(), u.path.join("::")));
                    }
                }
                ast::DeclKind::Mod(m) => {
                    let mut inner = module.to_vec();
                    inner.push(m.name.clone());
                    self.declare(&inner, &m.decls);
                }
                _ => {}
            }
        }
    }

    fn declare_type(&mut self, module: &[String], span: Span, sort: Sort) {
        let d = TypeDecl { module: module.to_vec(), sort, span: span.clone() };
        let key = qualify(module, d.name());
        let name = d.name().to_string();
        if self.decls.insert(key, d).is_some() {
            self.error(span, TypeErrorKind::Duplicate(name));
        }
    }

    // ---- pass 2: contractivity ----

    fn check_contractivity(&mut self) {
        let mus: Vec<String> = self
            .decls
            .iter()
            .filter(|(_, d)| matches!(d.sort, Sort::Mu(_)))
            .map(|(k, _)| k.clone())
            .collect();
        for key in mus {
            let (errors, found) = contractivity(self, &key);
            let bad = !errors.is_empty();
            self.errors.extend(errors);
            let d = &self.decls[&key];
            if !found {
                let (span, name) = (d.span.clone(), d.name().to_string());
                self.error(span, TypeErrorKind::MuWithoutRecursion(name));
            }
            if bad || !found {
                self.mu_bad.push(key);
            }
        }
    }

    // ---- pass 3: bodies ----

    fn resolve_bodies(&mut self, module: &[String], decls: &[ast::Decl]) {
        for d in decls {
            match &d.kind {
                ast::DeclKind::Record(_)
                | ast::DeclKind::TypeAlias(_)
                | ast::DeclKind::MuType(_)
                | ast::DeclKind::Newtype(_) => {
                    // Force the canonical instantiation — generics rigid — so a
                    // declaration's own errors are reported once, at the
                    // declaration, rather than at every use.
                    let key = qualify(module, decl_name(&d.kind));
                    let Some(decl) = self.decls.get(&key) else { continue };
                    let generics = decl.generics().to_vec();
                    let scope = Scope::new(module).with_rigid(self, &generics);
                    let args: Vec<TyId> = scope.vars.iter().map(|v| v.ty).collect();
                    self.instantiate(&key, args, &d.span);
                }
                ast::DeclKind::Fn(f) => {
                    let sig = self.fn_sig(module, f, &[], &d.span);
                    self.fns.push(sig);
                }
                ast::DeclKind::Protocol(p) => {
                    let key = qualify(module, &p.name);
                    let Some(&id) = self.protocol_ids.get(&key) else { continue };
                    let subject = ScopeVar {
                        name: p.subject.clone(),
                        ty: {
                            let n = self.solver.t.name(&p.subject);
                            self.solver.t.var(n)
                        },
                        arity: p.subject_arity,
                    };
                    let methods = p
                        .methods
                        .iter()
                        .map(|m| self.fn_sig(module, m, std::slice::from_ref(&subject), &d.span))
                        .collect();
                    self.protocols[id.0].methods = methods;
                }
                ast::DeclKind::Impl(i) => self.impl_def(module, i, &d.span),
                ast::DeclKind::Mod(m) => {
                    let mut inner = module.to_vec();
                    inner.push(m.name.clone());
                    self.resolve_bodies(&inner, &m.decls);
                }
                _ => {}
            }
        }
    }

    fn fn_sig(
        &mut self,
        module: &[String],
        f: &ast::FnDecl,
        extra: &[ScopeVar],
        span: &Span,
    ) -> FnSig {
        let mut scope = Scope::new(module);
        scope.vars.extend_from_slice(extra);
        let scope = scope.with_rigid(self, &f.generics);

        let params: Vec<(String, TyId)> = f
            .params
            .iter()
            .map(|p| (p.name.clone(), self.resolve(&scope, &p.ty)))
            .collect();
        let ret = match &f.ret {
            Some(r) => self.resolve(&scope, r),
            // No return type is `()`, and `()` is the empty tuple.
            None => self.solver.t.tuple(vec![]),
        };
        let throws = match &f.throws {
            Some(t) => self.resolve(&scope, t),
            None => self.solver.t.never(),
        };
        let wheres = f.wheres.iter().filter_map(|w| bound_path(w).map(|p| (w.param.clone(), p))).collect();
        let ty = {
            let ps = params.iter().map(|p| p.1).collect();
            self.solver.t.arrow(ps, throws, ret)
        };
        FnSig {
            name: f.name.clone(),
            module: module.to_vec(),
            generics: f.generics.clone(),
            params,
            ret,
            throws,
            wheres,
            ty,
            has_body: f.body.is_some(),
            span: span.clone(),
        }
    }

    fn impl_def(&mut self, module: &[String], i: &ast::ImplDecl, span: &Span) {
        let Some(protocol) = self.lookup_protocol(module, &i.protocol) else {
            self.error(span.clone(), TypeErrorKind::UnknownProtocol(i.protocol.join("::")));
            return;
        };
        let scope = Scope::new(module).with_rigid(self, &i.generics);

        // `impl Container for Box` names the constructor, not a type, so it has no
        // arguments to resolve and no arity to check.
        let head = match &i.target.kind {
            ast::TypeSpecKind::Named { path, args } if args.is_empty() => self
                .lookup(module, path)
                .filter(|k| !self.decls[k].generics().is_empty())
                .map(|_| path.join("::")),
            _ => None,
        };
        let target = match head {
            Some(_) => None,
            None => Some(self.resolve(&scope, &i.target)),
        };

        let subject = ScopeVar {
            name: self.protocols[protocol.0].subject.clone(),
            ty: target.unwrap_or(self.error_ty),
            arity: self.protocols[protocol.0].subject_arity,
        };
        let methods = i
            .methods
            .iter()
            .map(|m| self.fn_sig(module, m, std::slice::from_ref(&subject), span))
            .collect();

        self.impls.push(ImplDef {
            protocol,
            module: module.to_vec(),
            generics: i.generics.clone(),
            target,
            target_head: head,
            methods,
            span: span.clone(),
        });
    }

    // ---- name lookup ----

    /// `path` as seen from `module`: an inner module's names shadow an outer's,
    /// a `use` binds its last segment, and a fully qualified path always works.
    pub fn lookup(&self, module: &[String], path: &[String]) -> Option<String> {
        let joined = path.join("::");
        for n in (0..=module.len()).rev() {
            let m = module[..n].join("::");
            if let ([only], Some(us)) = (path, self.uses.get(&m)) {
                if let Some((_, full)) = us.iter().find(|(bound, _)| bound == only) {
                    if self.decls.contains_key(full) {
                        return Some(full.clone());
                    }
                }
            }
            let cand = if m.is_empty() { joined.clone() } else { format!("{m}::{joined}") };
            if self.decls.contains_key(&cand) {
                return Some(cand);
            }
        }
        None
    }

    fn lookup_protocol(&self, module: &[String], path: &[String]) -> Option<ProtocolId> {
        let joined = path.join("::");
        for n in (0..=module.len()).rev() {
            let m = module[..n].join("::");
            if let ([only], Some(us)) = (path, self.uses.get(&m)) {
                if let Some((_, full)) = us.iter().find(|(bound, _)| bound == only) {
                    if let Some(&id) = self.protocol_ids.get(full) {
                        return Some(id);
                    }
                }
            }
            let cand = if m.is_empty() { joined.clone() } else { format!("{m}::{joined}") };
            if let Some(&id) = self.protocol_ids.get(&cand) {
                return Some(id);
            }
        }
        None
    }

    // ---- instantiation ----

    pub fn instantiate(&mut self, key: &str, args: Vec<TyId>, span: &Span) -> TyId {
        let Some(decl) = self.decls.get(key) else {
            self.error(span.clone(), TypeErrorKind::Unknown(key.to_string()));
            return self.error_ty;
        };
        let decl = decl.clone();
        let name = decl.name().to_string();

        if decl.generics().len() != args.len() {
            self.error(
                span.clone(),
                TypeErrorKind::Arity {
                    name,
                    expected: decl.generics().len(),
                    found: args.len(),
                },
            );
            return self.error_ty;
        }
        if self.mu_bad.iter().any(|k| k == key) {
            return self.error_ty;
        }

        let ik = (key.to_string(), args.clone());
        if let Some(&t) = self.inst.get(&ik) {
            return t;
        }
        if self.active.contains(&ik) {
            let kind = match decl.sort {
                Sort::Newtype(_) => TypeErrorKind::RecursiveNewtype(name),
                _ => TypeErrorKind::RecursiveAlias(name),
            };
            self.error(decl.span.clone(), kind);
            return self.error_ty;
        }
        if self.depth >= MAX_DEPTH {
            self.error(span.clone(), TypeErrorKind::TooDeep(name));
            return self.error_ty;
        }

        let scope = self.bind(&decl, &args);
        self.depth += 1;
        let ty = match &decl.sort {
            Sort::Record(r) => {
                // Reserved before the fields are read, so `record Node { next: Node }`
                // is a cycle in the graph rather than an unbounded expansion.
                let id = self.solver.t.reserve();
                self.inst.insert(ik.clone(), id);
                let body = self.record_body(r, &scope, args.clone());
                let d = self.solver.t.data(body);
                self.solver.t.define(id, d);
                id
            }
            Sort::Mu(a) => {
                let id = self.solver.t.reserve();
                self.inst.insert(ik.clone(), id);
                let body = self.resolve(&scope, &a.value);
                let d = self.solver.t.data(body);
                self.solver.t.define(id, d);
                id
            }
            Sort::Alias(a) => {
                self.active.push(ik.clone());
                let t = self.resolve(&scope, &a.value);
                self.active.pop();
                t
            }
            Sort::Newtype(a) => {
                self.active.push(ik.clone());
                let inner = self.resolve(&scope, &a.value);
                self.active.pop();
                if self.is_error(inner) {
                    inner
                } else {
                    // A nominal wrapper and nothing else: one hidden field under a
                    // name source cannot write, so the newtype is disjoint from its
                    // representation and from its siblings.
                    let n = self.solver.t.name(decl.name());
                    let l = self.solver.t.name("#inner");
                    self.solver.t.nominal(n, args.clone(), vec![(l, inner)])
                }
            }
        };
        self.depth -= 1;
        self.inst.insert(ik, ty);
        ty
    }

    fn bind(&mut self, decl: &TypeDecl, args: &[TyId]) -> Scope {
        let mut scope = Scope::new(&decl.module);
        for (g, &a) in decl.generics().iter().zip(args) {
            scope.vars.push(ScopeVar { name: g.clone(), ty: a, arity: 0 });
        }
        scope
    }

    fn record_body(&mut self, r: &ast::RecordDecl, scope: &Scope, args: Vec<TyId>) -> TyId {
        let mut fields: Vec<(super::types::NameId, TyId)> = Vec::new();
        let mut poison = false;
        for f in &r.fields {
            let t = self.resolve(scope, &f.ty);
            poison |= self.is_error(t);
            let l = self.solver.t.name(&f.name);
            if fields.iter().any(|(seen, _)| *seen == l) {
                self.error(f.span.clone(), TypeErrorKind::DuplicateField(f.name.clone()));
                poison = true;
                continue;
            }
            fields.push((l, t));
        }
        if poison {
            return self.error_ty;
        }
        let n = self.solver.t.name(&r.name);
        self.solver.t.nominal(n, args, fields)
    }

    fn fields_visible(&self, decl_module: &[String], from: &[String], opaque: bool) -> bool {
        if !opaque || from == decl_module {
            return true;
        }
        // Module-scoped, not absolute: the declaring module and one parent.
        matches!(decl_module.split_last(), Some((_, parent)) if from == parent)
    }
}

fn decl_name(k: &ast::DeclKind) -> &str {
    match k {
        ast::DeclKind::Record(r) => &r.name,
        ast::DeclKind::TypeAlias(a) | ast::DeclKind::MuType(a) | ast::DeclKind::Newtype(a) => {
            &a.name
        }
        _ => "",
    }
}

fn qualify(module: &[String], name: &str) -> String {
    if module.is_empty() {
        name.to_string()
    } else {
        format!("{}::{name}", module.join("::"))
    }
}

/// A `where` bound names a protocol, not a type.
fn bound_path(w: &ast::WhereClause) -> Option<Vec<String>> {
    match &w.bound.kind {
        ast::TypeSpecKind::Named { path, args } if args.is_empty() => Some(path.clone()),
        _ => None,
    }
}

// ---- contractivity ----

/// A `TypeSpec` plus the module and generic bindings it is read under. A record's
/// field is written in the record's module, but the arguments substituted into it
/// come from the use site, so the two halves cannot share one context.
struct Ctx {
    module: Vec<String>,
    subst: HashMap<String, (ast::TypeSpec, Rc<Ctx>)>,
}

#[derive(Clone, Copy)]
struct Pos {
    /// Beneath a structural constructor: a generic argument, a field, a tuple
    /// element, an arrow's return. This is what makes unfolding make progress.
    guarded: bool,
    neg: bool,
    /// In a function parameter. An arrow is contravariant there, so it is the one
    /// constructor position that guards nothing.
    contra: bool,
}

struct Contract<'a> {
    env: &'a Env,
    /// The declaration key being checked. `mu` binds itself: an occurrence of any
    /// *other* `mu` on the way back is mutual recursion.
    key: &'a str,
    name: String,
    module: Vec<String>,
    foreign: Vec<String>,
    path: Vec<(String, Vec<ast::TypeSpec>)>,
    errors: Vec<TypeError>,
    found: bool,
}

fn contractivity(env: &Env, key: &str) -> (Vec<TypeError>, bool) {
    let decl = &env.decls[key];
    let Sort::Mu(a) = &decl.sort else { return (vec![], true) };
    let mut c = Contract {
        env,
        key,
        name: decl.name().to_string(),
        module: decl.module.clone(),
        foreign: vec![],
        path: vec![],
        errors: vec![],
        found: false,
    };
    let ctx = Rc::new(Ctx { module: decl.module.clone(), subst: HashMap::new() });
    c.walk(&a.value, &ctx, Pos { guarded: false, neg: false, contra: false });
    // One occurrence is reached twice — once as a generic argument, once through
    // the field that argument is substituted into — and it is one mistake.
    let mut seen: Vec<TypeError> = Vec::new();
    for e in c.errors {
        if !seen.contains(&e) {
            seen.push(e);
        }
    }
    (seen, c.found)
}

impl Contract<'_> {
    fn walk(&mut self, spec: &ast::TypeSpec, ctx: &Rc<Ctx>, pos: Pos) {
        let under = Pos { guarded: true, ..pos };
        match &spec.kind {
            ast::TypeSpecKind::Union(xs) | ast::TypeSpecKind::Intersect(xs) => {
                for x in xs {
                    self.walk(x, ctx, pos);
                }
            }
            ast::TypeSpecKind::Negate(x) => self.walk(x, ctx, Pos { neg: true, ..pos }),
            ast::TypeSpecKind::Tuple(xs) => {
                for x in xs {
                    self.walk(x, ctx, under);
                }
            }
            ast::TypeSpecKind::Struct(fs) => {
                for f in fs {
                    self.walk(&f.ty, ctx, under);
                }
            }
            ast::TypeSpecKind::Fn { params, throws, ret } => {
                // decisions.md rule 2: a parameter is contravariant and excluded, a
                // return is covariant and allowed. The arrow guards the return like
                // any other constructor — `ArrowAtom` holds it as a raw `TyId`, which
                // is exactly the path a boolean op never snapshots, so the cycle
                // closes there the same way it does through a field.
                let param = Pos { contra: true, ..under };
                for p in params {
                    self.walk(p, ctx, param);
                }
                // `throws` is covariant like the return, so it guards too.
                if let Some(t) = throws {
                    self.walk(t, ctx, under);
                }
                self.walk(ret, ctx, under);
            }
            ast::TypeSpecKind::Named { path, args } => self.named(spec, path, args, ctx, pos),
            _ => {}
        }
    }

    fn named(
        &mut self,
        spec: &ast::TypeSpec,
        path: &[String],
        args: &[ast::TypeSpec],
        ctx: &Rc<Ctx>,
        pos: Pos,
    ) {
        if let [only] = path {
            if let Some((s, c)) = ctx.subst.get(only) {
                let (s, c) = (s.clone(), c.clone());
                self.walk(&s, &c, pos);
                return;
            }
        }
        // A generic argument is a guard whatever the head is — it is visible in the
        // type expression, and an unresolvable head does not change that.
        for a in args {
            self.walk(a, ctx, Pos { guarded: true, ..pos });
        }
        let Some(key) = self.env.lookup(&ctx.module, path) else { return };

        if key == self.key {
            self.found = true;
            let kind = if let Some(other) = self.foreign.first() {
                TypeErrorKind::MuMutual { name: self.name.clone(), other: other.clone() }
            } else if pos.neg {
                TypeErrorKind::MuUnderNegation(self.name.clone())
            } else if pos.contra {
                TypeErrorKind::MuInParameter(self.name.clone())
            } else if !pos.guarded {
                TypeErrorKind::MuUnguarded(self.name.clone())
            } else {
                return;
            };
            self.errors.push(TypeError { span: spec.span.clone(), kind });
            return;
        }

        let step = (key.clone(), args.to_vec());
        if self.path.contains(&step) {
            return;
        }
        if self.path.len() >= MAX_DEPTH {
            self.found = true;
            self.errors.push(TypeError {
                span: spec.span.clone(),
                kind: TypeErrorKind::TooDeep(self.name.clone()),
            });
            return;
        }
        let decl = &self.env.decls[&key];
        let inner = Rc::new(Ctx {
            module: decl.module.clone(),
            subst: decl
                .generics()
                .iter()
                .cloned()
                .zip(args.iter().map(|a| (a.clone(), ctx.clone())))
                .collect(),
        });

        self.path.push(step);
        match &decl.sort {
            // Transparent: an alias is a name, not a type.
            Sort::Alias(a) => self.walk(&a.value, &inner, pos),
            Sort::Mu(a) => {
                self.foreign.push(decl.name().to_string());
                self.walk(&a.value, &inner, pos);
                self.foreign.pop();
            }
            // A data constructor with one field.
            Sort::Newtype(a) => self.walk(&a.value, &inner, Pos { guarded: true, ..pos }),
            Sort::Record(r) => {
                // Judged where the `mu` is declared: the same record is a data
                // constructor with a guardable field in its own module, and an
                // opaque atom with no position to guard outside it.
                if self.env.fields_visible(&decl.module, &self.module, r.opaque) {
                    for f in &r.fields {
                        self.walk(&f.ty, &inner, Pos { guarded: true, ..pos });
                    }
                }
            }
        }
        self.path.pop();
    }
}
